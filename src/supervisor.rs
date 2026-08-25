//! Local supervisor ownership primitives.
//!
//! Shared project state has one authoritative writer.  The owner is established
//! with an OS-held lease rather than a PID file, which is only diagnostic and
//! can be stale after a crash.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::config::ProjectEntry;

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

/// Failures exposed by the project state authority.
#[derive(Debug, Error)]
pub enum ProjectAuthorityError {
    #[error(transparent)]
    Lease(#[from] ProjectLeaseError),
    #[error(transparent)]
    Journal(#[from] OperationJournalError),
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
#[derive(Debug)]
pub struct LocalSupervisor {
    home: PathBuf,
    next_epoch: u64,
    projects: HashMap<String, ProjectStateAuthority>,
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
        self.projects.insert(project.name, authority);
        Ok(AttachOutcome::Attached)
    }

    /// Detaches a project and releases its writer lease.
    pub fn detach(&mut self, project_name: &str) -> Option<ProjectStateAuthority> {
        self.projects.remove(project_name)
    }

    /// Returns an attached project authority by registered project name.
    pub fn project(&self, project_name: &str) -> Option<&ProjectStateAuthority> {
        self.projects.get(project_name)
    }

    /// Handles one typed supervisor request synchronously.
    pub fn handle(&mut self, request: SupervisorRequest) -> SupervisorResponse {
        match request {
            SupervisorRequest::Attach { project } => match self.attach(project) {
                Ok(outcome) => SupervisorResponse::Attached {
                    outcome: outcome.into(),
                },
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
}

fn rejected(error: &ProjectAuthorityError) -> SupervisorResponse {
    let code = match error {
        ProjectAuthorityError::RevisionConflict { .. } => "revision_conflict",
        ProjectAuthorityError::Lease(ProjectLeaseError::Busy { .. }) => "project_busy",
        ProjectAuthorityError::Lease(_) | ProjectAuthorityError::Journal(_) => "supervisor_error",
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
        Ok(Self {
            project,
            state_dir,
            lease,
            journal,
        })
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
        if let Some(receipt) = self.journal.receipt(&operation.operation_id) {
            return Ok(receipt.clone());
        }
        let current_revision = self.journal.revision();
        if operation.base_revision != current_revision {
            return Err(ProjectAuthorityError::RevisionConflict {
                operation_id: operation.operation_id,
                base_revision: operation.base_revision,
                current_revision,
            });
        }
        Ok(self.journal.accept(operation)?)
    }
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
    use super::*;

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

    fn operation(id: uuid::Uuid, kind: &str) -> StateOperation {
        StateOperation {
            operation_id: id,
            base_revision: 0,
            kind: kind.into(),
            payload: serde_json::json!({"title": "example"}),
        }
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

        let first = journal.accept(operation(first_id, "create_orb")).unwrap();
        let retry = journal.accept(operation(first_id, "create_orb")).unwrap();
        let second = journal.accept(operation(second_id, "review_orb")).unwrap();

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
        let accepted = authority
            .accept(operation(uuid::Uuid::new_v4(), "create_orb"))
            .unwrap();

        let stale = authority
            .accept(StateOperation {
                operation_id: uuid::Uuid::new_v4(),
                base_revision: 0,
                kind: "update_orb".into(),
                payload: serde_json::json!({}),
            })
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
        let accepted = supervisor.handle(SupervisorRequest::Mutate {
            project_name: "alpha".into(),
            operation: operation(uuid::Uuid::new_v4(), "create_orb"),
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
}
