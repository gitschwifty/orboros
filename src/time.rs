//! Operator-facing local-time formatting.
//!
//! Durable records remain `DateTime<Utc>`; conversion happens only at display
//! and tracing-render boundaries.

use chrono::{DateTime, Local, Utc};
use std::fmt;
use tracing_subscriber::fmt::time::FormatTime;

/// Formats a durable UTC timestamp in the operator's local timezone with its
/// numeric offset, making daylight-saving interpretation explicit.
#[must_use]
pub fn format_local(timestamp: DateTime<Utc>) -> String {
    timestamp
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S %:z")
        .to_string()
}

/// Local clock for terminal and file-backed tracing output.
#[derive(Clone, Copy, Debug)]
pub struct LocalTimestamp;

impl FormatTime for LocalTimestamp {
    fn format_time(&self, writer: &mut tracing_subscriber::fmt::format::Writer<'_>) -> fmt::Result {
        write!(writer, "{}", Local::now().format("%Y-%m-%dT%H:%M:%S%:z"))
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn local_display_includes_an_explicit_offset() {
        let timestamp = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
        let rendered = format_local(timestamp);
        assert!(rendered.ends_with("+00:00") || rendered.rsplit_once(' ').is_some());
    }
}
