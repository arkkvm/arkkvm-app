pub mod rtc_request_params;
pub mod rtc_response_params;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct VirtualCMState {
    #[serde(rename = "video")]
    pub camera: bool,
    #[serde(rename = "audio")]
    pub microphone: bool,
}