use async_trait::async_trait;
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};
use tracing::{error, info};

use crate::{
    config,
    error::UsbError,
    events::{ChangeRequest, ChangeResponse, UsbChangeHandler},
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UmsMode {
    CdRom,
    Disk,
}

/// Runtime state of the UMS VM LUN (for zenoh queries and upper-layer sync).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UmsVmState {
    pub mode: UmsMode,
    pub mounted: bool,
    pub mounted_path: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UmsVmEvent {
    SwitchMode(UmsMode),
    Mount(String),
    Unmount,
    GetState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UmsVmResponse {
    SwitchMode(Result<bool, UsbError>),
    Mount(Result<(), UsbError>),
    Unmount(Result<(), UsbError>),
    GetState(UmsVmState),
}

pub struct UmsController {
    tx: mpsc::Sender<(UmsVmEvent, oneshot::Sender<UmsVmResponse>)>,
}

impl UmsController {
    pub fn new(tx: mpsc::Sender<(UmsVmEvent, oneshot::Sender<UmsVmResponse>)>) -> Self {
        Self { tx }
    }

    /// return: true: apply usbgadget; false: not apply usbgadget
    pub async fn switch_mode(&self, mode: UmsMode) -> Result<bool, UsbError> {
        let response = self.send_event(UmsVmEvent::SwitchMode(mode)).await?;
        match response {
            UmsVmResponse::SwitchMode(result) => result,
            e => Err(UsbError::InvalidArgument(format!("invalid response: {:?}", e))),
        }
    }

    pub async fn mount(&self, image_path: &str) -> Result<(), UsbError> {
        let response = self.send_event(UmsVmEvent::Mount(image_path.to_owned())).await?;
        match response {
            UmsVmResponse::Mount(result) => result,
            e => Err(UsbError::InvalidArgument(format!("invalid response: {:?}", e))),
        }
    }

    pub async fn unmount(&self) -> Result<(), UsbError> {
        let response = self.send_event(UmsVmEvent::Unmount).await?;
        match response {
            UmsVmResponse::Unmount(result) => result,
            e => Err(UsbError::InvalidArgument(format!("invalid response: {:?}", e))),
        }
    }

    pub async fn get_state(&self) -> Result<UmsVmState, UsbError> {
        let response = self.send_event(UmsVmEvent::GetState).await?;
        match response {
            UmsVmResponse::GetState(state) => Ok(state),
            e => Err(UsbError::InvalidArgument(format!("invalid response: {:?}", e))),
        }
    }
}

impl UmsController {
    async fn send_event(&self, event: UmsVmEvent) -> Result<UmsVmResponse, UsbError> {
        let (tx, rx) = oneshot::channel::<UmsVmResponse>();
        if let Err(e) = self.tx.send((event, tx)).await {
            return Err(UsbError::InvalidArgument(format!("failed to send event: {:?}", e)));
        }

        match rx.await {
            Ok(response) => Ok(response),
            Err(e) => {
                Err(UsbError::InvalidArgument(format!("failed to receive response: {:?}", e)))
            }
        }
    }
}

pub struct UmsHandle {
    name: String,
    inquiry_string: String,
    enabled: AtomicBool,
    mode: Mutex<UmsMode>,
    file_path: Mutex<Option<String>>,
    lun_file_path: Mutex<Option<PathBuf>>,
    udc_updating: AtomicBool,
}

impl UmsHandle {
    pub fn new(
        mode: UmsMode,
        enabled: bool,
        rx: mpsc::Receiver<(UmsVmEvent, oneshot::Sender<UmsVmResponse>)>,
    ) -> Arc<Self> {
        let handle = Arc::new(Self {
            name: config::UMS_VM_INQUIRY_STRING.to_owned(),
            inquiry_string: config::UMS_VM_INQUIRY_STRING.to_owned(),
            mode: Mutex::new(mode),
            enabled: AtomicBool::new(enabled),
            file_path: Mutex::new(None),
            lun_file_path: Mutex::new(None),
            udc_updating: AtomicBool::new(false),
        });
        let _ = Self::loop_events(handle.clone(), rx);
        handle
    }

    pub async fn get_mode(&self) -> UmsMode {
        self.mode.lock().await.clone()
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub async fn is_mounted(&self) -> bool {
        self.file_path.lock().await.is_some()
    }

    pub async fn mounted_path(&self) -> Option<String> {
        self.file_path.lock().await.clone()
    }

    pub async fn collect_state(&self) -> UmsVmState {
        let mounted_path = self.file_path.lock().await.clone();
        UmsVmState {
            mode: self.mode.lock().await.clone(),
            mounted: mounted_path.is_some(),
            mounted_path,
            enabled: self.enabled.load(Ordering::Acquire),
        }
    }
}

impl UmsHandle {
    fn loop_events(
        handle: Arc<Self>,
        mut rx: mpsc::Receiver<(UmsVmEvent, oneshot::Sender<UmsVmResponse>)>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                info!("ums handle loop events");
                match rx.recv().await {
                    Some((event, tx)) => {
                        handle.handle_event(event, tx).await;
                    }

                    None => break,
                }
                info!("ums handle loop events end");
            }
        })
    }

    async fn handle_event(&self, event: UmsVmEvent, tx: oneshot::Sender<UmsVmResponse>) {
        match event {
            UmsVmEvent::SwitchMode(mode) => {
                let result = self.switch_mode(mode).await;
                if let Err(e) = tx.send(UmsVmResponse::SwitchMode(result)) {
                    error!("failed to send switch mode response: {:?}", e);
                }
            }
            UmsVmEvent::Mount(image_path) => {
                let result = self.mount_ums(image_path.as_str()).await;
                if let Err(e) = tx.send(UmsVmResponse::Mount(result)) {
                    error!("failed to send mount response: {:?}", e);
                }
            }
            UmsVmEvent::Unmount => {
                let result = self.unmount_ums().await;
                if let Err(e) = tx.send(UmsVmResponse::Unmount(result)) {
                    error!("failed to send unmount response: {:?}", e);
                }
            }
            UmsVmEvent::GetState => {
                let state = self.collect_state().await;
                if let Err(e) = tx.send(UmsVmResponse::GetState(state)) {
                    error!("failed to send get state response: {:?}", e);
                }
            }
        }
    }

    async fn sync_lun_file_path(&self) {
        let mut cache = self.lun_file_path.lock().await;
        if !self.enabled.load(Ordering::Acquire) {
            *cache = None;
            return;
        }
        match super::resolve_mass_storage_file(&self.inquiry_string).await {
            Ok(path) => *cache = Some(path),
            Err(_) => *cache = None,
        }
    }

    async fn get_lun_file_path(&self) -> Result<PathBuf, UsbError> {
        if let Some(path) = self.lun_file_path.lock().await.clone() {
            return Ok(path);
        }
        let path = super::resolve_mass_storage_file(&self.inquiry_string).await?;
        *self.lun_file_path.lock().await = Some(path.clone());
        Ok(path)
    }

    async fn switch_mode(&self, ums_mode: UmsMode) -> Result<bool, UsbError> {
        if !self.enabled.load(Ordering::Acquire) {
            return Err(UsbError::DeviceNotEnabled(self.name().to_owned()));
        }

        let file_path = self.file_path.lock().await;
        if file_path.is_some() {
            return Err(UsbError::ChangeRejected(format!("{} is busy", self.name())));
        }

        if self.udc_updating.load(Ordering::Acquire) {
            return Err(UsbError::ChangeRejected(format!("USBGadget is updating")));
        }

        let mut mode = self.mode.lock().await;

        if *mode == ums_mode {
            return Ok(false);
        }

        *mode = ums_mode;
        Ok(true)
    }

    async fn mount_ums(&self, image_path: &str) -> Result<(), UsbError> {
        if !self.enabled.load(Ordering::Acquire) {
            return Err(UsbError::DeviceNotEnabled(self.name().to_owned()));
        }

        if self.udc_updating.load(Ordering::Acquire) {
            return Err(UsbError::ChangeRejected(format!("USBGadget is updating")));
        }

        if !Path::new(image_path).exists() {
            return Err(UsbError::FileNotFound(image_path.to_owned()));
        }

        let device_path = self.get_lun_file_path().await?;
        if !device_path.exists() {
            return Err(UsbError::DeviceNotFound(device_path.display().to_string()));
        }

        let mut file_path = self.file_path.lock().await;

        if let Some(existing_path) = file_path.as_ref() {
            return if existing_path == image_path {
                Ok(())
            } else {
                Err(UsbError::ChangeRejected(format!("{} is already mounted", self.name())))
            };
        }

        super::write_file(&device_path, image_path).await?;
        *file_path = Some(image_path.to_owned());
        Ok(())
    }

    async fn unmount_ums(&self) -> Result<(), UsbError> {
        if !self.enabled.load(Ordering::Acquire) {
            return Ok(());
        }

        if self.udc_updating.load(Ordering::Acquire) {
            return Err(UsbError::ChangeRejected(format!("USBGadget is updating")));
        }

        let device_path = match self.get_lun_file_path().await {
            Ok(path) => path,
            Err(_) => {
                *self.file_path.lock().await = None;
                return Ok(());
            }
        };
        if !device_path.exists() {
            *self.file_path.lock().await = None;
            return Ok(());
        }

        let mut file_path = self.file_path.lock().await;

        super::write_file(&device_path, "\n").await?;
        *file_path = None;
        Ok(())
    }
}

#[async_trait]
impl UsbChangeHandler for UmsHandle {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    async fn on_change_request(&self, req: &ChangeRequest) -> ChangeResponse {
        match req {
            ChangeRequest::RequestChange => {
                ChangeResponse::Proceed
            }

            ChangeRequest::PrepareChange => {
                match self.unmount_ums().await {
                    Ok(_) => {
                        self.udc_updating.store(true, Ordering::Release);
                        ChangeResponse::Proceed
                    }
                    Err(e) => {
                        ChangeResponse::Reject(format!("failed to unmount: {:?}", e))
                    }
                }
            }

            ChangeRequest::ChangeCompleted | ChangeRequest::ChangeCanceled => {
                self.sync_lun_file_path().await;
                self.udc_updating.store(false, Ordering::Release);
                ChangeResponse::Proceed
            }
        }
    }
}
