use std::time::Duration;

use anyhow::{Context, Result};
use prost::Message;
use tracing::{error, info, warn};
use zenoh::Session;
use zenoh::bytes::ZBytes;
use zenoh::query::Query;

use super::patch::{
    apply_response_err, apply_response_ok, decode_apply_request, decode_apply_runtime_request,
    decode_ums_control_request, encode_apply_response, encode_apply_runtime_response,
    encode_get_response, encode_ums_control_response, get_response_ok,
};
use super::service::ControlHandle;
use crate::control::zenoh_bus::{get_session, init};
use crate::proto::v1::*;

pub const KEY_APPLY: &str = "arkkvm/usb_devices/query/apply_switches";
pub const KEY_APPLY_RUNTIME: &str = "arkkvm/usb_devices/query/apply_runtime_config";
pub const KEY_GET: &str = "arkkvm/usb_devices/query/get_switches";
pub const KEY_UMS_CONTROL: &str = "arkkvm/usb_devices/query/ums_control";

pub const KEY_PUT_KEYBOARD: &str = "arkkvm/usb_devices/event/keyboard";
pub const KEY_PUT_ABSMOUSE: &str = "arkkvm/usb_devices/event/absmouse";
pub const KEY_PUT_RELMOUSE: &str = "arkkvm/usb_devices/event/relmouse";
pub const KEY_PUT_WHEEL: &str = "arkkvm/usb_devices/event/wheel";

pub const KEY_EVENT_KEYBOARD_LED: &str = "arkkvm/usb_devices/event/keyboard_led";
pub const KEY_EVENT_UDC_STATE: &str = "arkkvm/usb_devices/event/udc_state";
pub const KEY_EVENT_MIC_PROCESS: &str = "arkkvm/usb_devices/event/mic_process_state";
pub const KEY_GET_UDC_STATUS: &str = "arkkvm/usb_devices/query/get_udc_status";
pub const KEY_GET_USB_EMULATION_STATE: &str = "arkkvm/usb_devices/query/get_usb_emulation_state";
pub const KEY_SET_USB_EMULATION_STATE: &str = "arkkvm/usb_devices/query/set_usb_emulation_state";
pub const KEY_SET_MIC_PROCESS: &str = "arkkvm/usb_devices/query/set_mic_process";
pub const KEY_GET_MIC_PROCESS_STATE: &str = "arkkvm/usb_devices/query/get_mic_process_state";

pub async fn open_client() -> Result<Session> {
    zenoh::init_log_from_env_or("warn");

    init().await?;

    let session = get_session();
    info!("usb_devices zenoh session opened ");
    Ok(session)
}

pub async fn serve(session: Session, control: ControlHandle) -> Result<()> {
    let apply = session
        .declare_queryable(KEY_APPLY)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare queryable: {}", e))?;
    let apply_runtime = session
        .declare_queryable(KEY_APPLY_RUNTIME)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare runtime queryable: {}", e))?;
    let get = session
        .declare_queryable(KEY_GET)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare queryable: {}", e))?;
    let ums = session
        .declare_queryable(KEY_UMS_CONTROL)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare ums queryable: {}", e))?;
    let get_udc_status = session
        .declare_queryable(KEY_GET_UDC_STATUS)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare get_udc_status queryable: {}", e))?;
    let get_usb_emulation =
        session.declare_queryable(KEY_GET_USB_EMULATION_STATE).await.map_err(|e| {
            anyhow::anyhow!("Failed to declare get_usb_emulation_state queryable: {}", e)
        })?;
    let set_usb_emulation =
        session.declare_queryable(KEY_SET_USB_EMULATION_STATE).await.map_err(|e| {
            anyhow::anyhow!("Failed to declare set_usb_emulation_state queryable: {}", e)
        })?;
    let set_mic_process = session
        .declare_queryable(KEY_SET_MIC_PROCESS)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare set_mic_process queryable: {}", e))?;
    let get_mic_process_state = session
        .declare_queryable(KEY_GET_MIC_PROCESS_STATE)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare get_mic_process_state queryable: {}", e))?;
    info!(
        "zenoh queryables ready: {}, {}, {}, {}, {}, {}, {}, {}, {}",
        KEY_APPLY,
        KEY_APPLY_RUNTIME,
        KEY_GET,
        KEY_UMS_CONTROL,
        KEY_GET_UDC_STATUS,
        KEY_GET_USB_EMULATION_STATE,
        KEY_SET_USB_EMULATION_STATE,
        KEY_SET_MIC_PROCESS,
        KEY_GET_MIC_PROCESS_STATE
    );
    let sub = session
        .declare_subscriber("arkkvm/usb_devices/event/**")
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare subscriber: {}", e))?;

    let control_apply = control.clone();
    let apply_task = tokio::spawn(async move {
        loop {
            let query = match apply.recv_async().await {
                Ok(q) => q,
                Err(e) => {
                    warn!("apply queryable closed: {}", e);
                    break;
                }
            };
            if let Err(e) = handle_apply_query(query, control_apply.clone()).await {
                error!("apply_switches query failed: {:?}", e);
            }
        }
    });

    let control_apply_runtime = control.clone();
    let apply_runtime_task = tokio::spawn(async move {
        loop {
            let query = match apply_runtime.recv_async().await {
                Ok(q) => q,
                Err(e) => {
                    warn!("apply_runtime queryable closed: {}", e);
                    break;
                }
            };
            if let Err(e) = handle_apply_runtime_query(query, control_apply_runtime.clone()).await {
                error!("apply_runtime_config query failed: {:?}", e);
            }
        }
    });

    let control_ums = control.clone();
    let ums_task = tokio::spawn(async move {
        loop {
            let query = match ums.recv_async().await {
                Ok(q) => q,
                Err(e) => {
                    warn!("ums_control queryable closed: {}", e);
                    break;
                }
            };
            if let Err(e) = handle_ums_control_query(query, control_ums.clone()).await {
                error!("ums_control query failed: {:?}", e);
            }
        }
    });

    let control_get = control.clone();
    let get_task = tokio::spawn(async move {
        loop {
            let query = match get.recv_async().await {
                Ok(q) => q,
                Err(e) => {
                    warn!("get queryable closed: {}", e);
                    break;
                }
            };
            if let Err(e) = handle_get_query(query, control_get.clone()).await {
                error!("get_switches query failed: {:?}", e);
            }
        }
    });

    let control_get_udc = control.clone();
    let get_udc_task = tokio::spawn(async move {
        loop {
            let query = match get_udc_status.recv_async().await {
                Ok(q) => q,
                Err(e) => {
                    warn!("get_udc_status queryable closed: {}", e);
                    break;
                }
            };
            if let Err(e) = handle_get_udc_status_query(query, control_get_udc.clone()).await {
                error!("get_udc_status query failed: {:?}", e);
            }
        }
    });

    let control_get_emulation = control.clone();
    let get_emulation_task = tokio::spawn(async move {
        loop {
            let query = match get_usb_emulation.recv_async().await {
                Ok(q) => q,
                Err(e) => {
                    warn!("get_usb_emulation_state queryable closed: {}", e);
                    break;
                }
            };
            if let Err(e) =
                handle_get_usb_emulation_state_query(query, control_get_emulation.clone()).await
            {
                error!("get_usb_emulation_state query failed: {:?}", e);
            }
        }
    });

    let control_set_emulation = control.clone();
    let set_emulation_task = tokio::spawn(async move {
        loop {
            let query = match set_usb_emulation.recv_async().await {
                Ok(q) => q,
                Err(e) => {
                    warn!("set_usb_emulation_state queryable closed: {}", e);
                    break;
                }
            };
            if let Err(e) =
                handle_set_usb_emulation_state_query(query, control_set_emulation.clone()).await
            {
                error!("set_usb_emulation_state query failed: {:?}", e);
            }
        }
    });

    let control_set_mic = control.clone();
    let set_mic_process_task = tokio::spawn(async move {
        loop {
            let query = match set_mic_process.recv_async().await {
                Ok(q) => q,
                Err(e) => {
                    warn!("set_mic_process queryable closed: {}", e);
                    break;
                }
            };
            if let Err(e) = handle_set_mic_process_query(query, control_set_mic.clone()).await {
                error!("set_mic_process query failed: {:?}", e);
            }
        }
    });

    let control_get_mic = control.clone();
    let get_mic_process_task = tokio::spawn(async move {
        loop {
            let query = match get_mic_process_state.recv_async().await {
                Ok(q) => q,
                Err(e) => {
                    warn!("get_mic_process_state queryable closed: {}", e);
                    break;
                }
            };
            if let Err(e) = handle_get_mic_process_state_query(query, control_get_mic.clone()).await
            {
                error!("get_mic_process_state query failed: {:?}", e);
            }
        }
    });

    let control_put = control.clone();
    let put_task = tokio::spawn(async move {
        loop {
            let sample = match sub.recv_async().await {
                Ok(s) => s,
                Err(e) => {
                    warn!("put subscriber closed: {}", e);
                    break;
                }
            };
            let key = sample.key_expr().to_string();
            let payload = sample.payload().to_bytes();

            match key.as_str() {
                KEY_PUT_KEYBOARD => {
                    let event = match KeyboardReportParams::decode(payload.as_ref()) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!("Failed to decode keyboard event: {:?}", e);
                            continue;
                        }
                    };
                    control_put.key_put_keyboard(event).await.unwrap_or_else(|e| {
                        error!("Failed to handle keyboard event: {:?}", e);
                    });
                }
                KEY_PUT_ABSMOUSE => {
                    let event = match AbsMouseReportParams::decode(payload.as_ref()) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!("Failed to decode abs mouse event: {:?}", e);
                            continue;
                        }
                    };
                    control_put.key_put_absmouse(event).await.unwrap_or_else(|e| {
                        error!("Failed to handle abs mouse event: {:?}", e);
                    });
                }
                KEY_PUT_RELMOUSE => {
                    let event = match RelMouseReportParams::decode(payload.as_ref()) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!("Failed to decode rel mouse event: {:?}", e);
                            continue;
                        }
                    };
                    control_put.key_put_relmouse(event).await.unwrap_or_else(|e| {
                        error!("Failed to handle rel mouse event: {:?}", e);
                    });
                }

                KEY_PUT_WHEEL => {
                    let event = match WheelReportParams::decode(payload.as_ref()) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!("Failed to decode wheel event: {:?}", e);
                            continue;
                        }
                    };
                    control_put.key_put_wheel(event).await.unwrap_or_else(|e| {
                        error!("Failed to handle wheel event: {:?}", e);
                    });
                }

                KEY_EVENT_UDC_STATE | KEY_EVENT_KEYBOARD_LED | KEY_EVENT_MIC_PROCESS => {}

                _ => {
                    warn!("Received event with unknown key: {}", key);
                }
            }
        }
    });

    tokio::select! {
        _ = apply_task => {},
        _ = apply_runtime_task => {},
        _ = get_task => {},
        _ = get_udc_task => {},
        _ = get_emulation_task => {},
        _ = set_emulation_task => {},
        _ = set_mic_process_task => {},
        _ = get_mic_process_task => {},
        _ = ums_task => {},
        _ = put_task => {},
    }

    Ok(())
}

async fn handle_apply_query(query: Query, control: ControlHandle) -> Result<()> {
    let bytes = match query.payload() {
        Some(p) => p.to_bytes(),
        None => {
            reply_bytes(
                &query,
                encode_apply_response(&ApplySwitchesResponse {
                    ok: false,
                    applied: None,
                    error: Some("empty query payload".into()),
                }),
            )
            .await?;
            return Ok(());
        }
    };

    let req: ApplySwitchesRequest = match decode_apply_request(bytes.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            reply_bytes(
                &query,
                encode_apply_response(&apply_response_err(
                    DeviceSwitches::default(),
                    format!("invalid ApplySwitchesRequest protobuf: {e}"),
                )),
            )
            .await?;
            return Ok(());
        }
    };

    let usb_info: crate::proto::v1::UsbDeviceInfo = req.usb_info.context("missing SwitchPatch")?;
    println!("Received apply USB switch config: {:?}", usb_info);
    let resp: ApplySwitchesResponse = match control.apply(usb_info).await {
        Ok(applied) => {
            info!("apply_switches ok: {:?}", applied);
            apply_response_ok(applied)
        }
        Err(e) => {
            warn!("apply_switches error: {:?}", e);
            let applied = control.get().await.unwrap_or_default();
            apply_response_err(applied, e.to_string())
        }
    };
    reply_bytes(&query, encode_apply_response(&resp)).await
}

async fn handle_get_query(query: Query, control: ControlHandle) -> Result<()> {
    let applied = control.get().await.unwrap_or_default();
    let resp = get_response_ok(applied);
    reply_bytes(&query, encode_get_response(&resp)).await
}

async fn handle_get_udc_status_query(query: Query, control: ControlHandle) -> Result<()> {
    let state = control.get_udc_status().await.unwrap_or_else(|e| {
        warn!("get_udc_status control error: {:?}", e);
        "unknown".to_string()
    });
    let resp = GetUdcStatusResponse { ok: true, state, error: None };
    let payload = prost::Message::encode_to_vec(&resp);
    reply_bytes(&query, payload).await
}

async fn handle_get_usb_emulation_state_query(query: Query, control: ControlHandle) -> Result<()> {
    match control.get_usb_emulation_state().await {
        Ok(enabled) => {
            let resp = GetUsbEmulationStateResponse { ok: true, enabled, error: None };
            reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await
        }
        Err(e) => {
            let resp = GetUsbEmulationStateResponse {
                ok: false,
                enabled: false,
                error: Some(e.to_string()),
            };
            reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await
        }
    }
}

async fn handle_set_usb_emulation_state_query(query: Query, control: ControlHandle) -> Result<()> {
    let bytes = match query.payload() {
        Some(p) => p.to_bytes(),
        None => {
            let resp = SetUsbEmulationStateResponse {
                ok: false,
                enabled: false,
                error: Some("empty query payload".into()),
            };
            return reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await;
        }
    };

    let req = match SetUsbEmulationStateRequest::decode(bytes.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            let resp = SetUsbEmulationStateResponse {
                ok: false,
                enabled: false,
                error: Some(format!("invalid SetUsbEmulationStateRequest: {e}")),
            };
            return reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await;
        }
    };

    match control.set_usb_emulation_state(req.enabled).await {
        Ok(enabled) => {
            let resp = SetUsbEmulationStateResponse { ok: true, enabled, error: None };
            reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await
        }
        Err(e) => {
            let enabled = control.get_usb_emulation_state().await.unwrap_or(false);
            let resp =
                SetUsbEmulationStateResponse { ok: false, enabled, error: Some(e.to_string()) };
            reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await
        }
    }
}

async fn handle_get_mic_process_state_query(query: Query, control: ControlHandle) -> Result<()> {
    match control.get_mic_process_state().await {
        Ok(running) => {
            let resp = GetMicProcessStateResponse { ok: true, running, error: None };
            reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await
        }
        Err(e) => {
            let resp = GetMicProcessStateResponse {
                ok: false,
                running: false,
                error: Some(e.to_string()),
            };
            reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await
        }
    }
}

async fn handle_set_mic_process_query(query: Query, control: ControlHandle) -> Result<()> {
    let bytes = match query.payload() {
        Some(p) => p.to_bytes(),
        None => {
            let resp =
                SetMicProcessResponse { ok: false, error: Some("empty query payload".into()) };
            return reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await;
        }
    };

    let req = match SetMicProcessRequest::decode(bytes.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            let resp = SetMicProcessResponse {
                ok: false,
                error: Some(format!("invalid SetMicProcessRequest: {e}")),
            };
            return reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await;
        }
    };

    match control.set_mic_process(req.enabled).await {
        Ok(()) => {
            let resp = SetMicProcessResponse { ok: true, error: None };
            reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await
        }
        Err(e) => {
            let resp = SetMicProcessResponse { ok: false, error: Some(e.to_string()) };
            reply_bytes(&query, prost::Message::encode_to_vec(&resp)).await
        }
    }
}

fn classify_runtime_error(err: &str) -> (i32, bool) {
    let lower = err.to_ascii_lowercase();
    if lower.contains("busy")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("recover")
    {
        (ApplyErrorCode::UsbGadgetBusy as i32, true)
    } else if lower.contains("invalid") || lower.contains("missing") {
        (ApplyErrorCode::InvalidArgument as i32, false)
    } else {
        (ApplyErrorCode::Internal as i32, false)
    }
}

async fn handle_apply_runtime_query(query: Query, control: ControlHandle) -> Result<()> {
    let bytes = match query.payload() {
        Some(p) => p.to_bytes(),
        None => {
            reply_bytes(
                &query,
                encode_apply_runtime_response(&ApplyRuntimeConfigResponse {
                    ok: false,
                    applied: None,
                    error: Some("empty query payload".into()),
                    error_code: ApplyErrorCode::InvalidArgument as i32,
                    retryable: false,
                    applied_usb_info: None,
                }),
            )
            .await?;
            return Ok(());
        }
    };

    let req = match decode_apply_runtime_request(bytes.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            reply_bytes(
                &query,
                encode_apply_runtime_response(&ApplyRuntimeConfigResponse {
                    ok: false,
                    applied: None,
                    error: Some(format!("invalid ApplyRuntimeConfigRequest protobuf: {e}")),
                    error_code: ApplyErrorCode::InvalidArgument as i32,
                    retryable: false,
                    applied_usb_info: None,
                }),
            )
            .await?;
            return Ok(());
        }
    };

    let resp = match control.apply_runtime(req).await {
        Ok(ok_resp) => ok_resp,
        Err(e) => {
            let applied = control.get().await.unwrap_or_default();
            let err = e.to_string();
            let (error_code, retryable) = classify_runtime_error(&err);
            ApplyRuntimeConfigResponse {
                ok: false,
                applied: Some(applied),
                error: Some(err),
                error_code,
                retryable,
                applied_usb_info: None,
            }
        }
    };

    reply_bytes(&query, encode_apply_runtime_response(&resp)).await
}

async fn handle_ums_control_query(query: Query, control: ControlHandle) -> Result<()> {
    let bytes = match query.payload() {
        Some(p) => p.to_bytes(),
        None => {
            reply_bytes(
                &query,
                encode_ums_control_response(&UmsControlResponse {
                    ok: false,
                    error: Some("empty query payload".into()),
                    mounted: false,
                    mounted_path: String::new(),
                    vm_type: 0,
                }),
            )
            .await?;
            return Ok(());
        }
    };

    let req = match decode_ums_control_request(bytes.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            reply_bytes(
                &query,
                encode_ums_control_response(&UmsControlResponse {
                    ok: false,
                    error: Some(format!("invalid UmsControlRequest: {e}")),
                    mounted: false,
                    mounted_path: String::new(),
                    vm_type: 0,
                }),
            )
            .await?;
            return Ok(());
        }
    };

    let resp = control.ums_control(req).await.unwrap_or_else(|e| UmsControlResponse {
        ok: false,
        error: Some(e.to_string()),
        mounted: false,
        mounted_path: String::new(),
        vm_type: 0,
    });
    reply_bytes(&query, encode_ums_control_response(&resp)).await
}

async fn reply_bytes(query: &Query, bytes: Vec<u8>) -> Result<()> {
    query
        .reply(query.key_expr(), ZBytes::from(bytes))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to reply: {}", e))?;
    Ok(())
}

/// Zenoh client helper for arkkvm (protobuf payload).
pub async fn query_apply(
    session: &Session,
    req: ApplySwitchesRequest,
) -> Result<ApplySwitchesResponse> {
    let payload = prost::Message::encode_to_vec(&req);
    let replies = session
        .get(KEY_APPLY)
        .payload(ZBytes::from(payload))
        .timeout(Duration::from_secs(30))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get: {}", e))?;

    let sample = replies.into_iter().next().context("no reply from usb_devices apply_switches")?;
    let reply = sample.into_result().context("failed to get reply")?;

    let bytes = reply.payload().to_bytes();
    ApplySwitchesResponse::decode(bytes.as_ref()).context("decode ApplySwitchesResponse")
}

pub async fn query_apply_runtime(
    session: &Session,
    req: ApplyRuntimeConfigRequest,
) -> Result<ApplyRuntimeConfigResponse> {
    let payload = prost::Message::encode_to_vec(&req);
    let replies = session
        .get(KEY_APPLY_RUNTIME)
        .payload(ZBytes::from(payload))
        .timeout(Duration::from_secs(30))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get runtime apply: {}", e))?;

    let sample =
        replies.into_iter().next().context("no reply from usb_devices apply_runtime_config")?;
    let reply = sample.into_result().context("failed to get runtime reply")?;
    let bytes = reply.payload().to_bytes();
    ApplyRuntimeConfigResponse::decode(bytes.as_ref()).context("decode ApplyRuntimeConfigResponse")
}

pub async fn send_keyboard_led_event(state: KeyboardState) -> Result<()> {
    let payload = prost::Message::encode_to_vec(&state);
    get_session()
        .put(KEY_EVENT_KEYBOARD_LED, ZBytes::from(payload))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send keyboard LED event: {}", e))?;
    Ok(())
}

pub async fn send_udc_state_event(state: &str) -> Result<()> {
    let payload = prost::Message::encode_to_vec(&UdcStatus { state: state.to_string() });
    get_session()
        .put(KEY_EVENT_UDC_STATE, ZBytes::from(payload))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send UDC state event: {}", e))?;
    Ok(())
}

pub async fn send_mic_process_state_event(running: bool) -> Result<()> {
    let payload = prost::Message::encode_to_vec(&MicProcessStateEvent { running });
    get_session()
        .put(KEY_EVENT_MIC_PROCESS, ZBytes::from(payload))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send mic process state event: {}", e))?;
    Ok(())
}
