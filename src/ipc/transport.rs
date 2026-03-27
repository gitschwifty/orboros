use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};

use super::error::IpcError;
use super::types::{IpcRequest, IpcResponse};

/// Writes a single IPC request as a JSON line to the worker's stdin.
///
/// # Errors
///
/// Returns `IpcError::Parse` if serialization fails, or `IpcError::Write` if the write fails.
pub async fn write_request(stdin: &mut ChildStdin, request: &IpcRequest) -> Result<(), IpcError> {
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(IpcError::Write)?;
    stdin.flush().await.map_err(IpcError::Write)?;
    Ok(())
}

/// Reads a single IPC response as a JSON line from the worker's stdout.
/// Returns `None` if stdout is closed (EOF).
///
/// # Errors
///
/// Returns `IpcError::Read` if reading fails, or `IpcError::Parse` if the line is not valid JSON.
pub async fn read_response(
    reader: &mut BufReader<ChildStdout>,
) -> Result<Option<IpcResponse>, IpcError> {
    let mut line = String::new();
    let bytes_read = reader.read_line(&mut line).await.map_err(IpcError::Read)?;
    if bytes_read == 0 {
        return Ok(None);
    }
    let response: IpcResponse = serde_json::from_str(line.trim())?;
    Ok(Some(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::types::{InitConfig, PROTOCOL_VERSION};

    #[test]
    fn serialize_request_as_json_line() {
        let req = IpcRequest::Init {
            id: "1".into(),
            protocol_version: Some(PROTOCOL_VERSION.into()),
            config: InitConfig {
                model: "test".into(),
                system_prompt: "test".into(),
                tools: vec![],
                max_iterations: None,
                task_id: None,
                worker_id: None,
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"init""#));
        assert!(!json.contains('\n'));
    }

    #[test]
    fn deserialize_response_from_json_line() {
        let line = r#"{"type":"init_ok","id":"1","session_id":"s","protocol_version":"0.1.0"}"#;
        let resp: IpcResponse = serde_json::from_str(line).unwrap();
        assert!(matches!(resp, IpcResponse::InitOk { .. }));
    }
}
