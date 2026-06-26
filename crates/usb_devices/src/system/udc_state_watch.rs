use crate::error::UsbError;
use inotify::{Inotify, WatchMask};
use tokio::sync::mpsc;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use usb_gadget::{Udc, UdcState};

const UDC_SYSFS_ROOT: &str = "/sys/class/udc";
const EVENT_SLEEP_INTERVAL_MS: u64 = 100;
const DEBOUNCE_MS: u64 = 5;

pub struct UdcStateWatch {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl UdcStateWatch {
    pub fn start(udc: Udc, tx: mpsc::Sender<Result<UdcState, String>>) -> Result<Self, UsbError> {
        let udc_name = udc.name().to_string_lossy().to_string();
        let mut inotify = Inotify::init()
            .map_err(|e| UsbError::UDCError(format!("init inotify failed: {}", e)))?;

        let state_path = PathBuf::from(UDC_SYSFS_ROOT).join(&udc_name).join("state");
        inotify
            .watches()
            .add(
                &state_path,
                WatchMask::MODIFY | WatchMask::ATTRIB | WatchMask::CLOSE_WRITE,
            )
            .map_err(|e| {
                UsbError::UDCError(format!(
                    "add inotify watch failed for {}: {}",
                    state_path.display(),
                    e
                ))
            })?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name(format!("udc-watch-{}", udc_name))
            .spawn(move || {
                // Initial state emit
                let _ = match udc.state() {
                    Ok(state) => tx.try_send(Ok(state)),
                    Err(e) => tx.try_send(Err((format!("read initial UDC state failed: {}", e)))),
                };

                let mut buf = [0u8; 4096];
                while !stop_clone.load(Ordering::Relaxed) {
                    match inotify.read_events(&mut buf) {
                        Ok(events) => {
                            if events.count() == 0 {
                                thread::sleep(Duration::from_millis(EVENT_SLEEP_INTERVAL_MS));
                                continue;
                            }
                            if stop_clone.load(Ordering::Relaxed) {
                                break;
                            }
                            // Coalesce bursts from sysfs updates.
                            thread::sleep(Duration::from_millis(DEBOUNCE_MS));
                            let _ = match udc.state() {
                                Ok(state) => tx.try_send(Ok(state)),
                                Err(e) => tx.try_send(Err((format!("read UDC state failed: {}", e)))),
                            };
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(EVENT_SLEEP_INTERVAL_MS));
                        }
                        Err(e) => {
                            let _ = tx.try_send(Err((format!("inotify read failed: {}", e))));
                            thread::sleep(Duration::from_millis(EVENT_SLEEP_INTERVAL_MS));
                        }
                    }
                }
            })
            .map_err(|e| UsbError::UDCError(format!("spawn udc watch thread failed: {}", e)))?;

        Ok(Self {
            stop,
            join: Some(join),
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for UdcStateWatch {
    fn drop(&mut self) {
        self.stop();
    }
}
