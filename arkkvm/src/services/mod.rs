
pub mod audio;
pub mod virtual_mic;
pub mod gui_pipeline;

pub async fn init_audio_service(quality: f32) -> anyhow::Result<()> {
    audio::init(quality).await
}

pub async fn init_virtual_mic_service() -> anyhow::Result<()> {
    virtual_mic::init().await
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