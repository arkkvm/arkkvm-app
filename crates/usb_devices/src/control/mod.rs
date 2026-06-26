pub mod patch;
pub mod service;
pub mod zenoh;
pub mod zenoh_bus;
pub mod zenoh_udc;

pub use patch::{
     ProtoApplySwitchesRequest, ProtoApplySwitchesResponse, 
};
pub use crate::proto::v1::{GetSwitchesRequest};
pub use service::{spawn_control_service, ControlHandle};
pub use zenoh::{open_client, query_apply, serve, KEY_APPLY, KEY_GET};
