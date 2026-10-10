//! The directories an agent's file access resolves in: the repository, the store, and
//! the library. Which of them an agent reaches follows from its mounts (`host::Mounts`).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Context;
use cap_std::fs::Dir;
use monty_pool::MountSpec;

use crate::repl;

/// A directory paths resolve in. The REPL reaches each through its mount (`hostfs`);
/// the direct tools name it with their `root` argument (`as_str`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Root {
    /// The launch directory; access follows the permission level.
    Repository,
    /// `[rlm.store] path`; read-write at every permission level.
    Store,
    /// This tree's `library/`; read-only at every permission level.
    Library,
}

impl Root {
    /// Every root, in the order tool definitions list them.
    pub const ALL: [Root; 3] = [Root::Repository, Root::Store, Root::Library];

    /// The `root` argument value that names this root.
    pub fn as_str(self) -> &'static str {
        match self {
            Root::Repository => "repository",
            Root::Store => "store",
            Root::Library => "library",
        }
    }

    /// The root an argument value names exactly, if any.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|root| root.as_str() == name)
    }

    /// Whether anything may write under this root; never under the library.
    pub fn writable(self) -> bool {
        self != Root::Library
    }
}

/// A direct-tool call's root as the calling agent reaches it. Each runtime holds the
/// repository and store capabilities; the library capability is the agent's own
/// (`Library`), so only an agent given one can name it.
pub enum Access {
    Repository,
    Store,
    Library(Arc<Dir>),
}

impl Access {
    /// The root this call names, for results that name it.
    pub fn root(&self) -> Root {
        match self {
            Access::Repository => Root::Repository,
            Access::Store => Root::Store,
            Access::Library(_) => Root::Library,
        }
    }

    /// The capability and read flag a reading tool (read_file, grep, list) checks for
    /// this call. The repository's flag is the runtime's, following the permission
    /// level; the store and the library are readable at every level, so their flag is
    /// a fresh constant `true`.
    pub fn readable(
        self,
        repository: &Arc<Dir>,
        store: &Arc<Dir>,
        permission: &Arc<AtomicBool>,
    ) -> (Arc<Dir>, Arc<AtomicBool>) {
        match self {
            Access::Repository => (repository.clone(), permission.clone()),
            Access::Store => (store.clone(), Arc::new(AtomicBool::new(true))),
            Access::Library(library) => (library, Arc::new(AtomicBool::new(true))),
        }
    }
}

/// This tree's `library/` directory: the REPL's read-only `/library` mount and the
/// direct tools' `root = "library"` capability. The App opens it on first use and
/// opens it again from the renamed path after `/rename`; each agent at depth 2 and
/// deeper gets a clone and uses both handles as given, never opening them by path.
#[derive(Clone)]
pub struct Library {
    pub mount: MountSpec,
    pub dir: Arc<Dir>,
}

impl Library {
    /// Open both handles from the tree's current `library/` path.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            mount: repl::library_mount(path)?,
            dir: Arc::new(
                Dir::open_ambient_dir(path, cap_std::ambient_authority())
                    .with_context(|| format!("opening library {}", path.display()))?,
            ),
        })
    }
}
