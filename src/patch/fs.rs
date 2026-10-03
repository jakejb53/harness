//! Filesystem authority for patches, independent of the Python runtime.

use cap_std::fs::{Dir, OpenOptions, Permissions};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub struct Repository {
    root: Dir,
}

pub struct Target {
    pub canonical_path: PathBuf,
    parent: Dir,
    name: OsString,
}

pub struct Existing {
    pub target: Target,
    pub bytes: Vec<u8>,
    pub is_symlink: bool,
    permissions: Permissions,
}

pub struct Staged {
    target: Target,
    temporary: Option<OsString>,
}

/// Warnings describe failures after a namespace mutation already succeeded.
pub struct CommitReceipt {
    pub warnings: Vec<String>,
}

impl Repository {
    /// Ambient authority is used only to establish the repository capability.
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            root: Dir::open_ambient_dir(path, cap_std::ambient_authority())?,
        })
    }

    /// Updates resolve and read their target; entry-only operations inspect metadata only.
    pub fn existing(&self, path: &Path, follow_final: bool) -> io::Result<Existing> {
        validate_path(path)?;
        let canonical = if follow_final {
            self.root.canonicalize(path)?
        } else {
            self.entry_path(path)?
        };
        let target = self.target(canonical)?;
        let entry = target.parent.symlink_metadata(&target.name)?;
        let is_symlink = entry.file_type().is_symlink();
        if !entry.is_file() && !is_symlink {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file or symlink", path.display()),
            ));
        }
        let (bytes, metadata) = if !follow_final || is_symlink {
            // Deletion and native rename need no read access, UTF-8 text, or target resolution.
            (Vec::new(), entry)
        } else {
            let mut file = target.parent.open(&target.name)?;
            let metadata = file.metadata()?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            (bytes, metadata)
        };
        Ok(Existing {
            target,
            bytes,
            is_symlink,
            permissions: metadata.permissions(),
        })
    }

    /// Reject existing destinations during validation, including dangling symlinks.
    pub fn vacant(&self, path: &Path) -> io::Result<Target> {
        validate_path(path)?;
        let target = self.target(self.entry_path(path)?)?;
        match target.parent.symlink_metadata(&target.name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(target),
            Err(error) => Err(error),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("destination {} already exists", path.display()),
            )),
        }
    }

    /// Canonicalize parents only so deletion never follows the final entry.
    fn entry_path(&self, path: &Path) -> io::Result<PathBuf> {
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path must name a file"))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Ok(self.root.canonicalize(parent)?.join(name))
    }

    /// Pin the resolved parent so later operations do not reopen an ambient path.
    fn target(&self, canonical_path: PathBuf) -> io::Result<Target> {
        let name = canonical_path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path must name a file"))?
            .to_os_string();
        let parent_path = canonical_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = self.root.open_dir(parent_path)?;
        Ok(Target {
            canonical_path,
            parent,
            name,
        })
    }
}

impl Target {
    /// Stage a new file without exposing any partial contents at its destination.
    pub fn stage(self, contents: &[u8]) -> io::Result<Staged> {
        self.stage_with_permissions(contents, None)
    }

    /// A staged file owns cleanup until commit; all temporary paths stay confined.
    fn stage_with_permissions(
        self,
        contents: &[u8],
        permissions: Option<Permissions>,
    ) -> io::Result<Staged> {
        for _ in 0..100 {
            let name = OsString::from(format!(
                ".harness-patch-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = match self
                .parent
                .open_with(&name, OpenOptions::new().write(true).create_new(true))
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let mut staged = Staged {
                target: self,
                temporary: Some(name),
            };
            let result = (|| {
                // Apply restrictive source permissions before exposing any source contents.
                if let Some(permissions) = &permissions {
                    file.set_permissions(permissions.clone())?;
                }
                file.write_all(contents)?;
                // Writing may clear special permission bits; restore the complete source mode.
                if let Some(permissions) = permissions {
                    file.set_permissions(permissions)?;
                }
                file.sync_all()
            })();
            if let Err(error) = result {
                return Err(staged.error_with_cleanup(error));
            }
            return Ok(staged);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique patch staging file",
        ))
    }
}

impl Existing {
    /// Replacing a symlink entry is forbidden: callers must request its resolved target.
    pub fn stage(self, contents: &[u8]) -> io::Result<Staged> {
        if self.is_symlink {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "update must resolve the final symlink",
            ));
        }
        self.target
            .stage_with_permissions(contents, Some(self.permissions))
    }

    /// Rename the actual directory entry, retaining binary contents and symlinks unchanged.
    pub fn rename_to(self, target: Target) -> io::Result<CommitReceipt> {
        self.target
            .parent
            .rename(&self.target.name, &target.parent, &target.name)?;
        // Both namespaces changed. Report sync failures only after recording the move.
        let mut receipt = sync_receipt(&target.parent);
        receipt
            .warnings
            .extend(sync_receipt(&self.target.parent).warnings);
        Ok(receipt)
    }

    /// Remove the inspected entry; report durability errors without claiming no mutation.
    pub fn remove(self) -> io::Result<CommitReceipt> {
        self.target.parent.remove_file(&self.target.name)?;
        Ok(sync_receipt(&self.target.parent))
    }
}

impl Staged {
    /// Call explicitly on cancellation/failure so leftover temporary files are reported.
    pub fn cleanup(&mut self) -> io::Result<()> {
        if let Some(name) = &self.temporary {
            self.target.parent.remove_file(name).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "could not remove staging file {} beside {}: {error}",
                        name.to_string_lossy(),
                        self.target.canonical_path.display()
                    ),
                )
            })?;
            self.temporary = None;
        }
        Ok(())
    }

    /// Preserve the primary error and surface any failure cleaning up its staging file.
    fn error_with_cleanup(&mut self, error: io::Error) -> io::Error {
        match self.cleanup() {
            Ok(()) => error,
            Err(cleanup) => {
                io::Error::new(error.kind(), format!("{error}; cleanup failed: {cleanup}"))
            }
        }
    }

    /// Publish with a portable atomic rename; vacant names were checked during validation.
    pub fn commit(mut self) -> io::Result<CommitReceipt> {
        let temporary = self.temporary.as_ref().expect("uncommitted staging file");
        if let Err(error) =
            self.target
                .parent
                .rename(temporary, &self.target.parent, &self.target.name)
        {
            return Err(self.error_with_cleanup(error));
        }
        self.temporary = None;
        Ok(sync_receipt(&self.target.parent))
    }
}

impl Drop for Staged {
    /// Last-resort cleanup for unwinding; normal cancellation/failure calls cleanup explicitly.
    fn drop(&mut self) {
        if let Some(name) = &self.temporary {
            let _ = self.target.parent.remove_file(name);
        }
    }
}

/// Explicitly reject absolute input before cap-std resolves relative traversal.
fn validate_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| matches!(c, Component::RootDir | Component::Prefix(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("patch path must be repository-relative: {}", path.display()),
        ));
    }
    Ok(())
}

/// Namespace changes have happened already; fsync failures are durability warnings.
fn sync_receipt(parent: &Dir) -> CommitReceipt {
    let warnings = match parent
        .try_clone()
        .and_then(|dir| dir.into_std_file().sync_all())
    {
        Ok(()) => Vec::new(),
        Err(error) => vec![format!(
            "change committed but directory synchronization failed: {error}"
        )],
    };
    CommitReceipt { warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        /// Each fixture owns a private directory; production code never uses ambient file IO.
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "harness-patch-fs-test-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        /// Open the same capability used by production operations.
        fn repo(&self) -> Repository {
            Repository::open(&self.0).unwrap()
        }
    }

    impl Drop for Fixture {
        /// Clean up all fixture files, including failures inside assertions.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    /// Absolute paths and traversal are invalid even when an absolute path points inside.
    fn confines_paths() {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        std::fs::write(fixture.0.join("file"), b"original").unwrap();
        assert!(repo.existing(&fixture.0.join("file"), true).is_err());
        assert!(repo.existing(Path::new("../file"), true).is_err());
        assert!(repo.vacant(Path::new("../created")).is_err());
        assert!(repo.vacant(Path::new("missing/created")).is_err());
        assert!(repo.existing(Path::new("."), true).is_err());
    }

    #[test]
    /// A staged update does not change its file until commit; abandonment cleans the temp.
    fn stage_and_commit() {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        let path = fixture.0.join("file");
        std::fs::write(&path, b"old\r\n").unwrap();
        let staged = repo
            .existing(Path::new("file"), true)
            .unwrap()
            .stage(b"new\r\n")
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old\r\n");
        drop(staged);
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
        let staged = repo
            .existing(Path::new("file"), true)
            .unwrap()
            .stage(b"new\r\n")
            .unwrap();
        assert!(staged.commit().unwrap().warnings.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"new\r\n");
    }

    #[test]
    /// Creation rejects occupied names at validation and publishes an absent destination.
    fn creation_validates_destination() {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        let staged = repo
            .vacant(Path::new("file"))
            .unwrap()
            .stage(b"new")
            .unwrap();
        staged.commit().unwrap();
        assert_eq!(
            repo.vacant(Path::new("file")).err().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(fixture.0.join("file")).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    #[cfg(unix)]
    /// Internal relative symlinks resolve from the repository, preserving the link itself.
    fn follows_internal_links_and_rejects_escapes() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let repo = fixture.repo();
        std::fs::create_dir(fixture.0.join("nested")).unwrap();
        std::fs::write(fixture.0.join("target"), b"old").unwrap();
        symlink("../target", fixture.0.join("nested/link")).unwrap();
        repo.existing(Path::new("nested/link"), true)
            .unwrap()
            .stage(b"new")
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(std::fs::read(fixture.0.join("target")).unwrap(), b"new");
        assert!(
            std::fs::symlink_metadata(fixture.0.join("nested/link"))
                .unwrap()
                .is_symlink()
        );
        symlink(fixture.0.join("target"), fixture.0.join("absolute")).unwrap();
        symlink("../outside", fixture.0.join("escape")).unwrap();
        assert!(repo.existing(Path::new("absolute"), true).is_err());
        assert!(repo.existing(Path::new("escape"), true).is_err());
        assert!(repo.vacant(Path::new("escape")).is_err());
        repo.existing(Path::new("nested/link"), false)
            .unwrap()
            .remove()
            .unwrap();
        assert_eq!(std::fs::read(fixture.0.join("target")).unwrap(), b"new");
    }

    #[test]
    #[cfg(unix)]
    /// Atomic replacement preserves executable permissions without mutating hard-link aliases.
    fn permissions_and_hard_links() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let repo = fixture.repo();
        std::fs::write(fixture.0.join("source"), b"old").unwrap();
        std::fs::set_permissions(
            fixture.0.join("source"),
            std::fs::Permissions::from_mode(0o751),
        )
        .unwrap();
        std::fs::hard_link(fixture.0.join("source"), fixture.0.join("alias")).unwrap();
        let source = repo.existing(Path::new("source"), true).unwrap();
        source.stage(b"new").unwrap().commit().unwrap();
        assert_eq!(
            std::fs::metadata(fixture.0.join("source"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
        assert_eq!(std::fs::read(fixture.0.join("alias")).unwrap(), b"old");
    }

    #[test]
    /// Entry-only operations avoid loading binary content they will not interpret.
    fn deletion_reads_metadata_only() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("binary"), [0xff, 0x00]).unwrap();
        let entry = fixture.repo().existing(Path::new("binary"), false).unwrap();
        assert!(entry.bytes.is_empty());
        entry.remove().unwrap();
        assert!(!fixture.0.join("binary").exists());
    }

    #[test]
    /// Explicit cancellation cleanup removes staged contents and is safe to call twice.
    fn explicit_cleanup() {
        let fixture = Fixture::new();
        let mut staged = fixture
            .repo()
            .vacant(Path::new("new"))
            .unwrap()
            .stage(b"contents")
            .unwrap();
        staged.cleanup().unwrap();
        staged.cleanup().unwrap();
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    #[test]
    #[cfg(unix)]
    /// Native renames retain the original inode and move symlink entries without dereferencing.
    fn rename_preserves_binary_file_and_symlink() {
        use std::os::unix::fs::{MetadataExt, symlink};
        let fixture = Fixture::new();
        let repo = fixture.repo();
        std::fs::write(fixture.0.join("binary"), [0xff, 0x00]).unwrap();
        let inode = std::fs::metadata(fixture.0.join("binary")).unwrap().ino();
        let source = repo.existing(Path::new("binary"), false).unwrap();
        assert!(source.bytes.is_empty());
        source
            .rename_to(repo.vacant(Path::new("moved")).unwrap())
            .unwrap();
        assert_eq!(
            std::fs::metadata(fixture.0.join("moved")).unwrap().ino(),
            inode
        );
        assert_eq!(
            std::fs::read(fixture.0.join("moved")).unwrap(),
            [0xff, 0x00]
        );
        symlink("moved", fixture.0.join("link")).unwrap();
        repo.existing(Path::new("link"), false)
            .unwrap()
            .rename_to(repo.vacant(Path::new("new-link")).unwrap())
            .unwrap();
        assert_eq!(
            std::fs::read_link(fixture.0.join("new-link")).unwrap(),
            Path::new("moved")
        );
        assert!(std::fs::symlink_metadata(fixture.0.join("link")).is_err());
    }

    #[test]
    /// Commit errors clean temporary contents rather than relying on silent Drop cleanup.
    fn failed_commit_cleans_staging_file() {
        let fixture = Fixture::new();
        let staged = fixture
            .repo()
            .vacant(Path::new("file"))
            .unwrap()
            .stage(b"contents")
            .unwrap();
        std::fs::create_dir(fixture.0.join("file")).unwrap();
        assert!(staged.commit().is_err());
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }
}
