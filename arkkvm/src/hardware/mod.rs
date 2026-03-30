pub mod mpi;
pub mod audio;
pub mod block_device;
pub mod display;
pub mod hw;
pub mod native;
pub mod usb;
// pub mod fs_fat32;
pub mod atx;
pub mod fs_remote;
pub mod hdmi;

pub async fn init() -> anyhow::Result<()> {
    atx::init().await
}
