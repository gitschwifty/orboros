use serde::{Deserialize, Serialize};

// ── Protocol version ──

pub const PROTOCOL_VERSION: &str = "0.5.0";

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_attribution: Option<AppAttribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimePlacementConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppAttribution {
    pub referer: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<String>,
}

/// Requested placement for Heddle's per-worker runtime files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    Default,
    Isolated,
}

/// Feature advertisement returned by Heddle during the 0.5 handshake.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IpcCapabilities {
    pub enabled_tools: Vec<String>,
    pub explicit_tool_allowlist: bool,
    pub runtime_modes: Vec<RuntimeMode>,
    pub transcript_placement: bool,
    pub failure_details_version: String,
    pub routing_request_metadata: bool,
    pub effective_routing_metadata: bool,
    pub cache_usage_metrics: bool,
    pub cancellation: bool,
    pub turn_state_events: bool,
}

/// Safe reproducibility identity returned for a worker session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileIdentity {
    pub fingerprint: String,
    pub model: String,
}

/// Optional runtime placement supplied during worker initialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimePlacementConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RuntimeMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    /// Optional Heddle headless configuration file. Orboros resolves this
    /// through its global/project config layering and passes it only to the
    /// worker runtime, never into prompts or execution records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit_ambient_config: Option<bool>,
}

/// Requested or effective provider-routing metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grouping_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_model: Option<String>,
    /// Provider actually observed in a response, distinct from the requested
    /// routing provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_upstream_provider: Option<String>,
    /// Providers observed across a routed request. Empty for older workers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstream_provider_history: Vec<String>,
}

/// Routing facts observed by Heddle, distinct from caller-requested routing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EffectiveRoutingMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstream_provider_history: Vec<String>,
}

/// Effective runtime placement reported by Heddle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EffectiveRuntimeMetadata {
    pub mode: RuntimeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_root: Option<String>,
    pub transcript_path: String,
}

/// Structured termination data reported for a failed worker result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailureDetails {
    pub code: String,
    pub termination_reason: String,
    pub iterations: u32,
    pub tool_calls_made: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<ToolCallSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_threshold: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderFailureDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionFailureDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub malformed_tool_call: Option<ToolCallSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_source: Option<CancellationSource>,
}

/// Non-secret provider facts retained for transport-failure attribution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderFailureDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionFailureDetails {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CancellationSource {
    User,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime: Option<EffectiveRuntimeMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        routing: Option<RoutingMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_routing: Option<RoutingMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effective_routing: Option<EffectiveRoutingMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<IpcCapabilities>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<ProfileIdentity>,
    },
    Event {
        event: WorkerEvent,
        #[serde(default)]
        event_seq: u64,
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
        /// Worker-reported confidence in the result (0.0–1.0).
        /// Optional and forward-compatible — older heddle workers
        /// won't send it. A fallback parser in the orchestrator
        /// also extracts `CONFIDENCE: X.XX` from the response body.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime: Option<EffectiveRuntimeMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        routing: Option<RoutingMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_routing: Option<RoutingMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effective_routing: Option<EffectiveRoutingMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<FailureDetails>,
    },
    StatusOk {
        id: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_routed_model: Option<String>,
        messages_count: u64,
        session_id: String,
        active: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime: Option<EffectiveRuntimeMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        routing: Option<RoutingMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_routing: Option<RoutingMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effective_routing: Option<EffectiveRoutingMetadata>,
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
    TurnState {
        state: TurnStateEvent,
    },
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
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_micros: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_currency: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cached_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_write_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation_id: Option<String>,
    },
    RoutedModel {
        model: String,
    },
    /// Provider observed by the upstream transport for the current request.
    /// Heddle emits this independently of the requested routing metadata.
    UpstreamProvider {
        provider: String,
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
    PermissionRequest {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    PermissionDenied {
        name: String,
        reason: String,
    },
    PlanComplete {
        plan: String,
    },
    Heartbeat {
        duration_ms: u64,
    },
    ContextPrune {
        messages_pruned: u64,
        tokens_before: u64,
        tokens_after: u64,
    },
    ContextCompact {},
    ContextHandoff {},
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnStateEvent {
    Queued,
    Running,
    Cancelling,
    Completed,
}

// ── Shared types ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRecord {
    pub name: String,
    pub args: serde_json::Value,
}

/// Final-tool or malformed-call evidence carried by a 0.5 failure result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCallSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_currency: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
}

#[must_use]
pub fn u64_to_u32_saturating(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
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
                app_attribution: Some(AppAttribution {
                    referer: "https://github.com/gitschwifty/orboros".into(),
                    title: "Orboros".into(),
                    categories: Some("cli-agent".into()),
                }),
                runtime: None,
                routing: None,
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, parsed);
    }

    #[test]
    fn init_runtime_serializes_optional_config_path() {
        let runtime = RuntimePlacementConfig {
            mode: Some(RuntimeMode::Isolated),
            state_root: Some("/tmp/state".into()),
            transcript_path: Some("/tmp/transcript.jsonl".into()),
            config_path: Some("/Users/test/.orboros/heddle-config.toml".into()),
            inherit_ambient_config: Some(false),
        };
        let json = serde_json::to_value(&runtime).unwrap();
        assert_eq!(
            json["config_path"],
            "/Users/test/.orboros/heddle-config.toml"
        );
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
            protocol_version: Some("0.3.0".into()),
            error: None,
            runtime: None,
            routing: None,
            requested_routing: None,
            effective_routing: None,
            capabilities: None,
            profile: None,
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
                cost_micros: Some(123),
                cost_currency: Some("USD".into()),
                cached_tokens: Some(4),
                cache_write_tokens: Some(5),
                reasoning_tokens: Some(6),
                generation_id: Some("chatcmpl-test".into()),
            }),
            iterations: 2,
            error: None,
            session_id: None,
            task_id: None,
            worker_id: None,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
            confidence: Some(0.85),
            runtime: None,
            routing: None,
            requested_routing: None,
            effective_routing: None,
            failure: None,
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
            confidence: None,
            runtime: None,
            routing: None,
            requested_routing: None,
            effective_routing: None,
            failure: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, parsed);
    }

    #[test]
    fn parses_v050_provider_failure_details() {
        let response: IpcResponse = serde_json::from_str(
            r#"{"type":"result","id":"2","status":"error","tool_calls_made":[],"iterations":3,"failure":{"code":"provider_error","termination_reason":"stream decode failed","iterations":3,"tool_calls_made":2,"last_tool":{"id":"call-1","name":"read_file","args":{"path":"src/lib.rs"}},"provider":{"name":"openrouter","status":502,"status_category":"server_error","retry_after_ms":250,"error_type":"upstream","provider_code":"bad_gateway"}}}"#,
        )
        .unwrap();
        let IpcResponse::Result {
            failure: Some(failure),
            ..
        } = response
        else {
            panic!("expected result failure");
        };
        assert_eq!(failure.provider.as_ref().and_then(|p| p.status), Some(502));
        assert_eq!(
            failure.provider.as_ref().and_then(|p| p.retry_after_ms),
            Some(250)
        );
        assert_eq!(
            failure.last_tool.as_ref().map(|tool| tool.name.as_str()),
            Some("read_file")
        );
    }

    #[test]
    fn parses_v050_init_capabilities_and_turn_state() {
        let init: IpcResponse = serde_json::from_str(
            r#"{"type":"init_ok","id":"1","session_id":"s","protocol_version":"0.5.0","capabilities":{"enabled_tools":["read_file"],"explicit_tool_allowlist":true,"runtime_modes":["default","isolated"],"transcript_placement":true,"failure_details_version":"v2","routing_request_metadata":true,"effective_routing_metadata":true,"cache_usage_metrics":true,"cancellation":true,"turn_state_events":true},"profile":{"fingerprint":"abc","model":"test/model"}}"#,
        )
        .unwrap();
        assert!(matches!(
            init,
            IpcResponse::InitOk {
                capabilities: Some(_),
                ..
            }
        ));
        let event: IpcResponse = serde_json::from_str(
            r#"{"type":"event","event":{"event":"turn_state","state":"running"}}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            IpcResponse::Event {
                event: WorkerEvent::TurnState {
                    state: TurnStateEvent::Running
                },
                ..
            }
        ));
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
    fn round_trip_event_usage_with_metadata() {
        let resp = IpcResponse::Event {
            event: WorkerEvent::Usage {
                prompt_tokens: 42,
                completion_tokens: 15,
                total_tokens: 57,
                cost_micros: Some(123),
                cost_currency: Some("USD".into()),
                cached_tokens: Some(4),
                cache_write_tokens: Some(5),
                reasoning_tokens: Some(6),
                generation_id: Some("chatcmpl-test".into()),
            },
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
    fn round_trip_event_routed_model() {
        let resp = IpcResponse::Event {
            event: WorkerEvent::RoutedModel {
                model: "anthropic/claude-haiku-4.5".into(),
            },
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
    fn parses_heddle_upstream_provider_event() {
        let response: IpcResponse = serde_json::from_str(
            r#"{"type":"event","event":{"event":"upstream_provider","provider":"openai"},"event_seq":1,"send_id":"2"}"#,
        )
        .unwrap();
        assert!(matches!(
            response,
            IpcResponse::Event {
                event: WorkerEvent::UpstreamProvider { provider },
                ..
            } if provider == "openai"
        ));
    }

    #[test]
    fn parses_heddle_effective_routing_metadata() {
        let routing: RoutingMetadata = serde_json::from_str(
            r#"{"upstream_provider":"openrouter","effective_upstream_provider":"openai","upstream_provider_history":["openai"]}"#,
        )
        .unwrap();
        assert_eq!(
            routing.effective_upstream_provider.as_deref(),
            Some("openai")
        );
        assert_eq!(routing.upstream_provider_history, ["openai"]);
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
    fn all_fixtures_round_trip() {
        // Every fixture file should parse → serialize → parse back identically
        let fixtures = &["normal", "error", "cancel", "version-mismatch", "heartbeat"];
        for name in fixtures {
            let in_path = fixtures_dir().join(format!("{name}.in.jsonl"));
            for line in fs::read_to_string(&in_path)
                .unwrap()
                .lines()
                .filter(|l| !l.trim().is_empty())
            {
                let req: IpcRequest = serde_json::from_str(line).unwrap();
                let reserialized = serde_json::to_string(&req).unwrap();
                let reparsed: IpcRequest = serde_json::from_str(&reserialized).unwrap();
                assert_eq!(
                    req, reparsed,
                    "Round-trip failed for {name} request: {line}"
                );
            }

            let out_path = fixtures_dir().join(format!("{name}.out.jsonl"));
            for line in fs::read_to_string(&out_path)
                .unwrap()
                .lines()
                .filter(|l| !l.trim().is_empty())
            {
                let resp: IpcResponse = serde_json::from_str(line).unwrap();
                let reserialized = serde_json::to_string(&resp).unwrap();
                let reparsed: IpcResponse = serde_json::from_str(&reserialized).unwrap();
                assert_eq!(
                    resp, reparsed,
                    "Round-trip failed for {name} response: {line}"
                );
            }
        }
    }

    #[test]
    fn ignores_unknown_fields_in_responses() {
        // Per compatibility.md: clients must ignore unknown fields
        let json = r#"{"type":"init_ok","id":"1","session_id":"s","protocol_version":"0.3.0","some_future_field":"value"}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(matches!(resp, IpcResponse::InitOk { .. }));
    }
}
