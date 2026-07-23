# Orboros/Heddle IPC Compatibility Policy

## Protocol Versioning
- Protocol version is independent of package versions.
- Store it in `PROTOCOL_VERSION` (single line, e.g. `0.1.0`).
- `Init` may include an expected `protocol_version`.
- `InitOk` must include the actual `protocol_version`.

## Compatibility Rules
- **MAJOR**: breaking changes only.
  - Removing or renaming a required field.
  - Adding a required field.
  - Changing field meaning or type.
- **MINOR**: additive optional fields or new event types.
- **PATCH**: bug fixes, no schema shape changes.

## Field Naming
- IPC fields are **snake_case**.
- In Rust, use `#[serde(rename_all = "snake_case")]` to keep internal names idiomatic.

## Base IPC Types

### Requests
- `Init`
- `Send`
- `Status`
- `Shutdown`
- `Cancel`

### Responses/Events
- `InitOk`
- `Event` (`content_delta`, `tool_start`, `tool_end`, `usage`, `routed_model`, `error`, `heartbeat`, `context_prune`, `context_compact`, `context_handoff`, `permission_request`, `permission_denied`, `plan_complete`)
- `Result`
- `StatusOk`
- `ShutdownOk`

## Changelog

### 0.4.0

**Summary:** Add isolated headless runtime placement, routing metadata, and
structured worker failure details for benchmark artifact capture.

**From 0.3.0:**
- `InitConfig` gains optional `runtime` (`mode`, `state_root`,
  `transcript_path`, `inherit_ambient_config`) and `routing` metadata.
- `InitOk`, `StatusOk`, and `Result` gain optional effective `runtime` and
  `routing` metadata.
- `Result` gains optional `failure` (`code`, `termination_reason`, iteration
  and tool-call counts, and the last tool name).
- Minor protocol versions within the same major version are compatible;
  Orboros logs a warning rather than rejecting a worker solely for a minor
  mismatch.

### 0.3.0

**Summary:** Add routed-model metadata for router aliases such as `openrouter/free`, plus optional app attribution for embedded headless clients.

**From 0.2.0:**
- `routed_model` WorkerEvent (`model`) may be emitted during a send when the provider reports the concrete model that served the response.
- `StatusOk` gains optional `last_routed_model`; it is omitted until the provider reports a routed model.
- `InitConfig` gains optional `app_attribution` (`referer`, `title`, optional `categories`) so headless clients can set provider dashboard attribution. It is ignored unless both `referer` and `title` are present.
- `usage` events and result summaries gain optional `cost_micros`, `cost_currency`, token detail fields, and `generation_id`.

**Compatibility:** Additive only. Existing 0.x clients with the same MAJOR version remain compatible if they ignore unknown event types and unknown optional fields as required by this policy.

### 0.2.0

**Summary:** All Batch 4 integration hardening changes (tasks 11–14). Adds protocol hardening, cancel/heartbeat, correlation IDs, latency tracking, and context transition events.

**From 0.1.0:**
- `event_seq` (monotonic, 0-based per send) and `send_id` on all Event responses
- Structured `ErrorEnvelope` (`{ code, message, retryable, details? }`) replaces flat error strings on Result and InitOk
- `heartbeat` WorkerEvent — emitted at fixed interval during active sends
- `cancel` request aborts in-progress tools via AbortSignal threading
- `InitConfig` gains optional `task_id`, `worker_id`
- Event responses gain optional `session_id`, `task_id`, `worker_id` (correlation IDs)
- Result gains optional `session_id`, `task_id`, `worker_id`, `model_latency_ms`, `tool_latency_ms`, `total_latency_ms`
- `context_prune` WorkerEvent (`messages_pruned`, `tokens_before`, `tokens_after`)
- `context_compact`, `context_handoff` WorkerEvent placeholders (schema only, not emitted yet)
- `HeddleTool.execute` gains optional `signal` param (AbortSignal)

**Error codes:** `provider_error` (retryable), `protocol_error`, `protocol_version_mismatch`, `tool_error`, `loop_detected`, `cancelled` (all non-retryable).

### 0.1.0 (protocol-hardening)
- **Event responses** now include `event_seq` (monotonic counter, 0-based per send) and `send_id` (mirrors the originating send request `id`).
- **Result error** changed from `error?: string` to `error?: { code, message, retryable, details? }` (ErrorEnvelope).
- **InitOk error** changed from `error?: string` to `error?: ErrorEnvelope`.
- **WorkerEvent error variant** changed: `error` field renamed to `message`, `code` now required, `retryable` (boolean) added.
- **Error codes**: `provider_error` (retryable), `protocol_error`, `protocol_version_mismatch`, `tool_error`, `loop_detected`, `cancelled` (all non-retryable).
- *Note: 0.1.0 was never released independently; all changes are included in 0.2.0.*

## Forward/Backward Handling
- Clients must ignore unknown fields.
- Required fields must not be removed within a major version.
- New event types are allowed in MINOR versions; clients should treat unknown events as `Event::Unknown` and continue.

## Contract Tests
- Golden transcripts are the source of truth for expected behavior.
- Tests should be **strict line-by-line** with an allowlist of non-deterministic fields.
- JSON Schema from TypeBox may be used for docs and for fixture validation in tests.
- Avoid strict schema diffing between TypeBox and Rust generators; rely on fixtures + schema validation.
- Any IPC change must update:
  - JSON Schema
  - `PROTOCOL_VERSION`
  - Golden transcripts (normal + error + cancel flow)

## Rollout
- Bump version in Heddle first.
- Add parsing + handling in Orboros.
- Update transcripts and re-run contract tests.
