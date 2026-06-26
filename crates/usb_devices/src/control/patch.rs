
use crate::proto::v1::*;

pub use crate::proto::v1::{
    ApplySwitchesRequest as ProtoApplySwitchesRequest,
    ApplySwitchesResponse as ProtoApplySwitchesResponse, GetSwitchesRequest, 
};
 
pub fn apply_response_ok(applied: DeviceSwitches) -> ApplySwitchesResponse {
    ApplySwitchesResponse {
        ok: true,
        applied: Some(applied),
        error: None,
    }
}

pub fn apply_response_err(applied: DeviceSwitches, error: impl Into<String>) -> ApplySwitchesResponse {
    ApplySwitchesResponse {
        ok: false,
        applied: Some(applied),
        error: Some(error.into()),
    }
}

pub fn get_response_ok(applied: DeviceSwitches) -> GetSwitchesResponse {
    GetSwitchesResponse {
        ok: true,
        applied: Some(applied),
        error: None,
    }
}

pub fn decode_apply_request(bytes: &[u8]) -> Result<ApplySwitchesRequest, prost::DecodeError> {
    prost::Message::decode(bytes)
}

pub fn decode_apply_runtime_request(
    bytes: &[u8],
) -> Result<ApplyRuntimeConfigRequest, prost::DecodeError> {
    prost::Message::decode(bytes)
}

pub fn encode_apply_response(resp: &ApplySwitchesResponse) -> Vec<u8> {
    prost::Message::encode_to_vec(resp)
}

pub fn encode_apply_runtime_response(resp: &ApplyRuntimeConfigResponse) -> Vec<u8> {
    prost::Message::encode_to_vec(resp)
}

pub fn encode_get_response(resp: &GetSwitchesResponse) -> Vec<u8> {
    prost::Message::encode_to_vec(resp)
}

pub fn decode_ums_control_request(bytes: &[u8]) -> Result<UmsControlRequest, prost::DecodeError> {
    prost::Message::decode(bytes)
}

pub fn encode_ums_control_response(resp: &UmsControlResponse) -> Vec<u8> {
    prost::Message::encode_to_vec(resp)
}
 