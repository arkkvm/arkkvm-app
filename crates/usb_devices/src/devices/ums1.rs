use async_trait::async_trait;
use tokio::{sync::{Mutex, mpsc, oneshot}, task::JoinHandle};
use tracing::error;

use crate::{config, error::UsbError, events::{ChangeRequest, ChangeResponse, UsbChangeHandler}};
use std::{path::{Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, Ordering}}};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UmsFtEvent {
    Mount(String),
    Unmount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UmsFtResponse {
    Mount(Result<(), UsbError>),
    Unmount(Result<(), UsbError>),
}

pub struct Ums1Controller {
    tx: mpsc::Sender<(UmsFtEvent, oneshot::Sender<UmsFtResponse>)>,
}

impl Ums1Controller {
    pub fn new(tx: mpsc::Sender<(UmsFtEvent, oneshot::Sender<UmsFtResponse>)>) -> Self {
        Self { tx }
    }

    pub async fn mount(&self, image_path: &str) -> Result<(), UsbError> {
        let response = self.send_event(UmsFtEvent::Mount(image_path.to_owned())).await?;
        match response {
            UmsFtResponse::Mount(result) => result,
            e => Err(UsbError::InvalidArgument(format!("invalid response: {:?}", e))),
        }
    }

    pub async fn unmount(&self) -> Result<(), UsbError> {
        let response = self.send_event(UmsFtEvent::Unmount).await?;
        match response {
            UmsFtResponse::Unmount(result) => result,
            e => Err(UsbError::InvalidArgument(format!("invalid response: {:?}", e))),
        }
    }
}

impl Ums1Controller {
    async fn send_event(&self, event: UmsFtEvent) -> Result<UmsFtResponse, UsbError> {
        let (tx, rx) = oneshot::channel::<UmsFtResponse>();
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

pub struct Ums1Handle {
    name: String,
    inquiry_string: String,
    enabled: AtomicBool,
    file_path: Mutex<Option<String>>,
    lun_file_path: Mutex<Option<PathBuf>>,
    udc_updating: AtomicBool,
}

impl Ums1Handle {
    pub fn new(enabled: bool, rx: mpsc::Receiver<(UmsFtEvent, oneshot::Sender<UmsFtResponse>)>) -> Arc<Self> {
        let handle = Arc::new(Self {
            name: config::UMS_FT_INQUIRY_STRING.to_owned(),
            inquiry_string: config::UMS_FT_INQUIRY_STRING.to_owned(),
            enabled: AtomicBool::new(enabled),
            file_path: Mutex::new(None),
            lun_file_path: Mutex::new(None),
            udc_updating: AtomicBool::new(false),
        });
        let _ = Self::loop_events(handle.clone(), rx);
        handle
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
}

impl Ums1Handle {
    fn loop_events(
        handle: Arc<Self>,
        mut rx: mpsc::Receiver<(UmsFtEvent, oneshot::Sender<UmsFtResponse>)>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Some((event, tx)) => {
                        handle.handle_event(event, tx).await;
                    }

                    None => break,
                }
            }
        })
    }

    async fn handle_event(&self, event: UmsFtEvent, tx: oneshot::Sender<UmsFtResponse>) {
        match event {
            UmsFtEvent::Mount(image_path) => {
                let result = self.mount_ums1(image_path.as_str()).await;
                if let Err(e) = tx.send(UmsFtResponse::Mount(result)) {
                    error!("failed to send mount response: {:?}", e);
                }
            }
            UmsFtEvent::Unmount => {
                let result = self.unmount_ums1().await;
                if let Err(e) = tx.send(UmsFtResponse::Unmount(result)) {
                    error!("failed to send unmount response: {:?}", e);
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

    async fn mount_ums1(&self, image_path: &str) -> Result<(), UsbError> {
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
        if file_path.is_some() {
            return Err(UsbError::ChangeRejected(format!("{} is busy", self.name())));
        }

        super::write_file(&device_path, image_path).await?;
        *file_path = Some(image_path.to_owned());
        Ok(())
    }

    async fn unmount_ums1(&self) -> Result<(), UsbError> {
        if !self.enabled.load(Ordering::Acquire) {
            return Err(UsbError::DeviceNotEnabled(self.name().to_owned()));
        }

        if self.udc_updating.load(Ordering::Acquire) {
            return Err(UsbError::ChangeRejected(format!("USBGadget is updating")));
        }

        let device_path = self.get_lun_file_path().await?;
        if !device_path.exists() {
            return Err(UsbError::DeviceNotFound(device_path.display().to_string()));
        }

        let mut file_path = self.file_path.lock().await;
        super::write_file(&device_path, "\n").await?;
        *file_path = None;
        Ok(())
    }
}

#[async_trait]
impl UsbChangeHandler for Ums1Handle {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    async fn on_change_request(&self, req: &ChangeRequest) -> ChangeResponse {
        match req {
            ChangeRequest::RequestChange => {
                if self.file_path.lock().await.is_some() {
                    ChangeResponse::Reject(format!("{} is busy", self.name()))
                }
                else {
                    self.udc_updating.store(true, Ordering::Release);
                    ChangeResponse::Proceed
                }
            }

            ChangeRequest::PrepareChange => {
                *self.lun_file_path.lock().await = None;
                ChangeResponse::Proceed
            }

            ChangeRequest::ChangeCompleted | ChangeRequest::ChangeCanceled => {
                self.sync_lun_file_path().await;
                self.udc_updating.store(false, Ordering::Release);
                ChangeResponse::Proceed
            }
        }
    }
}
