
pub mod audio;
pub mod virtual_mic;
pub mod gui_pipeline;
pub mod usb;

pub use usb::get_usb;

pub async fn init_audio_service(quality: f32) -> anyhow::Result<()> {
    audio::init(quality).await
}

/// Force-start virtual mic (USB apply path; do not gate on saved config).
pub async fn init_virtual_mic_service() -> anyhow::Result<()> {
    virtual_mic::init().await
}

/// Startup: start only when microphone emulation is enabled in saved config.
pub async fn init_virtual_mic_from_config() -> anyhow::Result<()> {
    virtual_mic::init_from_saved_config().await
}

pub async fn uninit_virtual_mic_service() -> anyhow::Result<()> {
    virtual_mic::uninit().await
}

pub fn init_gui_pipeline() -> anyhow::Result<()> {
    gui_pipeline::init()
}

pub fn uninit_gui_pipeline() {
    gui_pipeline::uninit();
}

pub async fn init_usb_service() -> anyhow::Result<()> {
    usb::init().await
}