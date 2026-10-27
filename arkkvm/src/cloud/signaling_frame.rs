//! Cloud signaling frame helpers for `client_id` echo (testable without WS).
//! See arkkvm-cloud/docs/ws/MULTI_CLIENT_SIGNALING.md.

use serde_json::{Value, json};

/// Read top-level `client_id` from an inbound cloud frame (offer / session_request).
pub fn parse_client_id(message: &Value) -> Option<String> {
    message
        .get("client_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// Build answer frame; omit `client_id` when unset (legacy clouds / firmware path).
pub fn build_answer_frame(
    answer_sdp: &str,
    session_id: &str,
    client_type: Option<&str>,
    client_id: Option<&str>,
) -> Value {
    let mut response = json!({
        "type": "answer",
        "session_id": session_id,
        "data": answer_sdp,
    });
    if let Some(ct) = client_type {
        response["clientType"] = json!(ct);
    }
    if let Some(cid) = client_id.filter(|s| !s.is_empty()) {
        response["client_id"] = json!(cid);
    }
    response
}

/// Build agent-slot reject error; include `client_id` when known so cloud can demux.
pub fn build_agent_occupied_error(session_id: &str, client_id: Option<&str>) -> Value {
    let mut err = json!({
        "type": "error",
        "session_id": session_id,
        "data": {
            "code": "agentSlotOccupied",
            "message": "Agent session slot is already occupied"
        }
    });
    if let Some(cid) = client_id.filter(|s| !s.is_empty()) {
        err["client_id"] = json!(cid);
    }
    err
}

/// Build MCP-disabled reject error for agent claim (ARKKVM-619).
pub fn build_agent_mcp_disabled_error(session_id: &str, client_id: Option<&str>) -> Value {
    let mut err = json!({
        "type": "error",
        "session_id": session_id,
        "data": {
            "code": "mcpDisabled",
            "message": "MCP is disabled on the device; enable MCP before agent control"
        }
    });
    if let Some(cid) = client_id.filter(|s| !s.is_empty()) {
        err["client_id"] = json!(cid);
    }
    err
}

/// Build cloud ICE candidate frame with optional `client_id` echo.
pub fn build_cloud_ice_frame(
    session_id: &str,
    candidate: Value,
    client_id: Option<&str>,
) -> Value {
    let mut message = json!({
        "type": "new-ice-candidate",
        "session_id": session_id,
        "data": candidate
    });
    if let Some(cid) = client_id.filter(|s| !s.is_empty()) {
        message["client_id"] = json!(cid);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_client_id_present() {
        let msg = json!({"type":"offer","client_id":"abc","data":{}});
        assert_eq!(parse_client_id(&msg).as_deref(), Some("abc"));
    }

    #[test]
    fn parse_client_id_absent() {
        let msg = json!({"type":"offer","data":{}});
        assert!(parse_client_id(&msg).is_none());
    }

    #[test]
    fn answer_includes_client_id_when_set() {
        let v = build_answer_frame("sdp", "sid", Some("user"), Some("cid"));
        assert_eq!(v["client_id"], "cid");
        assert_eq!(v["session_id"], "sid");
        assert_eq!(v["clientType"], "user");
        assert_eq!(v["data"], "sdp");
    }

    #[test]
    fn answer_omits_client_id_when_unset() {
        let v = build_answer_frame("sdp", "sid", None, None);
        assert!(v.get("client_id").is_none());
    }

    #[test]
    fn error_and_ice_echo_client_id() {
        let e = build_agent_occupied_error("sid", Some("cid"));
        assert_eq!(e["client_id"], "cid");
        assert_eq!(e["data"]["code"], "agentSlotOccupied");

        let mcp = build_agent_mcp_disabled_error("sid", Some("cid"));
        assert_eq!(mcp["client_id"], "cid");
        assert_eq!(mcp["data"]["code"], "mcpDisabled");

        let ice = build_cloud_ice_frame("sid", json!({"candidate":"x"}), Some("cid"));
        assert_eq!(ice["client_id"], "cid");
        assert_eq!(ice["session_id"], "sid");

        let ice_legacy = build_cloud_ice_frame("sid", json!({}), None);
        assert!(ice_legacy.get("client_id").is_none());
    }
}
