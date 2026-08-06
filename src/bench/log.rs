//! Run-scoped benchmark CLI logging.
//!
//! The process-wide tracing subscriber writes to this sink in addition to the
//! terminal. Before a benchmark starts it drops records; `start` directs
//! subsequent records to that run's `cli.log` file.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::fmt::MakeWriter;

static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

fn log_file() -> &'static Mutex<Option<File>> {
    LOG_FILE.get_or_init(|| Mutex::new(None))
}

/// Starts recording CLI tracing output in `path`.
///
/// A CLI invocation runs one benchmark command, so replacing an existing sink
/// is intentional and makes this safe for command-handler tests.
pub fn start(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    *log_file()
        .lock()
        .expect("benchmark log file mutex poisoned") = Some(file);
    Ok(())
}

/// Writer factory installed on the process-wide tracing subscriber.
#[derive(Debug, Clone, Copy)]
pub struct BenchLogWriter;

impl<'a> MakeWriter<'a> for BenchLogWriter {
    type Writer = BenchLogGuard;

    fn make_writer(&'a self) -> Self::Writer {
        BenchLogGuard
    }
}

/// A lightweight proxy that locks only while a formatted event is written.
pub struct BenchLogGuard;

impl Write for BenchLogGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(file) = log_file()
            .lock()
            .expect("benchmark log file mutex poisoned")
            .as_mut()
        {
            file.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = log_file()
            .lock()
            .expect("benchmark log file mutex poisoned")
            .as_mut()
        {
            file.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_appends_to_the_selected_run_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cli.log");
        start(&path).unwrap();
        let mut writer = BenchLogWriter.make_writer();
        writer.write_all(b"hello\\n").unwrap();
        writer.flush().unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "hello\\n");
    }
}
