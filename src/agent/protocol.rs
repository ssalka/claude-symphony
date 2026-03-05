//! JSON-RPC 2.0 message types for the Claude Code stdio protocol.
//!
//! Claude Code communicates over stdio using a JSON-RPC 2.0-style protocol.
//! This module defines the request/response envelope types and the
//! domain-specific result payloads used during session initialization.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// -------------------------------------------------------------------------- //
// Outgoing requests
// -------------------------------------------------------------------------- //

/// A JSON-RPC 2.0 request (or notification when `id` is `None`).
///
/// Serialized directly to the Claude Code subprocess stdin, one JSON object
/// per line.
#[derive(Debug, Serialize)]
pub struct Request {
    /// `null` for notifications (no reply expected), otherwise a u64 request id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,

    /// The RPC method name (e.g. `"initialize"`, `"thread/start"`).
    pub method: String,

    /// Method parameters as an arbitrary JSON value (usually an object).
    pub params: Value,
}

impl Request {
    /// Create a request that expects a response, identified by `id`.
    pub fn with_id(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            id: Some(Value::Number(id.into())),
            method: method.into(),
            params,
        }
    }

    /// Create a notification (no response expected — `id` field omitted).
    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self {
            id: None,
            method: method.into(),
            params,
        }
    }
}

// -------------------------------------------------------------------------- //
// Generic incoming response / event envelope
// -------------------------------------------------------------------------- //

/// Generic JSON-RPC response or server-initiated event envelope.
///
/// Claude Code sends both reply messages (matching a prior request `id`) and
/// unsolicited event notifications (where `method` is populated and `id` may
/// be absent or carry the originating request id for approval flows).
#[derive(Debug, Deserialize)]
pub struct Response {
    /// Present on replies to requests.  May also be present on approval
    /// requests so that the client can send an approval response with the
    /// same id.
    pub id: Option<Value>,

    /// Populated on success replies.
    pub result: Option<Value>,

    /// Populated on error replies.
    pub error: Option<Value>,

    /// Populated on server-initiated events / notifications.
    pub method: Option<String>,

    /// Parameters accompanying a server-initiated event.
    pub params: Option<Value>,
}

// -------------------------------------------------------------------------- //
// Strongly-typed result payloads
// -------------------------------------------------------------------------- //

/// Decoded result payload for the `initialize` method response.
#[derive(Debug, Deserialize)]
pub struct InitializeResult {
    /// The protocol version negotiated by the server.  Optional because some
    /// implementations omit it.
    pub protocol_version: Option<String>,
}

/// Decoded result payload for the `thread/start` method response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResult {
    pub thread: ThreadInfo,
}

/// Thread identity returned by `thread/start`.
#[derive(Debug, Deserialize)]
pub struct ThreadInfo {
    pub id: String,
}

/// Decoded result payload for the `turn/start` method response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResult {
    pub turn: TurnInfo,
}

/// Turn identity returned by `turn/start`.
#[derive(Debug, Deserialize)]
pub struct TurnInfo {
    pub id: String,
}

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- Request serialization -------------------------------------------- //

    #[test]
    fn test_request_with_id_serializes_id_field() {
        let req = Request::with_id(1, "initialize", json!({"foo": "bar"}));
        let json = serde_json::to_value(&req).unwrap();

        assert_eq!(json["id"], json!(1));
        assert_eq!(json["method"], "initialize");
        assert_eq!(json["params"]["foo"], "bar");
    }

    #[test]
    fn test_notification_omits_id_field() {
        let req = Request::notification("initialized", json!({}));
        let json = serde_json::to_value(&req).unwrap();

        // id must be absent (skip_serializing_if = "Option::is_none")
        assert!(!json.as_object().unwrap().contains_key("id"));
        assert_eq!(json["method"], "initialized");
    }

    #[test]
    fn test_request_with_id_roundtrip_string() {
        let req = Request::with_id(
            42,
            "thread/start",
            json!({"approvalPolicy": "auto", "cwd": "/tmp"}),
        );
        let serialized = serde_json::to_string(&req).unwrap();
        // Verify it is valid JSON and contains expected fields.
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["method"], "thread/start");
        assert_eq!(parsed["params"]["approvalPolicy"], "auto");
    }

    #[test]
    fn test_notification_method_is_set() {
        let req = Request::notification("turn/cancel", json!({"threadId": "t1"}));
        assert_eq!(req.method, "turn/cancel");
        assert!(req.id.is_none());
    }

    // ---- Response deserialization ----------------------------------------- //

    #[test]
    fn test_response_deserializes_result() {
        let raw = r#"{"id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
        let resp: Response = serde_json::from_str(raw).unwrap();

        assert_eq!(resp.id, Some(json!(1)));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        assert!(resp.method.is_none());
    }

    #[test]
    fn test_response_deserializes_error() {
        let raw = r#"{"id":2,"error":{"code":-32600,"message":"Invalid Request"}}"#;
        let resp: Response = serde_json::from_str(raw).unwrap();

        assert!(resp.error.is_some());
        assert!(resp.result.is_none());
    }

    #[test]
    fn test_response_deserializes_event_notification() {
        let raw = r#"{"method":"turn/completed","params":{"turnId":"t1"}}"#;
        let resp: Response = serde_json::from_str(raw).unwrap();

        assert_eq!(resp.method.as_deref(), Some("turn/completed"));
        assert!(resp.params.is_some());
        assert!(resp.id.is_none());
    }

    // ---- InitializeResult deserialization --------------------------------- //

    #[test]
    fn test_initialize_result_with_version() {
        let raw = r#"{"protocol_version":"2024-11-05"}"#;
        let res: InitializeResult = serde_json::from_str(raw).unwrap();
        assert_eq!(res.protocol_version.as_deref(), Some("2024-11-05"));
    }

    #[test]
    fn test_initialize_result_without_version() {
        let raw = r#"{}"#;
        let res: InitializeResult = serde_json::from_str(raw).unwrap();
        assert!(res.protocol_version.is_none());
    }

    // ---- ThreadStartResult deserialization -------------------------------- //

    #[test]
    fn test_thread_start_result_deserializes() {
        let raw = r#"{"thread":{"id":"thread-abc-123"}}"#;
        let res: ThreadStartResult = serde_json::from_str(raw).unwrap();
        assert_eq!(res.thread.id, "thread-abc-123");
    }

    #[test]
    fn test_thread_info_id() {
        let info = ThreadInfo {
            id: "my-thread".to_string(),
        };
        assert_eq!(info.id, "my-thread");
    }

    // ---- TurnStartResult deserialization ---------------------------------- //

    #[test]
    fn test_turn_start_result_deserializes() {
        let raw = r#"{"turn":{"id":"turn-xyz-456"}}"#;
        let res: TurnStartResult = serde_json::from_str(raw).unwrap();
        assert_eq!(res.turn.id, "turn-xyz-456");
    }

    #[test]
    fn test_turn_info_id() {
        let info = TurnInfo {
            id: "my-turn".to_string(),
        };
        assert_eq!(info.id, "my-turn");
    }

    // ---- Full initialize handshake simulation ----------------------------- //

    #[test]
    fn test_initialize_request_shape() {
        let req = Request::with_id(
            1,
            "initialize",
            json!({
                "clientInfo": {"name": "symphony", "version": "1.0"},
                "capabilities": {}
            }),
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["method"], "initialize");
        assert_eq!(v["params"]["clientInfo"]["name"], "symphony");
        assert_eq!(v["params"]["clientInfo"]["version"], "1.0");
    }

    #[test]
    fn test_initialized_notification_shape() {
        let req = Request::notification("initialized", json!({}));
        let v = serde_json::to_value(&req).unwrap();
        assert!(!v.as_object().unwrap().contains_key("id"));
        assert_eq!(v["method"], "initialized");
    }

    #[test]
    fn test_thread_start_request_shape() {
        let req = Request::with_id(
            2,
            "thread/start",
            json!({
                "approvalPolicy": "auto",
                "sandbox": {},
                "cwd": "/workspace/my-project"
            }),
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["id"], 2);
        assert_eq!(v["method"], "thread/start");
        assert_eq!(v["params"]["approvalPolicy"], "auto");
        assert_eq!(v["params"]["cwd"], "/workspace/my-project");
    }
}
