//! Local supervisor ownership primitives.
//!
//! Shared project state has one authoritative writer.  The owner is established
//! with an OS-held lease rather than a PID file, which is only diagnostic and
//! can be stale after a crash.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    pub fn attach(&mut self, project: ProjectEntry) -> Result<AttachOutcome, ProjectLeaseError> {
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

    /// Returns the number of currently attached projects.
    #[must_use]
    pub fn project_count(&self) -> usize {
        self.projects.len()
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
    ) -> Result<Self, ProjectLeaseError> {
        let state_dir = project.shared_state_dir(home);
        let lease = ProjectStateLease::try_acquire(
            &state_dir,
            ProjectLeaseMetadata {
                project_id: project.name.clone(),
                pid: std::process::id(),
                epoch,
            },
        )?;
        Ok(Self {
            project,
            state_dir,
            lease,
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

        assert!(matches!(error, ProjectLeaseError::Busy { .. }));
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
}
