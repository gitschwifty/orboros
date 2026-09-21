//! Local supervisor ownership primitives.
//!
//! Shared project state has one authoritative writer.  The owner is established
//! with an OS-held lease rather than a PID file, which is only diagnostic and
//! can be stale after a crash.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::config::ProjectEntry;
use orbs::dep::DepEdge;
use orbs::dep_store::DepWriteSink;
use orbs::orb::Orb;
use orbs::orb_store::OrbWriteSink;

/// Metadata written by the process that currently owns a project state lease.
///
/// This is diagnostic information only. The held file descriptor is the source
/// of authority, so callers must never infer that a listed PID may be killed or
/// that a lease may be stolen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectLeaseMetadata {
    pub project_id: String,
    pub pid: u32,
    pub epoch: u64,
}

/// Failures while acquiring or maintaining a project state writer lease.
#[derive(Debug, Error)]
pub enum ProjectLeaseError {
    #[error("failed to create shared state directory {path}: {source}")]
    CreateStateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to open project state lease {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("project state is already owned ({path})")]
    Busy {
        path: PathBuf,
        owner: Option<ProjectLeaseMetadata>,
    },
    #[error("failed to lock project state lease {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write project state lease metadata {path}: {source}")]
    WriteMetadata {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize project state lease metadata {path}: {source}")]
    SerializeMetadata {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// A client mutation accepted by the project state authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateOperation {
    pub operation_id: uuid::Uuid,
    pub base_revision: u64,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// Durable outcome of an accepted mutation request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateReceipt {
    pub operation: StateOperation,
    pub revision: u64,
}

/// A state change understood by the shared-state projection.
///
/// The operation journal is the durable source of truth.  The JSONL stores
/// are materialized projections, kept in the existing format so workers and
/// read-only tooling can consume them without a second storage API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SharedStateMutation {
    /// Create or replace the latest record for one orb.
    UpsertOrb { orb: Orb },
    /// Operator retry intent, including the full failed snapshot in the audit event.
    ResetOrb {
        orb: Orb,
        event: orbs::audit::AuditEvent,
    },
    /// Create or replace the latest record for one dependency edge.
    UpsertDependency { edge: DepEdge },
    /// Apply several related changes as one authoritative operation. This is
    /// used for composite intents such as creating a decomposed plan, so a
    /// reader never observes only part of that intent.
    Batch { mutations: Vec<SharedStateMutation> },
}

impl StateOperation {
    /// Builds an operation carrying one typed shared-state mutation.
    #[must_use]
    pub fn mutation(
        operation_id: uuid::Uuid,
        base_revision: u64,
        mutation: SharedStateMutation,
    ) -> Self {
        let payload =
            serde_json::to_value(mutation).expect("shared state mutation is serializable");
        let kind = payload
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .expect("tagged shared state mutation has a kind")
            .to_owned();
        Self {
            operation_id,
            base_revision,
            kind,
            payload,
        }
    }

    fn shared_mutation(&self) -> Result<SharedStateMutation, StateProjectionError> {
        serde_json::from_value(self.payload.clone()).map_err(|source| {
            StateProjectionError::InvalidMutation {
                operation_id: self.operation_id,
                kind: self.kind.clone(),
                source,
            }
        })
    }
}

/// Failures while opening or appending the replayable operation journal.
#[derive(Debug, Error)]
pub enum OperationJournalError {
    #[error("failed to open operation journal {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read operation journal {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid operation journal row in {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize operation journal row for {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to append operation journal {path}: {source}")]
    Append {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Failures while rebuilding the materialized JSONL state from the journal.
#[derive(Debug, Error)]
pub enum StateProjectionError {
    #[error("failed to read legacy orb projection: {0}")]
    ReadLegacyOrb(#[source] io::Error),
    #[error("failed to read legacy dependency projection: {0}")]
    ReadLegacy(String),
    #[error("operation {operation_id} has invalid {kind} mutation payload: {source}")]
    InvalidMutation {
        operation_id: uuid::Uuid,
        kind: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize materialized state record in {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write materialized state record in {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read projection watermark {path}: {source}")]
    ReadWatermark {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid projection watermark {path}: {source}")]
    ParseWatermark {
        path: PathBuf,
        #[source]
        source: std::num::ParseIntError,
    },
}

/// Failures exposed by the project state authority.
#[derive(Debug, Error)]
pub enum ProjectAuthorityError {
    #[error(transparent)]
    Lease(#[from] ProjectLeaseError),
    #[error(transparent)]
    Journal(#[from] OperationJournalError),
    #[error(transparent)]
    Projection(#[from] StateProjectionError),
    #[error("operation {operation_id} is based on revision {base_revision}, but current revision is {current_revision}")]
    RevisionConflict {
        operation_id: uuid::Uuid,
        base_revision: u64,
        current_revision: u64,
    },
}

/// Append-only journal which deduplicates retried operations by UUID.
#[derive(Debug)]
pub struct OperationJournal {
    path: PathBuf,
    receipts: HashMap<uuid::Uuid, StateReceipt>,
    revision: u64,
}

impl OperationJournal {
    /// Opens the journal and rebuilds its idempotency index from durable rows.
    pub fn open(state_dir: &Path) -> Result<Self, OperationJournalError> {
        let path = state_dir.join("operations.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|source| OperationJournalError::Open {
                path: path.clone(),
                source,
            })?;
        let mut receipts = HashMap::new();
        let mut revision = 0;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|source| OperationJournalError::Read {
                path: path.clone(),
                source,
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let receipt: StateReceipt =
                serde_json::from_str(&line).map_err(|source| OperationJournalError::Parse {
                    path: path.clone(),
                    source,
                })?;
            revision = revision.max(receipt.revision);
            receipts.insert(receipt.operation.operation_id, receipt);
        }
        Ok(Self {
            path,
            receipts,
            revision,
        })
    }

    /// Returns the last assigned authoritative revision.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns a receipt for a previously accepted operation.
    pub fn receipt(&self, operation_id: &uuid::Uuid) -> Option<&StateReceipt> {
        self.receipts.get(operation_id)
    }

    /// Returns receipts in the exact order in which they were accepted.
    pub fn receipts_after(&self, revision: u64) -> Vec<StateReceipt> {
        let mut receipts = self
            .receipts
            .values()
            .filter(|receipt| receipt.revision > revision)
            .cloned()
            .collect::<Vec<_>>();
        receipts.sort_by_key(|receipt| receipt.revision);
        receipts
    }

    /// Records one operation once, returning the original receipt on retry.
    pub fn accept(
        &mut self,
        operation: StateOperation,
    ) -> Result<StateReceipt, OperationJournalError> {
        if let Some(receipt) = self.receipts.get(&operation.operation_id) {
            return Ok(receipt.clone());
        }
        self.revision += 1;
        let receipt = StateReceipt {
            operation,
            revision: self.revision,
        };
        let row =
            serde_json::to_vec(&receipt).map_err(|source| OperationJournalError::Serialize {
                path: self.path.clone(),
                source,
            })?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|source| OperationJournalError::Open {
                path: self.path.clone(),
                source,
            })?;
        file.write_all(&row)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_data())
            .map_err(|source| OperationJournalError::Append {
                path: self.path.clone(),
                source,
            })?;
        self.receipts
            .insert(receipt.operation.operation_id, receipt.clone());
        Ok(receipt)
    }
}

/// An exclusive, OS-held writer lease for one user-local project state home.
///
/// Dropping the lease releases the operating-system lock. If the owning process
/// crashes, the OS releases it automatically, allowing a subsequent supervisor
/// to recover the journal and become the next owner.
#[derive(Debug)]
pub struct ProjectStateLease {
    file: File,
    path: PathBuf,
    metadata: ProjectLeaseMetadata,
}

/// A short-lived exclusive lease for a standalone CLI mutation.
///
/// Shared-state projects always submit mutations to the daemon's authoritative
/// writer. This lease covers the remaining local-store case: it is held across
/// a whole mutating `orb` command so two CLI processes cannot interleave their
/// read/modify/write sequences or append competing JSONL projections.
#[derive(Debug)]
pub struct LocalMutationLease {
    file: File,
    path: PathBuf,
}

impl LocalMutationLease {
    /// Acquires the standalone mutation lease without waiting.
    pub fn try_acquire(state_dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("cli-mutation.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "project state is being modified by another CLI ({}) — retry when it finishes",
                        path.display()
                    ),
                ));
            }
            return Err(error);
        }
        Ok(Self { file, path })
    }

    /// Returns the lock path for diagnostics and tests.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LocalMutationLease {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// One project attached to the user-level Orboros supervisor.
///
/// The authority is intentionally separate from worker scheduling: it can own
/// serialized shared state even when the project has no configured worker
/// dispatch capability.
#[derive(Debug)]
pub struct ProjectStateAuthority {
    project: ProjectEntry,
    state_dir: PathBuf,
    lease: ProjectStateLease,
    journal: OperationJournal,
}

/// The project registry owned by one user-level supervisor process.
pub struct LocalSupervisor {
    home: PathBuf,
    next_epoch: u64,
    projects: HashMap<String, ProjectStateAuthority>,
    queues: HashMap<String, crate::queue_loop::QueueLoop>,
    dispatch: HashMap<String, Option<crate::daemon::DispatchSettings>>,
}

/// Result of an idempotent project attachment request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachOutcome {
    Attached,
    AlreadyAttached,
}

/// A request accepted by the user-level supervisor control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SupervisorRequest {
    Attach {
        project: ProjectEntry,
    },
    Detach {
        project_name: String,
    },
    Status {
        project_name: String,
    },
    Mutate {
        project_name: String,
        operation: StateOperation,
    },
}

/// A typed response returned by the supervisor control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SupervisorResponse {
    Attached { outcome: AttachOutcomeWire },
    Detached { detached: bool },
    Status { revision: u64 },
    Accepted { receipt: StateReceipt },
    Rejected { code: String, detail: String },
}

/// Wire-safe representation of the attachment outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachOutcomeWire {
    Attached,
    AlreadyAttached,
}

impl From<AttachOutcome> for AttachOutcomeWire {
    fn from(value: AttachOutcome) -> Self {
        match value {
            AttachOutcome::Attached => Self::Attached,
            AttachOutcome::AlreadyAttached => Self::AlreadyAttached,
        }
    }
}

/// Serves newline-delimited JSON supervisor requests on a local Unix socket.
pub async fn serve_local_socket(
    listener: UnixListener,
    supervisor: Arc<Mutex<LocalSupervisor>>,
) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, supervisor).await {
                tracing::warn!(%error, "supervisor client connection failed");
            }
        });
    }
}

/// Location of the one user-level supervisor control socket.
#[must_use]
pub fn control_socket_path(home: &Path) -> PathBuf {
    home.join(".orboros").join("supervisor.sock")
}

/// Sends one request to the running local supervisor and returns its response.
///
/// A missing or unreachable socket is deliberately surfaced to callers rather
/// than falling back to direct JSONL writes: that is what keeps shared-mode
/// clients from accidentally bypassing the authoritative writer.
pub async fn request_local_socket(
    socket_path: &Path,
    request: &SupervisorRequest,
) -> io::Result<SupervisorResponse> {
    let stream = UnixStream::connect(socket_path).await?;
    let (reader, mut writer) = stream.into_split();
    let encoded = serde_json::to_vec(request).map_err(io::Error::other)?;
    writer.write_all(&encoded).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    let mut lines = AsyncBufReader::new(reader).lines();
    let line = lines.next_line().await?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::UnexpectedEof, "supervisor closed response")
    })?;
    serde_json::from_str(&line).map_err(io::Error::other)
}

/// Blocking writer used by the synchronous CLI and queue storage APIs.
///
/// It is installed only for shared state. Every write discovers the current
/// revision then submits a typed operation; a concurrent client causes a
/// bounded retry with a fresh revision rather than a direct JSONL fallback.
#[derive(Debug, Clone)]
pub struct SupervisorStoreWriter {
    socket_path: PathBuf,
    project_name: String,
}

impl SupervisorStoreWriter {
    #[must_use]
    pub fn new(socket_path: PathBuf, project_name: String) -> Self {
        Self {
            socket_path,
            project_name,
        }
    }

    fn submit(
        &self,
        mutation: &SharedStateMutation,
        base_revision: Option<u64>,
    ) -> io::Result<u64> {
        const MAX_CONFLICT_RETRIES: u8 = 4;
        for _ in 0..MAX_CONFLICT_RETRIES {
            let revision = match base_revision {
                Some(revision) => revision,
                None => self.snapshot_revision()?,
            };
            let operation =
                StateOperation::mutation(uuid::Uuid::new_v4(), revision, mutation.clone());
            match self.request(&SupervisorRequest::Mutate {
                project_name: self.project_name.clone(),
                operation,
            })? {
                SupervisorResponse::Accepted { receipt } => return Ok(receipt.revision),
                SupervisorResponse::Rejected { code, .. }
                    if code == "revision_conflict" && base_revision.is_none() => {}
                SupervisorResponse::Rejected { code, detail } if code == "revision_conflict" => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("shared state changed since it was read: {detail}"),
                    ));
                }
                SupervisorResponse::Rejected { code, detail } => {
                    return Err(io::Error::other(format!(
                        "supervisor rejected mutation ({code}): {detail}"
                    )));
                }
                other => {
                    return Err(io::Error::other(format!(
                        "unexpected supervisor mutation response: {other:?}"
                    )))
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "shared state remained contended after four retries",
        ))
    }

    /// Prepare against a captured revision; reject concurrent state changes.
    pub fn reset_orb(
        &self,
        store: &orbs::orb_store::OrbStore,
        id: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        let revision = self.snapshot_revision()?;
        let (orb, event) = crate::orb_cmd::prepare_reset(store, id, reason)?;
        self.submit(
            &SharedStateMutation::ResetOrb { orb, event },
            Some(revision),
        )?;
        Ok(())
    }

    /// Submit related mutations as one revisioned, replayable operation.
    pub fn submit_batch(&self, mutations: Vec<SharedStateMutation>) -> io::Result<u64> {
        self.submit(&SharedStateMutation::Batch { mutations }, None)
    }

    fn request(&self, request: &SupervisorRequest) -> io::Result<SupervisorResponse> {
        let mut stream = StdUnixStream::connect(&self.socket_path).map_err(|error| {
            if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) {
                io::Error::new(
                    error.kind(),
                    format!(
                        "shared state for project {} requires its running daemon; start `orboros daemon` or wait for it to finish draining ({})",
                        self.project_name,
                        self.socket_path.display()
                    ),
                )
            } else {
                error
            }
        })?;
        let row = serde_json::to_vec(&request).map_err(io::Error::other)?;
        stream.write_all(&row)?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response)?;
        if response.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "supervisor closed response",
            ));
        }
        serde_json::from_str(&response).map_err(io::Error::other)
    }

    fn snapshot_revision(&self) -> io::Result<u64> {
        match self.request(&SupervisorRequest::Status {
            project_name: self.project_name.clone(),
        })? {
            SupervisorResponse::Status { revision } => Ok(revision),
            SupervisorResponse::Rejected { code, detail } => Err(io::Error::other(format!(
                "supervisor rejected status ({code}): {detail}"
            ))),
            other => Err(io::Error::other(format!(
                "unexpected supervisor status response: {other:?}"
            ))),
        }
    }
}

impl OrbWriteSink for SupervisorStoreWriter {
    fn snapshot_revision(&self) -> io::Result<u64> {
        SupervisorStoreWriter::snapshot_revision(self)
    }

    fn write_orb(&self, orb: &Orb, base_revision: Option<u64>) -> io::Result<u64> {
        self.submit(
            &SharedStateMutation::UpsertOrb { orb: orb.clone() },
            base_revision,
        )
    }
}

impl DepWriteSink for SupervisorStoreWriter {
    fn snapshot_revision(&self) -> io::Result<u64> {
        SupervisorStoreWriter::snapshot_revision(self)
    }

    fn write_edge(&self, edge: &DepEdge, base_revision: Option<u64>) -> io::Result<u64> {
        self.submit(
            &SharedStateMutation::UpsertDependency { edge: edge.clone() },
            base_revision,
        )
    }
}

async fn serve_connection(
    stream: UnixStream,
    supervisor: Arc<Mutex<LocalSupervisor>>,
) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = AsyncBufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<SupervisorRequest>(&line) {
            Ok(request) => supervisor.lock().await.handle(request),
            Err(error) => SupervisorResponse::Rejected {
                code: "invalid_request".into(),
                detail: error.to_string(),
            },
        };
        let encoded = serde_json::to_vec(&response).map_err(io::Error::other)?;
        writer.write_all(&encoded).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

impl LocalSupervisor {
    /// Creates an empty local supervisor registry rooted at this user's home.
    #[must_use]
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            next_epoch: 1,
            projects: HashMap::new(),
            queues: HashMap::new(),
            dispatch: HashMap::new(),
        }
    }

    /// Idempotently attaches a project and acquires its writer lease.
    pub fn attach(
        &mut self,
        project: ProjectEntry,
    ) -> Result<AttachOutcome, ProjectAuthorityError> {
        if self.projects.contains_key(&project.name) {
            return Ok(AttachOutcome::AlreadyAttached);
        }
        let authority =
            ProjectStateAuthority::attach(&self.home, project.clone(), self.next_epoch)?;
        self.next_epoch += 1;
        tracing::info!(
            project = %project.name,
            project_root = %project.runnable_path().map_or_else(|| Path::new("-").display().to_string(), |path| path.display().to_string()),
            state_dir = %authority.state_dir().display(),
            "shared supervisor project attached"
        );
        self.projects.insert(project.name, authority);
        Ok(AttachOutcome::Attached)
    }

    /// Detaches a project and releases its writer lease.
    pub fn detach(&mut self, project_name: &str) -> Option<ProjectStateAuthority> {
        if let Some(queue) = self.queues.remove(project_name) {
            queue.stop();
        }
        self.dispatch.remove(project_name);
        let detached = self.projects.remove(project_name);
        if let Some(authority) = &detached {
            tracing::info!(
                project = project_name,
                state_dir = %authority.state_dir().display(),
                "shared supervisor project detached"
            );
        }
        detached
    }

    /// Returns an attached project authority by registered project name.
    pub fn project(&self, project_name: &str) -> Option<&ProjectStateAuthority> {
        self.projects.get(project_name)
    }

    /// Returns mutable access to an attached project authority for supervisor
    /// startup recovery and migration work.
    pub fn project_mut(&mut self, project_name: &str) -> Option<&mut ProjectStateAuthority> {
        self.projects.get_mut(project_name)
    }

    /// Handles one typed supervisor request synchronously.
    pub fn handle(&mut self, request: SupervisorRequest) -> SupervisorResponse {
        match request {
            SupervisorRequest::Attach { project } => match self.attach(project.clone()) {
                Ok(outcome) => {
                    self.ensure_queue(&project.name);
                    SupervisorResponse::Attached {
                        outcome: outcome.into(),
                    }
                }
                Err(error) => rejected(&error),
            },
            SupervisorRequest::Detach { project_name } => SupervisorResponse::Detached {
                detached: self.detach(&project_name).is_some(),
            },
            SupervisorRequest::Status { project_name } => self.project(&project_name).map_or_else(
                || SupervisorResponse::Rejected {
                    code: "project_not_attached".into(),
                    detail: project_name,
                },
                |authority| SupervisorResponse::Status {
                    revision: authority.revision(),
                },
            ),
            SupervisorRequest::Mutate {
                project_name,
                operation,
            } => match self.projects.get_mut(&project_name) {
                Some(authority) => match authority.accept(operation) {
                    Ok(receipt) => SupervisorResponse::Accepted { receipt },
                    Err(error) => rejected(&error),
                },
                None => SupervisorResponse::Rejected {
                    code: "project_not_attached".into(),
                    detail: project_name,
                },
            },
        }
    }

    /// Returns the number of currently attached projects.
    #[must_use]
    pub fn project_count(&self) -> usize {
        self.projects.len()
    }

    #[cfg(test)]
    fn queue_count(&self) -> usize {
        self.queues.len()
    }

    fn ensure_queue(&mut self, project_name: &str) {
        if self.queues.contains_key(project_name) {
            return;
        }
        let Some(authority) = self.projects.get(project_name) else {
            return;
        };
        let state_dir = authority.state_dir().to_path_buf();
        let writer = Arc::new(SupervisorStoreWriter::new(
            control_socket_path(&self.home),
            project_name.into(),
        ));
        let log_dir = authority.project().log_dir(&self.home);
        let queue = crate::queue_loop::QueueLoop::new(
            orbs::orb_store::OrbStore::new(state_dir.join("orbs.jsonl"))
                .with_write_sink(writer.clone()),
            orbs::dep_store::DepStore::new(state_dir.join("deps.jsonl")).with_write_sink(writer),
            state_dir,
        )
        .with_worker_evidence_dir(log_dir.join("heddle"))
        .with_execution_log_path(authority.project().execution_log_path(&self.home))
        .with_project_key(project_name);
        self.queues.insert(project_name.into(), queue);
        let dispatch = crate::worker::dispatcher::project_worker_config(
            Some(&self.home),
            authority.project(),
        )
        .map(|base_worker_config| {
            crate::daemon::DispatchSettings {
                base_worker_config,
                max_concurrency: crate::config::load_config(authority.project().config_root())
                    .map_or(1, |config| config.max_concurrency),
            }
        })
        .map_err(|error| tracing::warn!(project = %project_name, %error, "dynamic project dispatch disabled"))
        .ok();
        self.dispatch.insert(project_name.into(), dispatch);
    }

    pub fn take_attached_queues(
        &mut self,
    ) -> (
        HashMap<String, crate::queue_loop::QueueLoop>,
        HashMap<String, Option<crate::daemon::DispatchSettings>>,
    ) {
        (
            std::mem::take(&mut self.queues),
            std::mem::take(&mut self.dispatch),
        )
    }

    pub fn restore_attached_queues(
        &mut self,
        queues: HashMap<String, crate::queue_loop::QueueLoop>,
        dispatch: HashMap<String, Option<crate::daemon::DispatchSettings>>,
    ) {
        self.queues.extend(queues);
        self.dispatch.extend(dispatch);
    }
}

fn rejected(error: &ProjectAuthorityError) -> SupervisorResponse {
    let code = match error {
        ProjectAuthorityError::RevisionConflict { .. } => "revision_conflict",
        ProjectAuthorityError::Lease(ProjectLeaseError::Busy { .. }) => "project_busy",
        ProjectAuthorityError::Lease(_)
        | ProjectAuthorityError::Journal(_)
        | ProjectAuthorityError::Projection(_) => "supervisor_error",
    };
    SupervisorResponse::Rejected {
        code: code.into(),
        detail: error.to_string(),
    }
}

impl ProjectStateAuthority {
    /// Attaches a registered project to the local supervisor.
    ///
    /// The caller supplies a monotonically increasing supervisor epoch. The
    /// epoch is diagnostic; exclusive lease ownership remains authoritative.
    pub fn attach(
        home: &Path,
        project: ProjectEntry,
        epoch: u64,
    ) -> Result<Self, ProjectAuthorityError> {
        let state_dir = project.shared_state_dir(home);
        let lease = ProjectStateLease::try_acquire(
            &state_dir,
            ProjectLeaseMetadata {
                project_id: project.name.clone(),
                pid: std::process::id(),
                epoch,
            },
        )?;
        let journal = OperationJournal::open(&state_dir)?;
        let mut authority = Self {
            project,
            state_dir,
            lease,
            journal,
        };
        authority.project_pending_operations()?;
        Ok(authority)
    }

    /// Returns the registered project attached to this authority.
    pub fn project(&self) -> &ProjectEntry {
        &self.project
    }

    /// Returns the authority's user-local state directory.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Returns the exclusive writer lease held for this project.
    pub fn lease(&self) -> &ProjectStateLease {
        &self.lease
    }

    /// Returns the currently durable state revision.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.journal.revision()
    }

    /// Accepts a new operation once, rejecting stale state-dependent writes.
    pub fn accept(
        &mut self,
        operation: StateOperation,
    ) -> Result<StateReceipt, ProjectAuthorityError> {
        if let Some(receipt) = self.journal.receipt(&operation.operation_id).cloned() {
            self.project_pending_operations()?;
            return Ok(receipt);
        }
        // Reject malformed mutations before they enter the authoritative WAL:
        // an invalid durable row could otherwise prevent every later replay.
        let _ = operation.shared_mutation()?;
        let current_revision = self.journal.revision();
        if operation.base_revision != current_revision {
            return Err(ProjectAuthorityError::RevisionConflict {
                operation_id: operation.operation_id,
                base_revision: operation.base_revision,
                current_revision,
            });
        }
        let receipt = self.journal.accept(operation)?;
        self.project_pending_operations()?;
        Ok(receipt)
    }

    fn project_pending_operations(&mut self) -> Result<(), StateProjectionError> {
        let applied_revision = StateProjection::applied_revision(&self.state_dir)?;
        for receipt in self.journal.receipts_after(applied_revision) {
            StateProjection::apply(&self.state_dir, &receipt)?;
        }
        Ok(())
    }

    /// Imports an existing isolated `.orbs` projection exactly once.
    ///
    /// Migration is deliberately allowed only before this authority has
    /// accepted any operation. Each imported latest-value record enters the
    /// journal normally, giving the shared state an auditable recovery trail.
    pub fn import_legacy_projection(
        &mut self,
        legacy_dir: &Path,
    ) -> Result<usize, ProjectAuthorityError> {
        if self.revision() != 0 {
            return Ok(0);
        }
        let orbs = orbs::orb_store::OrbStore::new(legacy_dir.join("orbs.jsonl"))
            .load_all_including_tombstoned()
            .map_err(StateProjectionError::ReadLegacyOrb)?;
        let edges = orbs::dep_store::DepStore::new(legacy_dir.join("deps.jsonl"))
            .all_edges()
            .map_err(|error| StateProjectionError::ReadLegacy(error.to_string()))?;
        let mut imported = 0;
        for orb in orbs {
            self.accept(StateOperation::mutation(
                uuid::Uuid::new_v4(),
                self.revision(),
                SharedStateMutation::UpsertOrb { orb },
            ))?;
            imported += 1;
        }
        for edge in edges {
            self.accept(StateOperation::mutation(
                uuid::Uuid::new_v4(),
                self.revision(),
                SharedStateMutation::UpsertDependency { edge },
            ))?;
            imported += 1;
        }
        Ok(imported)
    }
}

/// Materializes journal entries into the legacy JSONL files.
///
/// The projection watermark advances only after an individual record is
/// flushed. A crash between the record and watermark writes repeats a complete
/// latest-value record, which is semantically idempotent under the JSONL
/// stores' normal replay rules. A crash cannot advance the watermark ahead of
/// durable state.
#[derive(Debug, Default)]
struct StateProjection;

impl StateProjection {
    fn watermark_path(state_dir: &Path) -> PathBuf {
        state_dir.join("projection-revision")
    }

    fn applied_revision(state_dir: &Path) -> Result<u64, StateProjectionError> {
        let path = Self::watermark_path(state_dir);
        match std::fs::read_to_string(&path) {
            Ok(value) => value
                .trim()
                .parse()
                .map_err(|source| StateProjectionError::ParseWatermark { path, source }),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(source) => Err(StateProjectionError::ReadWatermark { path, source }),
        }
    }

    fn apply(state_dir: &Path, receipt: &StateReceipt) -> Result<(), StateProjectionError> {
        Self::apply_mutation(state_dir, receipt.operation.shared_mutation()?)?;
        let path = Self::watermark_path(state_dir);
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|source| StateProjectionError::Write {
                path: path.clone(),
                source,
            })?;
        file.write_all(receipt.revision.to_string().as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_data())
            .map_err(|source| StateProjectionError::Write { path, source })
    }

    fn apply_mutation(
        state_dir: &Path,
        mutation: SharedStateMutation,
    ) -> Result<(), StateProjectionError> {
        match mutation {
            SharedStateMutation::UpsertOrb { orb } => {
                append_materialized(&state_dir.join("orbs.jsonl"), &orb)?;
            }
            SharedStateMutation::ResetOrb { orb, event } => {
                crate::orb_cmd::persist_reset(state_dir, &orb, &event).map_err(|source| {
                    StateProjectionError::Write {
                        path: state_dir.to_path_buf(),
                        source,
                    }
                })?;
            }
            SharedStateMutation::UpsertDependency { edge } => {
                append_materialized(&state_dir.join("deps.jsonl"), &edge)?;
            }
            SharedStateMutation::Batch { mutations } => {
                for mutation in mutations {
                    Self::apply_mutation(state_dir, mutation)?;
                }
            }
        }
        Ok(())
    }
}

fn append_materialized<T: Serialize>(path: &Path, value: &T) -> Result<(), StateProjectionError> {
    let row = serde_json::to_vec(value).map_err(|source| StateProjectionError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| StateProjectionError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&row)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_data())
        .map_err(|source| StateProjectionError::Write {
            path: path.to_path_buf(),
            source,
        })
}

impl ProjectStateLease {
    /// Acquires the exclusive writer lease for `state_home`.
    ///
    /// The lock is non-blocking so a second supervisor can report a typed busy
    /// condition immediately instead of hanging during startup.
    pub fn try_acquire(
        state_home: &Path,
        metadata: ProjectLeaseMetadata,
    ) -> Result<Self, ProjectLeaseError> {
        std::fs::create_dir_all(state_home).map_err(|source| {
            ProjectLeaseError::CreateStateDir {
                path: state_home.to_path_buf(),
                source,
            }
        })?;
        let path = state_home.join("supervisor.lock");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| ProjectLeaseError::Open {
                path: path.clone(),
                source,
            })?;

        let lock_result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if lock_result != 0 {
            let source = io::Error::last_os_error();
            if matches!(source.kind(), io::ErrorKind::WouldBlock) {
                return Err(ProjectLeaseError::Busy {
                    path,
                    owner: read_metadata(&mut file),
                });
            }
            return Err(ProjectLeaseError::Lock { path, source });
        }

        write_metadata(&mut file, &path, &metadata)?;
        Ok(Self {
            file,
            path,
            metadata,
        })
    }

    /// Returns the path of the held lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns this lease's diagnostic ownership metadata.
    pub fn metadata(&self) -> &ProjectLeaseMetadata {
        &self.metadata
    }
}

impl Drop for ProjectStateLease {
    fn drop(&mut self) {
        // Releasing an already-released advisory lock is harmless. Drop cannot
        // report failures, and process exit releases the lock as a backstop.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn read_metadata(file: &mut File) -> Option<ProjectLeaseMetadata> {
    file.rewind().ok()?;
    serde_json::from_reader(file).ok()
}

fn write_metadata(
    file: &mut File,
    path: &Path,
    metadata: &ProjectLeaseMetadata,
) -> Result<(), ProjectLeaseError> {
    let encoded =
        serde_json::to_vec(metadata).map_err(|source| ProjectLeaseError::SerializeMetadata {
            path: path.to_path_buf(),
            source,
        })?;
    file.set_len(0)
        .and_then(|()| file.rewind())
        .and_then(|()| file.write_all(&encoded))
        .and_then(|()| file.sync_all())
        .map_err(|source| ProjectLeaseError::WriteMetadata {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use orbs::dep_store::DepStore;
    use orbs::orb_store::OrbStore;

    use super::*;

    #[test]
    fn reset_projection_replay_preserves_audit_identity_and_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let mut orb = Orb::new("Failed", "specification");
        orb.status = Some(orbs::orb::OrbStatus::Failed);
        orb.result = Some("setup failure".into());
        store.append(&orb).unwrap();
        let (reset, event) =
            crate::orb_cmd::prepare_reset(&store, orb.id.as_str(), Some("fixed")).unwrap();
        let mutation = SharedStateMutation::ResetOrb { orb: reset, event };
        StateProjection::apply_mutation(dir.path(), mutation.clone()).unwrap();
        StateProjection::apply_mutation(dir.path(), mutation).unwrap();
        let events = orbs::audit_store::AuditStore::new(dir.path().join("events.jsonl"))
            .events_for_orb(&orb.id)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0]
            .details
            .as_ref()
            .unwrap()
            .contains("setup failure"));
        assert_eq!(
            store.load_by_id(&orb.id).unwrap().unwrap().status,
            Some(orbs::orb::OrbStatus::Pending)
        );
    }

    fn metadata(project_id: &str, epoch: u64) -> ProjectLeaseMetadata {
        ProjectLeaseMetadata {
            project_id: project_id.into(),
            pid: std::process::id(),
            epoch,
        }
    }

    fn project(name: &str) -> ProjectEntry {
        ProjectEntry {
            name: name.into(),
            path: None,
            root_dir: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn operation(id: uuid::Uuid) -> StateOperation {
        StateOperation::mutation(
            id,
            0,
            SharedStateMutation::UpsertOrb {
                orb: Orb::new("example", "example description"),
            },
        )
    }

    #[test]
    fn project_state_lease_rejects_a_second_writer() {
        let directory = tempfile::tempdir().unwrap();
        let first = ProjectStateLease::try_acquire(directory.path(), metadata("demo", 1)).unwrap();

        let error = ProjectStateLease::try_acquire(directory.path(), metadata("demo", 2))
            .expect_err("a second supervisor must not acquire the same project state");

        match error {
            ProjectLeaseError::Busy { owner, .. } => {
                assert_eq!(owner, Some(metadata("demo", 1)));
            }
            other => panic!("expected busy lease error, got {other:?}"),
        }
        assert!(first.path().ends_with("supervisor.lock"));
    }

    #[test]
    fn project_state_lease_is_available_after_owner_drops() {
        let directory = tempfile::tempdir().unwrap();
        let first = ProjectStateLease::try_acquire(directory.path(), metadata("demo", 1)).unwrap();
        drop(first);

        let second = ProjectStateLease::try_acquire(directory.path(), metadata("demo", 2)).unwrap();
        assert_eq!(second.metadata(), &metadata("demo", 2));
    }

    #[test]
    fn local_mutation_lease_rejects_a_concurrent_cli_writer() {
        let directory = tempfile::tempdir().unwrap();
        let first = LocalMutationLease::try_acquire(directory.path()).unwrap();
        let error = LocalMutationLease::try_acquire(directory.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("another CLI"));
        assert!(first.path().ends_with("cli-mutation.lock"));
    }

    #[test]
    fn project_authority_owns_the_registered_projects_local_state_home() {
        let home = tempfile::tempdir().unwrap();
        let authority = ProjectStateAuthority::attach(home.path(), project("alpha"), 7).unwrap();

        assert_eq!(authority.project().name, "alpha");
        assert_eq!(
            authority.state_dir(),
            home.path()
                .join(".orboros/projects/alpha-8ed3f6ad685b/state")
        );
        assert_eq!(authority.lease().metadata(), &metadata("alpha", 7));
    }

    #[test]
    fn project_authority_rejects_a_second_supervisor_for_one_project() {
        let home = tempfile::tempdir().unwrap();
        let first = ProjectStateAuthority::attach(home.path(), project("alpha"), 1).unwrap();

        let error = ProjectStateAuthority::attach(home.path(), project("alpha"), 2)
            .expect_err("the project already has an authoritative writer");

        assert!(matches!(
            error,
            ProjectAuthorityError::Lease(ProjectLeaseError::Busy { .. })
        ));
        drop(first);
    }

    #[test]
    fn local_supervisor_attaches_multiple_projects_and_deduplicates_retries() {
        let home = tempfile::tempdir().unwrap();
        let mut supervisor = LocalSupervisor::new(home.path().to_path_buf());

        assert_eq!(
            supervisor.attach(project("alpha")).unwrap(),
            AttachOutcome::Attached
        );
        assert_eq!(
            supervisor.attach(project("beta")).unwrap(),
            AttachOutcome::Attached
        );
        assert_eq!(
            supervisor.attach(project("alpha")).unwrap(),
            AttachOutcome::AlreadyAttached
        );
        assert_eq!(supervisor.project_count(), 2);
        assert_eq!(
            supervisor.project("beta").unwrap().lease().metadata().epoch,
            2
        );

        drop(supervisor.detach("alpha"));
        assert_eq!(supervisor.project_count(), 1);
        assert!(supervisor.project("alpha").is_none());
    }

    #[test]
    fn operation_journal_deduplicates_retries_and_replays_revisions() {
        let state_dir = tempfile::tempdir().unwrap();
        let first_id = uuid::Uuid::new_v4();
        let second_id = uuid::Uuid::new_v4();
        let mut journal = OperationJournal::open(state_dir.path()).unwrap();

        let first = journal.accept(operation(first_id)).unwrap();
        let retry = journal.accept(operation(first_id)).unwrap();
        let second = journal.accept(operation(second_id)).unwrap();

        assert_eq!(first.revision, 1);
        assert_eq!(retry, first);
        assert_eq!(second.revision, 2);

        let reopened = OperationJournal::open(state_dir.path()).unwrap();
        assert_eq!(reopened.revision(), 2);
    }

    #[test]
    fn project_authority_rejects_stale_operations_but_replays_a_retry() {
        let home = tempfile::tempdir().unwrap();
        let mut authority =
            ProjectStateAuthority::attach(home.path(), project("alpha"), 1).unwrap();
        let accepted = authority.accept(operation(uuid::Uuid::new_v4())).unwrap();

        let stale = authority
            .accept(StateOperation::mutation(
                uuid::Uuid::new_v4(),
                0,
                SharedStateMutation::UpsertOrb {
                    orb: Orb::new("stale", "must be rejected"),
                },
            ))
            .unwrap_err();
        assert!(matches!(
            stale,
            ProjectAuthorityError::RevisionConflict {
                current_revision: 1,
                ..
            }
        ));
        assert_eq!(authority.accept(accepted.operation).unwrap().revision, 1);
    }

    #[test]
    fn authority_materializes_and_recovers_a_durable_orb_operation() {
        let home = tempfile::tempdir().unwrap();
        let orb = Orb::new("shared orb", "written through the supervisor");
        let operation = StateOperation::mutation(
            uuid::Uuid::new_v4(),
            0,
            SharedStateMutation::UpsertOrb { orb: orb.clone() },
        );

        let state_dir = {
            let mut authority =
                ProjectStateAuthority::attach(home.path(), project("alpha"), 1).unwrap();
            let receipt = authority.accept(operation.clone()).unwrap();
            assert_eq!(receipt.revision, 1);
            assert_eq!(
                std::fs::read_to_string(authority.state_dir().join("projection-revision"))
                    .unwrap()
                    .trim(),
                "1"
            );
            authority.state_dir().to_path_buf()
        };

        let reopened = ProjectStateAuthority::attach(home.path(), project("alpha"), 2).unwrap();
        assert_eq!(reopened.revision(), 1);
        let materialized: Orb = serde_json::from_str(
            std::fs::read_to_string(state_dir.join("orbs.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(materialized.id, orb.id);
        assert_eq!(materialized.title, "shared orb");
    }

    #[test]
    fn authority_rejects_invalid_operations_before_journaling_them() {
        let home = tempfile::tempdir().unwrap();
        let mut authority =
            ProjectStateAuthority::attach(home.path(), project("alpha"), 1).unwrap();
        let error = authority
            .accept(StateOperation {
                operation_id: uuid::Uuid::new_v4(),
                base_revision: 0,
                kind: "unknown".into(),
                payload: serde_json::json!({"kind": "unknown"}),
            })
            .unwrap_err();
        assert!(matches!(error, ProjectAuthorityError::Projection(_)));
        assert_eq!(authority.revision(), 0);
        assert!(!authority.state_dir().join("projection-revision").exists());
    }

    #[test]
    fn authority_imports_an_isolated_projection_once() {
        let home = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let orb = Orb::new("legacy orb", "preserve this state");
        orbs::orb_store::OrbStore::new(legacy.path().join("orbs.jsonl"))
            .append(&orb)
            .unwrap();

        let mut authority =
            ProjectStateAuthority::attach(home.path(), project("alpha"), 1).unwrap();
        assert_eq!(
            authority.import_legacy_projection(legacy.path()).unwrap(),
            1
        );
        assert_eq!(authority.revision(), 1);
        assert_eq!(
            authority.import_legacy_projection(legacy.path()).unwrap(),
            0
        );
        let shared = orbs::orb_store::OrbStore::new(authority.state_dir().join("orbs.jsonl"));
        assert_eq!(shared.load_all().unwrap()[0].id, orb.id);
    }

    #[test]
    fn supervisor_control_plane_attaches_and_serializes_mutations() {
        let home = tempfile::tempdir().unwrap();
        let mut supervisor = LocalSupervisor::new(home.path().to_path_buf());

        assert_eq!(
            supervisor.handle(SupervisorRequest::Attach {
                project: project("alpha"),
            }),
            SupervisorResponse::Attached {
                outcome: AttachOutcomeWire::Attached
            }
        );
        assert_eq!(supervisor.queue_count(), 1);
        let accepted = supervisor.handle(SupervisorRequest::Mutate {
            project_name: "alpha".into(),
            operation: operation(uuid::Uuid::new_v4()),
        });
        assert!(matches!(
            accepted,
            SupervisorResponse::Accepted {
                receipt: StateReceipt { revision: 1, .. }
            }
        ));
        assert_eq!(
            supervisor.handle(SupervisorRequest::Status {
                project_name: "alpha".into(),
            }),
            SupervisorResponse::Status { revision: 1 }
        );
    }

    #[test]
    fn unavailable_supervisor_writer_fails_without_local_fallback() {
        let home = tempfile::tempdir().unwrap();
        let socket = control_socket_path(home.path());
        let writer = SupervisorStoreWriter::new(socket, "missing".into());
        let error = writer
            .write_orb(&Orb::new("must not fall back", "shared mode"), None)
            .unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ));
        assert!(error.to_string().contains("requires its running daemon"));
        assert!(!home.path().join("orbs.jsonl").exists());
    }

    #[test]
    fn batch_projection_replays_all_records_as_one_revision() {
        let home = tempfile::tempdir().unwrap();
        let mut supervisor = LocalSupervisor::new(home.path().to_path_buf());
        supervisor.attach(project("batch")).unwrap();
        let parent = Orb::new("parent", "parent");
        let child = Orb::new("child", "child");
        let edge = DepEdge::new(
            parent.id.clone(),
            child.id.clone(),
            orbs::dep::EdgeType::Parent,
        );
        let response = supervisor.handle(SupervisorRequest::Mutate {
            project_name: "batch".into(),
            operation: StateOperation::mutation(
                uuid::Uuid::new_v4(),
                0,
                SharedStateMutation::Batch {
                    mutations: vec![
                        SharedStateMutation::UpsertOrb {
                            orb: parent.clone(),
                        },
                        SharedStateMutation::UpsertOrb { orb: child.clone() },
                        SharedStateMutation::UpsertDependency { edge: edge.clone() },
                    ],
                },
            ),
        });
        assert!(
            matches!(response, SupervisorResponse::Accepted { ref receipt } if receipt.revision == 1)
        );

        let state_dir = supervisor
            .project("batch")
            .unwrap()
            .state_dir()
            .to_path_buf();
        assert_eq!(
            OrbStore::new(state_dir.join("orbs.jsonl"))
                .load_all()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            DepStore::new(state_dir.join("deps.jsonl"))
                .all_edges()
                .unwrap(),
            vec![edge]
        );
        assert_eq!(
            std::fs::read_to_string(state_dir.join("projection-revision"))
                .unwrap()
                .trim(),
            "1"
        );
    }

    #[tokio::test]
    async fn local_socket_serves_typed_supervisor_requests() {
        let home = tempfile::tempdir().unwrap();
        let socket = home.path().join("supervisor.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let supervisor = Arc::new(Mutex::new(LocalSupervisor::new(home.path().to_path_buf())));
        let task = tokio::spawn(serve_local_socket(listener, Arc::clone(&supervisor)));

        let stream = UnixStream::connect(&socket).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let request = serde_json::to_vec(&SupervisorRequest::Attach {
            project: project("alpha"),
        })
        .unwrap();
        writer.write_all(&request).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        let mut lines = AsyncBufReader::new(reader).lines();
        let response: SupervisorResponse =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            response,
            SupervisorResponse::Attached {
                outcome: AttachOutcomeWire::Attached
            }
        );
        task.abort();
    }

    #[tokio::test]
    async fn local_socket_client_surfaces_typed_responses() {
        let home = tempfile::tempdir().unwrap();
        let socket = control_socket_path(home.path());
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let supervisor = Arc::new(Mutex::new(LocalSupervisor::new(home.path().to_path_buf())));
        let task = tokio::spawn(serve_local_socket(listener, supervisor));

        let response = request_local_socket(
            &socket,
            &SupervisorRequest::Attach {
                project: project("alpha"),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            response,
            SupervisorResponse::Attached {
                outcome: AttachOutcomeWire::Attached
            }
        );
        task.abort();
    }

    #[tokio::test]
    async fn store_writer_serializes_orb_updates_through_the_supervisor() {
        let home = tempfile::tempdir().unwrap();
        let socket = control_socket_path(home.path());
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let supervisor = Arc::new(Mutex::new(LocalSupervisor::new(home.path().to_path_buf())));
        let task = tokio::spawn(serve_local_socket(listener, Arc::clone(&supervisor)));
        request_local_socket(
            &socket,
            &SupervisorRequest::Attach {
                project: project("alpha"),
            },
        )
        .await
        .unwrap();

        let writer = SupervisorStoreWriter::new(socket, "alpha".into());
        let orb = Orb::new("shared writer", "must use the authority");
        tokio::task::spawn_blocking(move || writer.write_orb(&orb, None))
            .await
            .unwrap()
            .unwrap();

        let state_dir = supervisor
            .lock()
            .await
            .project("alpha")
            .unwrap()
            .state_dir()
            .to_path_buf();
        let stored = orbs::orb_store::OrbStore::new(state_dir.join("orbs.jsonl"));
        assert_eq!(stored.load_all().unwrap().len(), 1);
        task.abort();
    }
}
