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
- `Event` (`content_delta`, `tool_start`, `tool_end`, `usage`, `error`)
- `Result`
- `StatusOk`
- `ShutdownOk`

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
