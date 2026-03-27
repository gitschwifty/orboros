use serde::{Deserialize, Serialize};

// ── Protocol version ──

pub const PROTOCOL_VERSION: &str = "0.2.0";

// ── Requests (Orboros → Heddle) ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    Init {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        protocol_version: Option<String>,
        config: InitConfig,
    },
    Send {
        id: String,
        message: String,
    },
    Status {
        id: String,
    },
    Shutdown {
        id: String,
    },
    Cancel {
        id: String,
        target_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InitConfig {
    pub model: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}

/// Structured error envelope returned by heddle in protocol 0.2.0+.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorEnvelope {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

// ── Responses (Heddle → Orboros) ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcResponse {
    InitOk {
        id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        protocol_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorEnvelope>,
    },
    Event {
        event: WorkerEvent,
        #[serde(default)]
        event_seq: u32,
        #[serde(default)]
        send_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_id: Option<String>,
    },
    Result {
        id: String,
        status: ResultStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        response: Option<String>,
        #[serde(default)]
        tool_calls_made: Vec<ToolCallRecord>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default)]
        iterations: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorEnvelope>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_latency_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_latency_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_latency_ms: Option<u64>,
    },
    StatusOk {
        id: String,
        model: String,
        messages_count: u32,
        session_id: String,
        active: bool,
    },
    ShutdownOk {
        id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Ok,
    Error,
    Cancelled,
}

// ── Events (streamed during send) ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorkerEvent {
    ContentDelta {
        text: String,
    },
    ToolStart {
        name: String,
        args: serde_json::Value,
    },
    ToolEnd {
        name: String,
        result_preview: String,
    },
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
        total_tokens: u32,
    },
    Error {
        message: String,
        code: String,
        retryable: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    Heartbeat {
        duration_ms: u32,
    },
    ContextPrune {
        messages_pruned: u32,
        tokens_before: u32,
        tokens_after: u32,
    },
    ContextCompact {},
    ContextHandoff {},
}

// ── Shared types ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRecord {
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-fixtures/ipc")
    }

    fn parse_jsonl_requests(content: &str) -> Vec<IpcRequest> {
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn parse_jsonl_responses(content: &str) -> Vec<IpcResponse> {
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    // ── Round-trip tests ──

    #[test]
    fn round_trip_init_request() {
        let req = IpcRequest::Init {
            id: "1".into(),
            protocol_version: Some(PROTOCOL_VERSION.into()),
            config: InitConfig {
                model: "openrouter/auto".into(),
                system_prompt: "You are a helpful assistant.".into(),
                tools: vec!["read_file".into(), "glob".into()],
                max_iterations: Some(10),
                task_id: None,
                worker_id: None,
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, parsed);
    }

    #[test]
    fn round_trip_send_request() {
        let req = IpcRequest::Send {
            id: "2".into(),
            message: "Hello".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, parsed);
    }

    #[test]
    fn round_trip_cancel_request() {
        let req = IpcRequest::Cancel {
            id: "3".into(),
            target_id: "2".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, parsed);
    }

    #[test]
    fn round_trip_init_ok_response() {
        let resp = IpcResponse::InitOk {
            id: "1".into(),
            session_id: "sess-123".into(),
            protocol_version: Some("0.2.0".into()),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, parsed);
    }

    #[test]
    fn round_trip_result_ok() {
        let resp = IpcResponse::Result {
            id: "2".into(),
            status: ResultStatus::Ok,
            response: Some("Hello!".into()),
            tool_calls_made: vec![ToolCallRecord {
                name: "glob".into(),
                args: serde_json::json!({"pattern": "*"}),
            }],
            usage: Some(Usage {
                prompt_tokens: 42,
                completion_tokens: 15,
                total_tokens: 57,
            }),
            iterations: 2,
            error: None,
            session_id: None,
            task_id: None,
            worker_id: None,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, parsed);
    }

    #[test]
    fn round_trip_result_error() {
        let resp = IpcResponse::Result {
            id: "2".into(),
            status: ResultStatus::Error,
            response: None,
            tool_calls_made: vec![],
            usage: None,
            iterations: 0,
            error: Some(ErrorEnvelope {
                code: "provider_error".into(),
                message: "Model error".into(),
                retryable: true,
                details: None,
            }),
            session_id: None,
            task_id: None,
            worker_id: None,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, parsed);
    }

    #[test]
    fn round_trip_event_tool_start() {
        let resp = IpcResponse::Event {
            event: WorkerEvent::ToolStart {
                name: "glob".into(),
                args: serde_json::json!({"pattern": "*"}),
            },
            event_seq: 0,
            send_id: "2".into(),
            session_id: None,
            task_id: None,
            worker_id: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, parsed);
    }

    #[test]
    fn round_trip_event_error() {
        let resp = IpcResponse::Event {
            event: WorkerEvent::Error {
                message: "Model error".into(),
                code: "provider_error".into(),
                retryable: true,
                provider: Some("openrouter".into()),
                details: Some(serde_json::json!({"error": {"message": "fail"}})),
            },
            event_seq: 0,
            send_id: "2".into(),
            session_id: None,
            task_id: None,
            worker_id: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, parsed);
    }

    #[test]
    fn round_trip_event_heartbeat() {
        let resp = IpcResponse::Event {
            event: WorkerEvent::Heartbeat { duration_ms: 5000 },
            event_seq: 1,
            send_id: "2".into(),
            session_id: None,
            task_id: None,
            worker_id: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, parsed);
    }

    #[test]
    fn round_trip_event_context_prune() {
        let resp = IpcResponse::Event {
            event: WorkerEvent::ContextPrune {
                messages_pruned: 10,
                tokens_before: 50000,
                tokens_after: 30000,
            },
            event_seq: 2,
            send_id: "2".into(),
            session_id: None,
            task_id: None,
            worker_id: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, parsed);
    }

    // ── Golden fixture tests ──

    #[test]
    fn parse_normal_fixture_requests() {
        let content = fs::read_to_string(fixtures_dir().join("normal.in.jsonl")).unwrap();
        let requests = parse_jsonl_requests(&content);
        assert_eq!(requests.len(), 3);
        assert!(matches!(&requests[0], IpcRequest::Init { .. }));
        assert!(matches!(&requests[1], IpcRequest::Send { .. }));
        assert!(matches!(&requests[2], IpcRequest::Shutdown { .. }));
    }

    #[test]
    fn parse_normal_fixture_responses() {
        let content = fs::read_to_string(fixtures_dir().join("normal.out.jsonl")).unwrap();
        let responses = parse_jsonl_responses(&content);
        assert_eq!(responses.len(), 7);
        assert!(matches!(&responses[0], IpcResponse::InitOk { .. }));
        // events: tool_start, tool_end, content_delta, usage
        assert!(matches!(&responses[1], IpcResponse::Event { .. }));
        assert!(matches!(&responses[2], IpcResponse::Event { .. }));
        assert!(matches!(&responses[3], IpcResponse::Event { .. }));
        assert!(matches!(&responses[4], IpcResponse::Event { .. }));
        // result + shutdown_ok
        assert!(matches!(&responses[5], IpcResponse::Result { .. }));
        assert!(matches!(&responses[6], IpcResponse::ShutdownOk { .. }));
    }

    #[test]
    fn parse_error_fixture_requests() {
        let content = fs::read_to_string(fixtures_dir().join("error.in.jsonl")).unwrap();
        let requests = parse_jsonl_requests(&content);
        assert_eq!(requests.len(), 3);
    }

    #[test]
    fn parse_error_fixture_responses() {
        let content = fs::read_to_string(fixtures_dir().join("error.out.jsonl")).unwrap();
        let responses = parse_jsonl_responses(&content);
        assert_eq!(responses.len(), 4);
        // init_ok, error event, result(error), shutdown_ok
        assert!(matches!(&responses[0], IpcResponse::InitOk { .. }));
        assert!(matches!(
            &responses[1],
            IpcResponse::Event {
                event: WorkerEvent::Error { .. },
                ..
            }
        ));
        assert!(matches!(
            &responses[2],
            IpcResponse::Result {
                status: ResultStatus::Error,
                ..
            }
        ));
        assert!(matches!(&responses[3], IpcResponse::ShutdownOk { .. }));
    }

    #[test]
    fn parse_cancel_fixture_requests() {
        let content = fs::read_to_string(fixtures_dir().join("cancel.in.jsonl")).unwrap();
        let requests = parse_jsonl_requests(&content);
        assert_eq!(requests.len(), 4);
        assert!(matches!(&requests[2], IpcRequest::Cancel { .. }));
    }

    #[test]
    fn parse_cancel_fixture_responses() {
        let content = fs::read_to_string(fixtures_dir().join("cancel.out.jsonl")).unwrap();
        let responses = parse_jsonl_responses(&content);
        assert_eq!(responses.len(), 3);
        // init_ok, result(cancelled), shutdown_ok
        assert!(matches!(
            &responses[1],
            IpcResponse::Result {
                status: ResultStatus::Error,
                ..
            }
        ));
    }

    #[test]
    fn parse_version_mismatch_fixture() {
        let in_content =
            fs::read_to_string(fixtures_dir().join("version-mismatch.in.jsonl")).unwrap();
        let requests = parse_jsonl_requests(&in_content);
        assert_eq!(requests.len(), 1);

        let out_content =
            fs::read_to_string(fixtures_dir().join("version-mismatch.out.jsonl")).unwrap();
        let responses = parse_jsonl_responses(&out_content);
        assert_eq!(responses.len(), 1);
        assert!(matches!(
            &responses[0],
            IpcResponse::Result {
                status: ResultStatus::Error,
                ..
            }
        ));
    }

    #[test]
    fn parse_heartbeat_fixture() {
        let in_content = fs::read_to_string(fixtures_dir().join("heartbeat.in.jsonl")).unwrap();
        let requests = parse_jsonl_requests(&in_content);
        assert_eq!(requests.len(), 3);

        let out_content = fs::read_to_string(fixtures_dir().join("heartbeat.out.jsonl")).unwrap();
        let responses = parse_jsonl_responses(&out_content);
        assert_eq!(responses.len(), 8);
        assert!(matches!(&responses[0], IpcResponse::InitOk { .. }));
        assert!(matches!(
            &responses[1],
            IpcResponse::Event {
                event: WorkerEvent::Heartbeat { duration_ms: 5000 },
                ..
            }
        ));
        // result should have latency fields
        match &responses[6] {
            IpcResponse::Result {
                model_latency_ms,
                tool_latency_ms,
                total_latency_ms,
                ..
            } => {
                assert_eq!(*model_latency_ms, Some(4900));
                assert_eq!(*tool_latency_ms, Some(200));
                assert_eq!(*total_latency_ms, Some(5100));
            }
            other => panic!("Expected Result, got: {other:?}"),
        }
        assert!(matches!(&responses[7], IpcResponse::ShutdownOk { .. }));
    }

    #[test]
    fn normal_fixture_round_trips() {
        // Parse and re-serialize each fixture line, then parse again — should be equal
        let in_content = fs::read_to_string(fixtures_dir().join("normal.in.jsonl")).unwrap();
        for line in in_content.lines().filter(|l| !l.trim().is_empty()) {
            let req: IpcRequest = serde_json::from_str(line).unwrap();
            let reserialized = serde_json::to_string(&req).unwrap();
            let reparsed: IpcRequest = serde_json::from_str(&reserialized).unwrap();
            assert_eq!(req, reparsed, "Round-trip failed for request: {line}");
        }

        let out_content = fs::read_to_string(fixtures_dir().join("normal.out.jsonl")).unwrap();
        for line in out_content.lines().filter(|l| !l.trim().is_empty()) {
            let resp: IpcResponse = serde_json::from_str(line).unwrap();
            let reserialized = serde_json::to_string(&resp).unwrap();
            let reparsed: IpcResponse = serde_json::from_str(&reserialized).unwrap();
            assert_eq!(resp, reparsed, "Round-trip failed for response: {line}");
        }
    }

    #[test]
    fn ignores_unknown_fields_in_responses() {
        // Per compatibility.md: clients must ignore unknown fields
        let json = r#"{"type":"init_ok","id":"1","session_id":"s","protocol_version":"0.2.0","some_future_field":"value"}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(matches!(resp, IpcResponse::InitOk { .. }));
    }
}
