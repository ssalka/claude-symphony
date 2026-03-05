//! Stream-JSON event types for the Claude CLI protocol.
//!
//! Claude CLI v2+ uses `--output-format stream-json` and emits newline-delimited
//! JSON events. Each event has a `"type"` field that determines its structure.

use serde::Deserialize;
use serde_json::Value;

// -------------------------------------------------------------------------- //
// Stream event envelope
// -------------------------------------------------------------------------- //

/// A single event from the Claude CLI stream-json output.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Emitted once at the start of a session.
    System(SystemEvent),
    /// Assistant message content (text, tool use, etc.).
    Assistant(AssistantEvent),
    /// Terminal event indicating the run completed.
    Result(ResultEvent),
    /// Rate limit information from the API.
    #[serde(rename = "rate_limit_event")]
    RateLimitEvent(RateLimitEvent),
    /// Any other event type we don't explicitly handle.
    #[serde(other)]
    Unknown,
}

// -------------------------------------------------------------------------- //
// Event payloads
// -------------------------------------------------------------------------- //

/// System init event — carries the session ID and model info.
#[derive(Debug, Deserialize)]
pub struct SystemEvent {
    pub subtype: Option<String>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub tools: Option<Value>,
}

/// Assistant message event — carries content blocks and usage.
#[derive(Debug, Deserialize)]
pub struct AssistantEvent {
    pub message: AssistantMessage,
    pub session_id: Option<String>,
}

/// The message body within an assistant event.
#[derive(Debug, Deserialize)]
pub struct AssistantMessage {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    pub usage: Option<Usage>,
}

/// A single content block within an assistant message.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text output.
    Text { text: String },
    /// Tool invocation.
    ToolUse {
        name: String,
        input: Value,
        #[serde(default)]
        id: Option<String>,
    },
    /// Tool result (returned in subsequent messages).
    ToolResult {
        tool_use_id: Option<String>,
        content: Option<Value>,
    },
    /// Any other content block type.
    #[serde(other)]
    Other,
}

/// Token usage counters.
#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// Terminal result event — indicates the run completed or failed.
#[derive(Debug, Deserialize)]
pub struct ResultEvent {
    pub subtype: Option<String>,
    pub result: Option<String>,
    pub session_id: Option<String>,
    pub usage: Option<Usage>,
    pub total_cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub num_turns: Option<u32>,
}

/// Rate limit event payload.
#[derive(Debug, Deserialize)]
pub struct RateLimitEvent {
    pub rate_limit_info: Option<Value>,
}

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_init_event() {
        let json = r#"{"type":"system","subtype":"init","session_id":"abc-123","model":"claude-sonnet-4-20250514","tools":[]}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::System(sys) => {
                assert_eq!(sys.subtype.as_deref(), Some("init"));
                assert_eq!(sys.session_id.as_deref(), Some("abc-123"));
                assert_eq!(sys.model.as_deref(), Some("claude-sonnet-4-20250514"));
            }
            _ => panic!("expected System event"),
        }
    }

    #[test]
    fn test_assistant_text_event() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world"}],"usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::Assistant(evt) => {
                assert_eq!(evt.message.content.len(), 1);
                match &evt.message.content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
                    _ => panic!("expected Text block"),
                }
                let usage = evt.message.usage.unwrap();
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.output_tokens, 50);
            }
            _ => panic!("expected Assistant event"),
        }
    }

    #[test]
    fn test_assistant_tool_use_event() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/src/main.rs"},"id":"tu_1"}]}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::Assistant(evt) => match &evt.message.content[0] {
                ContentBlock::ToolUse { name, input, id } => {
                    assert_eq!(name, "Read");
                    assert_eq!(input["file_path"], "/src/main.rs");
                    assert_eq!(id.as_deref(), Some("tu_1"));
                }
                _ => panic!("expected ToolUse block"),
            },
            _ => panic!("expected Assistant event"),
        }
    }

    #[test]
    fn test_result_success_event() {
        let json = r#"{"type":"result","subtype":"success","result":"Task completed","duration_ms":4521,"num_turns":3,"total_cost_usd":0.0234,"usage":{"input_tokens":5000,"output_tokens":2000},"session_id":"abc-123"}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::Result(res) => {
                assert_eq!(res.subtype.as_deref(), Some("success"));
                assert_eq!(res.result.as_deref(), Some("Task completed"));
                assert_eq!(res.duration_ms, Some(4521));
                assert_eq!(res.num_turns, Some(3));
                assert!((res.total_cost_usd.unwrap() - 0.0234).abs() < 0.0001);
                let usage = res.usage.unwrap();
                assert_eq!(usage.input_tokens, 5000);
                assert_eq!(usage.output_tokens, 2000);
            }
            _ => panic!("expected Result event"),
        }
    }

    #[test]
    fn test_result_error_event() {
        let json = r#"{"type":"result","subtype":"error","result":"Something went wrong"}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::Result(res) => {
                assert_eq!(res.subtype.as_deref(), Some("error"));
            }
            _ => panic!("expected Result event"),
        }
    }

    #[test]
    fn test_rate_limit_event() {
        let json = r#"{"type":"rate_limit_event","rate_limit_info":{"requests_remaining":10}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::RateLimitEvent(evt) => {
                assert!(evt.rate_limit_info.is_some());
            }
            _ => panic!("expected RateLimitEvent"),
        }
    }

    #[test]
    fn test_unknown_event_type() {
        let json = r#"{"type":"some_future_event","data":"hello"}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, StreamEvent::Unknown));
    }

    #[test]
    fn test_assistant_multiple_content_blocks() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me read that file"},{"type":"tool_use","name":"Read","input":{"file_path":"/src/lib.rs"}}]}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::Assistant(evt) => {
                assert_eq!(evt.message.content.len(), 2);
                assert!(matches!(&evt.message.content[0], ContentBlock::Text { .. }));
                assert!(matches!(
                    &evt.message.content[1],
                    ContentBlock::ToolUse { .. }
                ));
            }
            _ => panic!("expected Assistant event"),
        }
    }

    #[test]
    fn test_assistant_with_session_id() {
        let json = r#"{"type":"assistant","message":{"content":[]},"session_id":"sess-42"}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::Assistant(evt) => {
                assert_eq!(evt.session_id.as_deref(), Some("sess-42"));
            }
            _ => panic!("expected Assistant event"),
        }
    }

    #[test]
    fn test_content_block_other_variant() {
        let json =
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"}]}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::Assistant(evt) => {
                assert_eq!(evt.message.content.len(), 1);
                assert!(matches!(&evt.message.content[0], ContentBlock::Other));
            }
            _ => panic!("expected Assistant event"),
        }
    }

    #[test]
    fn test_usage_defaults_to_zero() {
        let json = r#"{"input_tokens":0,"output_tokens":0}"#;
        let usage: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }
}
