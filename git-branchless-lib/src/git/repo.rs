//! Operations on the Git repository. This module exists for a few reasons:
//!
//! - To ensure that every call to a Git operation has an associated `wrap_err`
//! for use with `Try`.
//! - To improve the interface in some cases. In particular, some operations in
//! `git2` return an `Error` with code `ENOTFOUND`, but we should really return
//! an `Option` in those cases.
//! - To make it possible to audit all the Git operations carried out in the
//! codebase.
//! - To collect some different helper Git functions.

use std::borrow::{Borrow, Cow};
use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::ffi::OsStr;
use std::num::TryFromIntError;
use std::ops::Add;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::string::FromUtf8Error;
use std::time::{Duration, SystemTime};
use std::{io, time};

use bstr::{BString, ByteSlice, ByteVec};
use chrono::NaiveDateTime;
use cursive::theme::BaseColor;
use cursive::utils::markup::StyledString;
use git2::{message_trailers_bytes, DiffOptions};
use itertools::Itertools;
use thiserror::Error;
use tracing::{instrument, warn};

use crate::core::effects::{Effects, OperationType};
use crate::core::eventlog::EventTransactionId;
use crate::core::formatting::{Glyphs, StyledStringBuilder};
use crate::core::node_descriptors::{
    render_node_descriptors, CommitMessageDescriptor, CommitOidDescriptor, NodeObject, Redactor,
};
use crate::git::config::{Config, ConfigRead};
use crate::git::oid::{make_non_zero_oid, MaybeZeroOid, NonZeroOid};
use crate::git::run::GitRunInfo;
use crate::git::tree::{dehydrate_tree, get_changed_paths_between_trees, hydrate_tree, Tree};

use super::index::{Index, IndexEntry};
use super::snapshot::WorkingCopySnapshot;
use super::status::FileMode;
use super::{tree, Diff, StatusEntry};

#[allow(missing_docs)]
#[derive(Debug, Error)]
pub enum Error {
    #[error("could not open repository: {0}")]
    OpenRepo(#[source] git2::Error),

    #[error("could not read index: {0}")]
    ReadIndex(#[source] git2::Error),

    #[error("could not create .git/branchless directory at {path}: {source}")]
    CreateBranchlessDir { source: io::Error, path: PathBuf },

    #[error("could not open database connection at {path}: {source}")]
    OpenDatabase {
        source: rusqlite::Error,
        path: PathBuf,
    },

    #[error("this repository does not have an associated working copy")]
    NoWorkingCopyPath,

    #[error("could not read config: {0}")]
    ReadConfig(#[source] git2::Error),

    #[error("could not set HEAD (detached) to {oid}: {source}")]
    SetHead {
        source: git2::Error,
        oid: NonZeroOid,
    },

    #[error("could not find object {oid}")]
    FindObject { oid: NonZeroOid },

    #[error("could not calculate merge-base between {lhs} and {rhs}: {source}")]
    FindMergeBase {
        source: git2::Error,
        lhs: NonZeroOid,
        rhs: NonZeroOid,
    },

    #[error("could not find blob {oid}: {source} ")]
    FindBlob {
        source: git2::Error,
        oid: NonZeroOid,
    },

    #[error("could not create blob: {0}")]
    CreateBlob(#[source] git2::Error),

    #[error("could not create blob from {path}: {source}")]
    CreateBlobFromPath { source: git2::Error, path: PathBuf },

    #[error("could not find commit {oid}: {source}")]
    FindCommit {
        source: git2::Error,
        oid: NonZeroOid,
    },

    #[error("could not create commit: {0}")]
    CreateCommit(#[source] git2::Error),

    #[error("could not cherry-pick commit {commit} onto {onto}: {0}")]
    CherryPickCommit {
        source: git2::Error,
        commit: NonZeroOid,
        onto: NonZeroOid,
    },

    #[error("could not fast-cherry-pick commit {commit} onto {onto}: {0}")]
    CherryPickFast {
        source: git2::Error,
        commit: NonZeroOid,
        onto: NonZeroOid,
    },

    #[error("could not amend the current commit: {0}")]
    Amend(#[source] git2::Error),

    #[error("could not find tree {oid}: {source}")]
    FindTree {
        source: git2::Error,
        oid: MaybeZeroOid,
    },

    #[error(transparent)]
    ReadTree(tree::Error),

    #[error(transparent)]
    ReadTreeEntry(tree::Error),

    #[error(transparent)]
    HydrateTree(tree::Error),

    #[error("could not write index as tree: {0}")]
    WriteIndexToTree(#[source] git2::Error),

    #[error("could not read branch information: {0}")]
    ReadBranch(#[source] git2::Error),

    #[error("could not find branch with name '{name}': {source}")]
    FindBranch { source: git2::Error, name: String },

    #[error("could not find upstream branch for branch with name '{name}': {source}")]
    FindUpstreamBranch { source: git2::Error, name: String },

    #[error("could not create branch with name '{name}': {source}")]
    CreateBranch { source: git2::Error, name: String },

    #[error("could not read reference information: {0}")]
    ReadReference(#[source] git2::Error),

    #[error("could not find reference '{}': {source}", name.as_str())]
    FindReference {
        source: git2::Error,
        name: ReferenceName,
    },

    #[error("could not delete reference: {0}")]
    DeleteReference(#[source] git2::Error),

    #[error("could not resolve reference: {0}")]
    ResolveReference(#[source] git2::Error),

    #[error("could not diff trees {old_tree} and {new_tree}: {source}")]
    DiffTreeToTree {
        source: git2::Error,
        old_tree: MaybeZeroOid,
        new_tree: MaybeZeroOid,
    },

    #[error("could not diff tree {tree} and index: {source}")]
    DiffTreeToIndex {
        source: git2::Error,
        tree: NonZeroOid,
    },

    #[error(transparent)]
    DehydrateTree(tree::Error),

    #[error("could not create working copy snapshot: {0}")]
    CreateSnapshot(#[source] eyre::Error),

    #[error("could not create reference: {0}")]
    CreateReference(#[source] git2::Error),

    #[error("could not calculate changed paths: {0}")]
    GetChangedPaths(#[source] super::tree::Error),

    #[error("could not get paths touched by commit {commit}")]
    GetPatch { commit: NonZeroOid },

    #[error("compute patch ID: {0}")]
    GetPatchId(#[source] git2::Error),

    #[error("could not get references: {0}")]
    GetReferences(#[source] git2::Error),

    #[error("could not get branches: {0}")]
    GetBranches(#[source] git2::Error),

    #[error("could not get remote names: {0}")]
    GetRemoteNames(#[source] git2::Error),

    #[error("HEAD is unborn (try making a commit?)")]
    UnbornHead,

    #[error("could not create commit signature: {0}")]
    CreateSignature(#[source] git2::Error),

    #[error("could not execute git: {0}")]
    ExecGit(#[source] eyre::Error),

    #[error("unsupported spec: {0} (ends with @, which is buggy in libgit2")]
    UnsupportedRevParseSpec(String),

    #[error("could not parse git version output: {0}")]
    ParseGitVersionOutput(String),

    #[error("could not parse git version specifier: {0}")]
    ParseGitVersionSpecifier(String),

    #[error("comment char was not ASCII: {char}")]
    CommentCharNotAscii { source: TryFromIntError, char: u32 },

    #[error("unknown status line prefix ASCII character: {prefix}")]
    UnknownStatusLinePrefix { prefix: u8 },

    #[error("could not parse status line: {0}")]
    ParseStatusEntry(#[source] eyre::Error),

    #[error("could not decode UTF-8 value for {item}")]
    DecodeUtf8 { item: &'static str },

    #[error("could not decode UTF-8 value for reference name: {0}")]
    DecodeReferenceName(#[from] ReferenceNameError),

    #[error("could not read message trailers: {0}")]
    ReadMessageTrailer(#[source] git2::Error),

    #[error("could not describe commit {commit}: {source}")]
    DescribeCommit {
        source: eyre::Error,
        commit: NonZeroOid,
    },

    #[error(transparent)]
    IntegerConvert(TryFromIntError),

    #[error(transparent)]
    SystemTime(time::SystemTimeError),

    #[error(transparent)]
    Git(git2::Error),

    #[error(transparent)]
    Io(io::Error),

    #[error("miscellaneous error: {0}")]
    Other(String),
}

/// Result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Convert a `git2::Error` into an `eyre::Error` with an auto-generated message.
pub(super) fn wrap_git_error(error: git2::Error) -> eyre::Error {
    eyre::eyre!("Git error {:?}: {}", error.code(), error.message())
}

/// Clean up a message, removing extraneous whitespace plus comment lines starting with
/// `comment_char`, and ensure that the message ends with a newline.
#[instrument]
pub fn message_prettify(message: &str, comment_char: Option<char>) -> Result<String> {
    let comment_char = match comment_char {
        Some(ch) => {
            let ch = u32::from(ch);
            let ch = u8::try_from(ch).map_err(|err| Error::CommentCharNotAscii {
                source: err,
                char: ch,
            })?;
            Some(ch)
        }
        None => None,
    };
    let message = git2::message_prettify(message, comment_char).map_err(Error::Git)?;
    Ok(message)
}

/// A snapshot of information about a certain reference. Updates to the
/// reference after this value is obtained are not reflected.
///
/// `HEAD` is typically a symbolic reference, which means that it's a reference
/// that points to another reference. Usually, the other reference is a branch.
/// In this way, you can check out a branch and move the branch (e.g. by
/// committing) and `HEAD` is also effectively updated (you can traverse the
/// pointed-to reference and get the current commit OID).
///
/// There are a couple of interesting edge cases to worry about:
///
/// - `HEAD` is detached. This means that it's pointing directly to a commit and
/// is not a symbolic reference for the time being. This is uncommon in normal
/// Git usage, but very common in `git-branchless` usage.
/// - `HEAD` is unborn. This means that it doesn't even exist yet. This happens
/// when a repository has been freshly initialized, but no commits have been
/// made, for example.
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedReferenceInfo {
    /// The OID of the commit that `HEAD` points to. If `HEAD` is unborn, then
    /// this is `None`.
    pub oid: Option<NonZeroOid>,

    /// The name of the reference that `HEAD` points to symbolically. If `HEAD`
    /// is detached, then this is `None`.
    pub reference_name: Option<ReferenceName>,
}

impl ResolvedReferenceInfo {
    /// Get the name of the branch, if any. Returns `None` if `HEAD` is
    /// detached. The `refs/heads/` prefix, if any, is stripped.
    pub fn get_branch_name(&self) -> Result<Option<&str>> {
        let reference_name = match &self.reference_name {
            Some(reference_name) => reference_name.as_str(),
            None => return Ok(None),
        };
        Ok(Some(
            reference_name
                .strip_prefix("refs/heads/")
                .unwrap_or(reference_name),
        ))
    }
}

/// The parsed version of Git.
#[derive(Debug, PartialEq, PartialOrd, Eq)]
pub struct GitVersion(pub isize, pub isize, pub isize);

impl FromStr for GitVersion {
    type Err = Error;

    #[instrument]
    fn from_str(output: &str) -> Result<GitVersion> {
        let output = output.trim();
        let words = output.split(&[' ', '-'][..]).collect::<Vec<&str>>();
        let version_str: &str = match &words.as_slice() {
            [_git, _version, version_str, ..] => version_str,
            _ => return Err(Error::ParseGitVersionOutput(output.to_owned())),
        };
        match version_str.split('.').collect::<Vec<&str>>().as_slice() {
            [major, minor, patch, ..] => {
                let major = major
                    .parse()
                    .map_err(|_| Error::ParseGitVersionSpecifier(version_str.to_owned()))?;
                let minor = minor
                    .parse()
                    .map_err(|_| Error::ParseGitVersionSpecifier(version_str.to_owned()))?;

                // Example version without a real patch number: `2.33.GIT`.
                let patch: isize = patch.parse().unwrap_or_default();

                Ok(GitVersion(major, minor, patch))
            }
            _ => Err(Error::ParseGitVersionSpecifier(version_str.to_owned())),
        }
    }
}

/// Options for `Repo::cherry_pick_fast`.
#[derive(Clone, Debug)]
pub struct CherryPickFastOptions {
    /// Detect if a commit is being applied onto a parent with the same tree,
    /// and skip applying the patch in that case.
    pub reuse_parent_tree_if_possible: bool,
}

/// An error raised when attempting the `Repo::cherry_pick_fast` operation.
#[allow(missing_docs)]
#[derive(Debug, Error)]
pub enum CherryPickFastError {
    /// A merge conflict occurred, so the cherry-pick could not continue.
    #[error("merge conflict in {} paths", conflicting_paths.len())]
    MergeConflict {
        /// The paths that were in conflict.
        conflicting_paths: HashSet<PathBuf>,
    },

    #[error("could not get paths touched by commit {commit}")]
    GetPatch { commit: NonZeroOid },

    #[error("could not get conflicts generated by cherry-pick of {commit} onto {onto}: {source}")]
    GetConflicts {
        source: git2::Error,
        commit: NonZeroOid,
        onto: NonZeroOid,
    },

    #[error("invalid UTF-8 for {item} path: {source}")]
    DecodePath {
        source: bstr::FromUtf8Error,
        item: &'static str,
    },

    #[error(transparent)]
    HydrateTree(tree::Error),

    #[error(transparent)]
    Repo(#[from] Error),

    #[error(transparent)]
    Git(git2::Error),
}

/// Options for `Repo::amend_fast`
#[derive(Debug)]
pub enum AmendFastOptions {
    /// Amend a set of paths from the current state of the working copy.
    FromWorkingCopy {
        /// The status entries for the files to amend.
        status_entries: Vec<StatusEntry>,
    },
    /// Amend a set of paths from the current state of the index.
    FromIndex {
        /// The paths to amend.
        paths: Vec<PathBuf>,
    },
}

impl AmendFastOptions {
    /// Returns whether there are any paths to be amended.
    pub fn is_empty(&self) -> bool {
        match &self {
            AmendFastOptions::FromIndex { paths } => paths.is_empty(),
            AmendFastOptions::FromWorkingCopy { status_entries } => status_entries.is_empty(),
        }
    }
}

/// Wrapper around `git2::Repository`.
pub struct Repo {
    pub(super) inner: git2::Repository,
}

impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<Git repository at: {:?}>", self.get_path())
    }
}

impl Repo {
    /// Get the Git repository associated with the given directory.
    #[instrument]
    pub fn from_dir(path: &Path) -> Result<Self> {
        let repo = git2::Repository::discover(path).map_err(Error::OpenRepo)?;
        Ok(Repo { inner: repo })
    }

    /// Get the Git repository associated with the current directory.
    #[instrument]
    pub fn from_current_dir() -> Result<Self> {
        let path = std::env::current_dir().map_err(Error::Io)?;
        Repo::from_dir(&path)
    }

    /// Open a new copy of the repository.
    #[instrument]
    pub fn try_clone(&self) -> Result<Self> {
        let path = self.get_path();
        let repo = git2::Repository::open(path).map_err(Error::OpenRepo)?;
        Ok(Repo { inner: repo })
    }

    /// Get the path to the `.git` directory for the repository.
    pub fn get_path(&self) -> &Path {
        self.inner.path()
    }

    /// Get the path to the `packed-refs` file for the repository.
    pub fn get_packed_refs_path(&self) -> PathBuf {
        self.inner.path().join("packed-refs")
    }

    /// Get the path to the directory inside the `.git` directory which contains
    /// state used for the current rebase (if any).
    pub fn get_rebase_state_dir_path(&self) -> PathBuf {
        self.inner.path().join("rebase-merge")
    }

    /// Get the path to the working copy for this repository. If the repository
    /// is bare (has no working copy), returns `None`.
    pub fn get_working_copy_path(&self) -> Option<&Path> {
        self.inner.workdir()
    }

    /// Get the index file for this repository.
    pub fn get_index(&self) -> Result<Index> {
        let mut index = self.inner.index().map_err(Error::ReadIndex)?;
        // If we call `get_index` twice in a row, it seems to return the same index contents, even if the on-disk index has changed.
        index.read(false).map_err(Error::ReadIndex)?;
        Ok(Index { inner: index })
    }

    /// Get the configuration object for the repository.
    ///
    /// **Warning**: This object should only be used for read operations. Write
    /// operations should go to the `config` file under the `.git/branchless`
    /// directory.
    #[instrument]
    pub fn get_readonly_config(&self) -> Result<impl ConfigRead> {
        let config = self.inner.config().map_err(Error::ReadConfig)?;
        Ok(Config::from(config))
    }

    /// Get the file where git-branchless-specific Git configuration is stored.
    #[instrument]
    pub fn get_config_path(&self) -> PathBuf {
        self.get_path().join("branchless").join("config")
    }

    /// Get the directory where the DAG for the repository is stored.
    #[instrument]
    pub fn get_dag_dir(&self) -> PathBuf {
        self.get_path().join("branchless").join("dag")
    }

    /// Get the directory to store man-pages. Note that this is the `man`
    /// directory, and not a subsection thereof. `git-branchless` man-pages must
    /// go into the `man/man1` directory to be found by `man`.
    #[instrument]
    pub fn get_man_dir(&self) -> PathBuf {
        self.get_path().join("branchless").join("man")
    }

    /// Get a directory suitable for storing temporary files.
    ///
    /// In particular, this directory is guaranteed to be on the same filesystem
    /// as the Git repository itself, so you can move files between them
    /// atomically. See
    /// <https://github.com/arxanas/git-branchless/discussions/120>.
    #[instrument]
    pub fn get_tempfile_dir(&self) -> PathBuf {
        self.get_path().join("branchless").join("tmp")
    }

    /// Get the connection to the SQLite database for this repository.
    #[instrument]
    pub fn get_db_conn(&self) -> Result<rusqlite::Connection> {
        let dir = self.get_path().join("branchless");
        std::fs::create_dir_all(&dir).map_err(|err| Error::CreateBranchlessDir {
            source: err,
            path: dir.clone(),
        })?;
        let path = dir.join("db.sqlite3");
        let conn = rusqlite::Connection::open(&path).map_err(|err| Error::OpenDatabase {
            source: err,
            path: path.clone(),
        })?;
        Ok(conn)
    }

    /// Get a snapshot of information about a given reference.
    #[instrument]
    pub fn resolve_reference(&self, reference: &Reference) -> Result<ResolvedReferenceInfo> {
        let oid = reference.peel_to_commit()?.map(|commit| commit.get_oid());
        let reference_name: Option<ReferenceName> = match reference.inner.kind() {
            Some(git2::ReferenceType::Direct) => None,
            Some(git2::ReferenceType::Symbolic) => match reference.inner.symbolic_target_bytes() {
                Some(name) => Some(ReferenceName::from_bytes(name.to_vec())?),
                None => {
                    return Err(Error::DecodeUtf8 { item: "reference" });
                }
            },
            None => return Err(Error::Other("Unknown `HEAD` reference type".to_string())),
        };
        Ok(ResolvedReferenceInfo {
            oid,
            reference_name,
        })
    }

    /// Get the OID for the repository's `HEAD` reference.
    #[instrument]
    pub fn get_head_info(&self) -> Result<ResolvedReferenceInfo> {
        match self.find_reference(&"HEAD".into())? {
            Some(reference) => self.resolve_reference(&reference),
            None => Ok(ResolvedReferenceInfo {
                oid: None,
                reference_name: None,
            }),
        }
    }

    /// Set the `HEAD` reference directly to the provided `oid`. Does not touch
    /// the working copy.
    #[instrument]
    pub fn set_head(&self, oid: NonZeroOid) -> Result<()> {
        self.inner
            .set_head_detached(oid.inner)
            .map_err(|err| Error::SetHead { source: err, oid })?;
        Ok(())
    }

    /// Detach `HEAD` by making it point directly to its current OID, rather
    /// than to a branch. If `HEAD` is unborn, logs a warning.
    #[instrument]
    pub fn detach_head(&self, head_info: &ResolvedReferenceInfo) -> Result<()> {
        match head_info.oid {
            Some(oid) => self
                .inner
                .set_head_detached(oid.inner)
                .map_err(|err| Error::SetHead { source: err, oid }),
            None => {
                warn!("Attempted to detach `HEAD` while `HEAD` is unborn");
                Ok(())
            }
        }
    }

    /// Detect if an interactive rebase has started but not completed.
    ///
    /// Git will send us spurious `post-rewrite` events marked as `amend` during an
    /// interactive rebase, indicating that some of the commits have been rewritten
    /// as part of the rebase plan, but not all of them. This function attempts to
    /// detect when an interactive rebase is underway, and if the current
    /// `post-rewrite` event is spurious.
    ///
    /// There are two practical issues for users as a result of this Git behavior:
    ///
    ///   * During an interactive rebase, we may see many "processing 1 rewritten
    ///   commit" messages, and then a final "processing X rewritten commits" message
    ///   once the rebase has concluded. This is potentially confusing for users, since
    ///   the operation logically only rewrote the commits once, but we displayed the
    ///   message multiple times.
    ///
    ///   * During an interactive rebase, we may warn about abandoned commits, when the
    ///   next operation in the rebase plan fixes up the abandoned commit. This can
    ///   happen even if no conflict occurred and the rebase completed successfully
    ///   without any user intervention.
    #[instrument]
    pub fn is_rebase_underway(&self) -> Result<bool> {
        use git2::RepositoryState::*;
        match self.inner.state() {
            Rebase | RebaseInteractive | RebaseMerge => Ok(true),

            // Possibly some of these states should also be treated as `true`?
            Clean | Merge | Revert | RevertSequence | CherryPick | CherryPickSequence | Bisect
            | ApplyMailbox | ApplyMailboxOrRebase => Ok(false),
        }
    }

    /// Get the type current multi-step operation (such as `rebase` or
    /// `cherry-pick`) which is underway. Returns `None` if there is no such
    /// operation.
    pub fn get_current_operation_type(&self) -> Option<&str> {
        use git2::RepositoryState::*;
        match self.inner.state() {
            Clean | Bisect => None,
            Merge => Some("merge"),
            Revert | RevertSequence => Some("revert"),
            CherryPick | CherryPickSequence => Some("cherry-pick"),
            Rebase | RebaseInteractive | RebaseMerge => Some("rebase"),
            ApplyMailbox | ApplyMailboxOrRebase => Some("am"),
        }
    }

    /// Find the merge-base between two commits. Returns `None` if a merge-base
    /// could not be found.
    #[instrument]
    pub fn find_merge_base(&self, lhs: NonZeroOid, rhs: NonZeroOid) -> Result<Option<NonZeroOid>> {
        match self.inner.merge_base(lhs.inner, rhs.inner) {
            Ok(merge_base_oid) => Ok(Some(make_non_zero_oid(merge_base_oid))),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(Error::FindMergeBase {
                source: err,
                lhs,
                rhs,
            }),
        }
    }

    /// Get the patch for a commit, i.e. the diff between that commit and its
    /// parent.
    ///
    /// If the commit has more than one parent, returns `None`.
    #[instrument]
    pub fn get_patch_for_commit(&self, effects: &Effects, commit: &Commit) -> Result<Option<Diff>> {
        let changed_paths = match self.get_paths_touched_by_commit(commit)? {
            None => return Ok(None),
            Some(changed_paths) => changed_paths,
        };
        let dehydrated_commit = self.dehydrate_commit(
            commit,
            changed_paths
                .iter()
                .map(|x| -> &Path { x })
                .collect_vec()
                .as_slice(),
            true,
        )?;

        let parent = dehydrated_commit.get_only_parent();
        let parent_tree = match &parent {
            Some(parent) => Some(parent.get_tree()?),
            None => None,
        };
        let current_tree = dehydrated_commit.get_tree()?;
        let diff = self.get_diff_between_trees(effects, parent_tree.as_ref(), &current_tree, 3)?;
        Ok(Some(diff))
    }

    /// Get the diff between two trees. This is more performant than calling
    /// libgit2's `diff_tree_to_tree` directly since it dehydrates commits
    /// before diffing them.
    #[instrument]
    pub fn get_diff_between_trees(
        &self,
        effects: &Effects,
        old_tree: Option<&Tree>,
        new_tree: &Tree,
        num_context_lines: usize,
    ) -> Result<Diff> {
        let (effects, _progress) = effects.start_operation(OperationType::CalculateDiff);
        let _effects = effects;

        let old_tree = old_tree.map(|tree| &tree.inner);
        let new_tree = Some(&new_tree.inner);

        let diff = self
            .inner
            .diff_tree_to_tree(
                old_tree,
                new_tree,
                Some(DiffOptions::new().context_lines(num_context_lines.try_into().unwrap())),
            )
            .map_err(|err| Error::DiffTreeToTree {
                source: err,
                old_tree: old_tree
                    .map(|tree| MaybeZeroOid::from(tree.id()))
                    .unwrap_or(MaybeZeroOid::Zero),
                new_tree: new_tree
                    .map(|tree| MaybeZeroOid::from(tree.id()))
                    .unwrap_or(MaybeZeroOid::Zero),
            })?;
        Ok(Diff { inner: diff })
    }

    /// Returns the set of paths currently staged to the repository's index.
    #[instrument]
    pub fn get_staged_paths(&self) -> Result<HashSet<PathBuf>> {
        let head_commit_oid = match self.get_head_info()?.oid {
            Some(oid) => oid,
            None => return Err(Error::UnbornHead),
        };
        let head_commit = self.find_commit_or_fail(head_commit_oid)?;
        let head_tree = self.find_tree_or_fail(head_commit.get_tree()?.get_oid())?;

        let diff = self
            .inner
            .diff_tree_to_index(Some(&head_tree.inner), Some(&self.get_index()?.inner), None)
            .map_err(|err| Error::DiffTreeToIndex {
                source: err,
                tree: head_tree.get_oid(),
            })?;
        let paths = diff
            .deltas()
            .into_iter()
            .flat_map(|delta| vec![delta.old_file().path(), delta.new_file().path()])
            .flat_map(|p| p.map(PathBuf::from))
            .collect();
        Ok(paths)
    }

    /// Get the file paths which were added, removed, or changed by the given
    /// commit.
    ///
    /// If the commit has no parents, returns all of the file paths in that
    /// commit's tree.
    ///
    /// If the commit has more than one parent, returns `None`.
    #[instrument]
    pub fn get_paths_touched_by_commit(&self, commit: &Commit) -> Result<Option<HashSet<PathBuf>>> {
        let parent_commits = commit.get_parents();
        let parent_tree = match parent_commits.as_slice() {
            [] => None,
            [only_parent] => Some(only_parent.get_tree()?.inner),
            [..] => return Ok(None),
        };

        let current_tree = commit.get_tree()?.inner;
        let changed_paths =
            get_changed_paths_between_trees(self, parent_tree.as_ref(), Some(&current_tree))
                .map_err(Error::GetChangedPaths)?;
        Ok(Some(changed_paths))
    }

    /// Get the patch ID for this commit.
    #[instrument]
    pub fn get_patch_id(&self, effects: &Effects, commit: &Commit) -> Result<Option<PatchId>> {
        let patch = match self.get_patch_for_commit(effects, commit)? {
            None => return Ok(None),
            Some(diff) => diff,
        };
        let patch_id = {
            let (_effects, _progress) = effects.start_operation(OperationType::CalculatePatchId);
            patch.inner.patchid(None).map_err(Error::GetPatchId)?
        };
        Ok(Some(PatchId { patch_id }))
    }

    /// Attempt to parse the user-provided object descriptor.
    pub fn revparse_single_commit(&self, spec: &str) -> Result<Option<Commit>> {
        if spec.ends_with('@') && spec.len() > 1 {
            // Weird bug in `libgit2`; it seems that it treats a name like
            // `foo-@` the same as `@`, and ignores the leading `foo`.
            return Err(Error::UnsupportedRevParseSpec(spec.to_owned()));
        }

        match self.inner.revparse_single(spec) {
            Ok(object) => match object.into_commit() {
                Ok(commit) => Ok(Some(Commit { inner: commit })),
                Err(_) => Ok(None),
            },
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(Error::Git(err)),
        }
    }

    /// Find all references in the repository.
    #[instrument]
    pub fn get_all_references(&self) -> Result<Vec<Reference>> {
        let mut all_references = Vec::new();
        for reference in self.inner.references().map_err(Error::GetReferences)? {
            let reference = reference.map_err(Error::ReadReference)?;
            all_references.push(Reference { inner: reference });
        }
        Ok(all_references)
    }

    /// Check if the repository has staged or unstaged changes. Untracked files
    /// are not included. This operation may take a while.
    #[instrument]
    pub fn has_changed_files(&self, effects: &Effects, git_run_info: &GitRunInfo) -> Result<bool> {
        let exit_code = git_run_info
            .run(
                effects,
                // This is not a mutating operation, so we don't need a transaction ID.
                None,
                &["diff", "--quiet"],
            )
            .map_err(Error::ExecGit)?;
        if exit_code.is_success() {
            Ok(false)
        } else {
            Ok(true)
        }
    }

    /// Returns the current status of the repo index and working copy.
    pub fn get_status(
        &self,
        effects: &Effects,
        git_run_info: &GitRunInfo,
        index: &Index,
        head_info: &ResolvedReferenceInfo,
        event_tx_id: Option<EventTransactionId>,
    ) -> Result<(WorkingCopySnapshot, Vec<StatusEntry>)> {
        let (effects, _progress) = effects.start_operation(OperationType::QueryWorkingCopy);
        let _effects = effects;

        let output = git_run_info
            .run_silent(
                self,
                event_tx_id,
                &["status", "--porcelain=v2", "--untracked-files=no", "-z"],
                Default::default(),
            )
            .map_err(Error::ExecGit)?
            .stdout;

        let not_null_terminator = |c: &u8| *c != 0_u8;
        let mut statuses = Vec::new();
        let mut status_bytes = output.into_iter().peekable();

        // Iterate over the status entries in the output.
        // This takes some care, because NUL bytes are both used to delimit
        // between entries, and as a separator between paths in the case
        // of renames.
        // See https://git-scm.com/docs/git-status#_porcelain_format_version_2
        while let Some(line_prefix) = status_bytes.peek() {
            let line = match line_prefix {
                // Ordinary change entry or unmerged entry.
                b'1' | b'u' => {
                    let line = status_bytes
                        .by_ref()
                        .take_while(not_null_terminator)
                        .collect_vec();
                    line
                }
                // Rename or copy change entry.
                b'2' => {
                    let mut line = status_bytes
                        .by_ref()
                        .take_while(not_null_terminator)
                        .collect_vec();
                    line.push(0_u8); // Persist first null terminator in the line.
                    line.extend(status_bytes.by_ref().take_while(not_null_terminator));
                    line
                }
                _ => {
                    return Err(Error::UnknownStatusLinePrefix {
                        prefix: *line_prefix,
                    })
                }
            };
            let entry: StatusEntry = line
                .as_slice()
                .try_into()
                .map_err(Error::ParseStatusEntry)?;
            statuses.push(entry);
        }

        let snapshot = WorkingCopySnapshot::create(self, index, head_info, &statuses)
            .map_err(Error::CreateSnapshot)?;
        Ok((snapshot, statuses))
    }

    /// Create a new reference or update an existing one.
    #[instrument]
    pub fn create_reference(
        &self,
        name: &ReferenceName,
        oid: NonZeroOid,
        force: bool,
        log_message: &str,
    ) -> Result<Reference> {
        let reference = self
            .inner
            .reference(name.as_str(), oid.inner, force, log_message)
            .map_err(Error::CreateReference)?;
        Ok(Reference { inner: reference })
    }

    /// Get a list of all remote names.
    #[instrument]
    pub fn get_all_remote_names(&self) -> Result<Vec<String>> {
        let remotes = self.inner.remotes().map_err(Error::GetRemoteNames)?;
        Ok(remotes
            .into_iter()
            .enumerate()
            .filter_map(|(i, remote_name)| match remote_name {
                Some(remote_name) => Some(remote_name.to_owned()),
                None => {
                    warn!(remote_index = i, "Remote name could not be decoded");
                    None
                }
            })
            .sorted()
            .collect())
    }

    /// Look up a reference with the given name. Returns `None` if not found.
    #[instrument]
    pub fn find_reference(&self, name: &ReferenceName) -> Result<Option<Reference>> {
        match self.inner.find_reference(name.as_str()) {
            Ok(reference) => Ok(Some(Reference { inner: reference })),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(Error::FindReference {
                source: err,
                name: name.clone(),
            }),
        }
    }

    /// Get all local branches in the repository.
    #[instrument]
    pub fn get_all_local_branches(&self) -> Result<Vec<Branch>> {
        let mut all_branches = Vec::new();
        for branch in self
            .inner
            .branches(Some(git2::BranchType::Local))
            .map_err(Error::GetBranches)?
        {
            let (branch, _branch_type) = branch.map_err(Error::ReadBranch)?;
            all_branches.push(Branch {
                repo: self,
                inner: branch,
            });
        }
        Ok(all_branches)
    }

    /// Look up the branch with the given name. Returns `None` if not found.
    #[instrument]
    pub fn find_branch(&self, name: &str, branch_type: BranchType) -> Result<Option<Branch>> {
        match self.inner.find_branch(name, branch_type) {
            Ok(branch) => Ok(Some(Branch {
                repo: self,
                inner: branch,
            })),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(Error::FindBranch {
                source: err,
                name: name.to_owned(),
            }),
        }
    }

    /// Look up a commit with the given OID. Returns `None` if not found.
    #[instrument]
    pub fn find_commit(&self, oid: NonZeroOid) -> Result<Option<Commit>> {
        match self.inner.find_commit(oid.inner) {
            Ok(commit) => Ok(Some(Commit { inner: commit })),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(Error::FindCommit { source: err, oid }),
        }
    }

    /// Like `find_commit`, but raises a generic error if the commit could not
    /// be found.
    #[instrument]
    pub fn find_commit_or_fail(&self, oid: NonZeroOid) -> Result<Commit> {
        match self.inner.find_commit(oid.inner) {
            Ok(commit) => Ok(Commit { inner: commit }),
            Err(err) => Err(Error::FindCommit { source: err, oid }),
        }
    }

    /// Look up a blob with the given OID. Returns `None` if not found.
    #[instrument]
    pub fn find_blob(&self, oid: NonZeroOid) -> Result<Option<Blob>> {
        match self.inner.find_blob(oid.inner) {
            Ok(blob) => Ok(Some(Blob { inner: blob })),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(Error::FindBlob { source: err, oid }),
        }
    }

    /// Like `find_blob`, but raises a generic error if the blob could not be
    /// found.
    #[instrument]
    pub fn find_blob_or_fail(&self, oid: NonZeroOid) -> Result<Blob> {
        match self.inner.find_blob(oid.inner) {
            Ok(blob) => Ok(Blob { inner: blob }),
            Err(err) => Err(Error::FindBlob { source: err, oid }),
        }
    }

    /// Look up the commit with the given OID and render a friendly description
    /// of it, or render an error message if not found.
    pub fn friendly_describe_commit_from_oid(
        &self,
        glyphs: &Glyphs,
        oid: NonZeroOid,
    ) -> Result<StyledString> {
        match self.find_commit(oid)? {
            Some(commit) => Ok(commit.friendly_describe(glyphs)?),
            None => {
                let NonZeroOid { inner: oid } = oid;
                Ok(StyledString::styled(
                    format!("<commit not available: {}>", oid),
                    BaseColor::Red.light(),
                ))
            }
        }
    }

    /// Read a file from disk and create a blob corresponding to its contents.
    /// If the file doesn't exist on disk, returns `None` instead.
    #[instrument]
    pub fn create_blob_from_path(&self, path: &Path) -> Result<Option<NonZeroOid>> {
        match self.inner.blob_path(path) {
            Ok(oid) => Ok(Some(make_non_zero_oid(oid))),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(Error::CreateBlobFromPath {
                source: err,
                path: path.to_owned(),
            }),
        }
    }

    /// Create a blob corresponding to the provided byte slice.
    #[instrument]
    pub fn create_blob_from_contents(&self, contents: &[u8]) -> Result<NonZeroOid> {
        let oid = self.inner.blob(contents).map_err(Error::CreateBlob)?;
        Ok(make_non_zero_oid(oid))
    }

    /// Create a new commit.
    #[instrument]
    pub fn create_commit(
        &self,
        update_ref: Option<&str>,
        author: &Signature,
        committer: &Signature,
        message: &str,
        tree: &Tree,
        parents: Vec<&Commit>,
    ) -> Result<NonZeroOid> {
        let parents = parents
            .iter()
            .map(|commit| &commit.inner)
            .collect::<Vec<_>>();
        let oid = self
            .inner
            .commit(
                update_ref,
                &author.inner,
                &committer.inner,
                message,
                &tree.inner,
                parents.as_slice(),
            )
            .map_err(Error::CreateCommit)?;
        Ok(make_non_zero_oid(oid))
    }

    /// Cherry-pick a commit in memory and return the resulting index.
    #[instrument]
    pub fn cherry_pick_commit(
        &self,
        cherry_pick_commit: &Commit,
        our_commit: &Commit,
        mainline: u32,
    ) -> Result<Index> {
        let index = self
            .inner
            .cherrypick_commit(&cherry_pick_commit.inner, &our_commit.inner, mainline, None)
            .map_err(|err| Error::CherryPickCommit {
                source: err,
                commit: cherry_pick_commit.get_oid(),
                onto: our_commit.get_oid(),
            })?;
        Ok(Index { inner: index })
    }

    /// Cherry-pick a commit in memory and return the resulting tree.
    ///
    /// The `libgit2` routines operate on entire `Index`es, which contain one
    /// entry per file in the repository. When operating on a large repository,
    /// this is prohibitively slow, as it takes several seconds just to write
    /// the index to disk. To improve performance, we reduce the size of the
    /// involved indexes by filtering out any unchanged entries from the input
    /// trees, then call into `libgit2`, then add back the unchanged entries to
    /// the output tree.
    #[instrument]
    pub fn cherry_pick_fast<'repo>(
        &'repo self,
        patch_commit: &'repo Commit,
        target_commit: &'repo Commit,
        options: &CherryPickFastOptions,
    ) -> std::result::Result<Tree<'repo>, CherryPickFastError> {
        let CherryPickFastOptions {
            reuse_parent_tree_if_possible,
        } = options;

        if *reuse_parent_tree_if_possible {
            if let Some(only_parent) = patch_commit.get_only_parent() {
                if only_parent.get_tree()?.get_oid() == target_commit.get_tree()?.get_oid() {
                    // If this patch is being applied to the same commit it was
                    // originally based on, then we can skip cherry-picking
                    // altogether, and use its tree directly. This is common e.g.
                    // when only rewording a commit message.
                    return Ok(patch_commit.get_tree()?);
                }
            };
        }

        let changed_pathbufs = self
            .get_paths_touched_by_commit(patch_commit)?
            .ok_or_else(|| CherryPickFastError::GetPatch {
                commit: patch_commit.get_oid(),
            })?
            .into_iter()
            .collect_vec();
        let changed_paths = changed_pathbufs.iter().map(PathBuf::borrow).collect_vec();

        let dehydrated_patch_commit =
            self.dehydrate_commit(patch_commit, changed_paths.as_slice(), true)?;
        let dehydrated_target_commit =
            self.dehydrate_commit(target_commit, changed_paths.as_slice(), false)?;

        let rebased_index =
            self.cherry_pick_commit(&dehydrated_patch_commit, &dehydrated_target_commit, 0)?;
        let rebased_tree = {
            if rebased_index.has_conflicts() {
                let conflicting_paths = {
                    let mut result = HashSet::new();
                    for conflict in rebased_index.inner.conflicts().map_err(|err| {
                        CherryPickFastError::GetConflicts {
                            source: err,
                            commit: patch_commit.get_oid(),
                            onto: target_commit.get_oid(),
                        }
                    })? {
                        let conflict =
                            conflict.map_err(|err| CherryPickFastError::GetConflicts {
                                source: err,
                                commit: patch_commit.get_oid(),
                                onto: target_commit.get_oid(),
                            })?;
                        if let Some(ancestor) = conflict.ancestor {
                            result.insert(ancestor.path.into_path_buf().map_err(|err| {
                                CherryPickFastError::DecodePath {
                                    source: err,
                                    item: "ancestor",
                                }
                            })?);
                        }
                        if let Some(our) = conflict.our {
                            result.insert(our.path.into_path_buf().map_err(|err| {
                                CherryPickFastError::DecodePath {
                                    source: err,
                                    item: "our",
                                }
                            })?);
                        }
                        if let Some(their) = conflict.their {
                            result.insert(their.path.into_path_buf().map_err(|err| {
                                CherryPickFastError::DecodePath {
                                    source: err,
                                    item: "their",
                                }
                            })?);
                        }
                    }
                    result
                };

                if conflicting_paths.is_empty() {
                    warn!("BUG: A merge conflict was detected, but there were no entries in `conflicting_paths`. Maybe the wrong index entry was used?")
                }

                return Err(CherryPickFastError::MergeConflict { conflicting_paths });
            }
            let rebased_entries: HashMap<PathBuf, Option<(NonZeroOid, FileMode)>> =
                changed_pathbufs
                    .into_iter()
                    .map(|changed_path| {
                        let value = match rebased_index.get_entry(&changed_path) {
                            Some(IndexEntry {
                                oid: MaybeZeroOid::Zero,
                                file_mode: _,
                            }) => {
                                warn!(
                                    ?patch_commit,
                                    ?changed_path,
                                    "BUG: index entry was zero. \
                                This probably indicates that a removed path \
                                was not handled correctly."
                                );
                                None
                            }
                            Some(IndexEntry {
                                oid: MaybeZeroOid::NonZero(oid),
                                file_mode,
                            }) => Some((oid, file_mode)),
                            None => None,
                        };
                        (changed_path, value)
                    })
                    .collect();
            let rebased_tree_oid =
                hydrate_tree(self, Some(&target_commit.get_tree()?), rebased_entries)
                    .map_err(CherryPickFastError::HydrateTree)?;
            self.find_tree_or_fail(rebased_tree_oid)?
        };
        Ok(rebased_tree)
    }

    #[instrument]
    fn dehydrate_commit(
        &self,
        commit: &Commit,
        changed_paths: &[&Path],
        base_on_parent: bool,
    ) -> Result<Commit> {
        let tree = commit.get_tree()?;
        let dehydrated_tree_oid =
            dehydrate_tree(self, &tree, changed_paths).map_err(Error::DehydrateTree)?;
        let dehydrated_tree = self.find_tree_or_fail(dehydrated_tree_oid)?;

        let signature = Signature::automated()?;
        let message = format!(
            "generated by git-branchless: temporary dehydrated commit \
                \
                This commit was originally: {:?}",
            commit.get_oid()
        );

        let parents = if base_on_parent {
            match commit.get_only_parent() {
                Some(parent) => {
                    let dehydrated_parent = self.dehydrate_commit(&parent, changed_paths, false)?;
                    vec![dehydrated_parent]
                }
                None => vec![],
            }
        } else {
            vec![]
        };
        let dehydrated_commit_oid = self.create_commit(
            None,
            &signature,
            &signature,
            &message,
            &dehydrated_tree,
            parents.iter().collect_vec(),
        )?;
        let dehydrated_commit = self.find_commit_or_fail(dehydrated_commit_oid)?;
        Ok(dehydrated_commit)
    }

    /// Look up the tree with the given OID. Returns `None` if not found.
    #[instrument]
    pub fn find_tree(&self, oid: NonZeroOid) -> Result<Option<Tree>> {
        match self.inner.find_tree(oid.inner) {
            Ok(tree) => Ok(Some(Tree { inner: tree })),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(Error::FindTree {
                source: err,
                oid: oid.into(),
            }),
        }
    }

    /// Like `find_tree`, but raises a generic error if the commit could not
    /// be found.
    #[instrument]
    pub fn find_tree_or_fail(&self, oid: NonZeroOid) -> Result<Tree> {
        match self.inner.find_tree(oid.inner) {
            Ok(tree) => Ok(Tree { inner: tree }),
            Err(err) => Err(Error::FindTree {
                source: err,
                oid: oid.into(),
            }),
        }
    }

    /// Write the provided in-memory index as a tree into Git`s object database.
    /// There must be no merge conflicts in the index.
    #[instrument]
    pub fn write_index_to_tree(&self, index: &mut Index) -> Result<NonZeroOid> {
        let oid = index
            .inner
            .write_tree_to(&self.inner)
            .map_err(Error::WriteIndexToTree)?;
        Ok(make_non_zero_oid(oid))
    }

    /// Amends the provided parent commit in memory and returns the resulting tree.
    ///
    /// Only amends the files provided in the options, and only supports amending from
    /// either the working tree or the index, but not both.
    ///
    /// See `Repo::cherry_pick_fast` for motivation for performing the operation
    /// in-memory.
    #[instrument]
    pub fn amend_fast(&self, parent_commit: &Commit, opts: &AmendFastOptions) -> Result<Tree> {
        let parent_commit_pathbufs = self
            .get_paths_touched_by_commit(parent_commit)?
            .ok_or_else(|| Error::GetPatch {
                commit: parent_commit.get_oid(),
            })?
            .into_iter()
            .collect_vec();
        let changed_paths: Vec<PathBuf> = {
            let mut result: HashSet<PathBuf> = parent_commit_pathbufs.into_iter().collect();
            match opts {
                AmendFastOptions::FromIndex { paths } => result.extend(paths.iter().cloned()),
                AmendFastOptions::FromWorkingCopy { ref status_entries } => {
                    for entry in status_entries {
                        result.extend(entry.paths().iter().cloned());
                    }
                }
            };
            result.into_iter().collect_vec()
        };
        let changed_paths = changed_paths
            .iter()
            .map(|path| path.as_path())
            .collect_vec();

        let dehydrated_parent =
            self.dehydrate_commit(parent_commit, changed_paths.as_slice(), true)?;
        let dehydrated_parent_tree = dehydrated_parent.get_tree()?;

        let repo_path = self
            .get_working_copy_path()
            .ok_or(Error::NoWorkingCopyPath)?;
        let new_tree_entries: HashMap<PathBuf, Option<(NonZeroOid, FileMode)>> = match opts {
            AmendFastOptions::FromWorkingCopy { status_entries } => status_entries
                .iter()
                .flat_map(|entry| {
                    entry.paths().into_iter().map(
                        move |path| -> Result<(PathBuf, Option<(NonZeroOid, FileMode)>)> {
                            let file_path = &repo_path.join(&path);
                            // Try to create a new blob OID based on the current on-disk
                            // contents of the file in the working copy.
                            let entry = self
                                .create_blob_from_path(file_path)?
                                .map(|oid| (oid, entry.working_copy_file_mode));
                            Ok((path, entry))
                        },
                    )
                })
                .collect::<Result<HashMap<_, _>>>()?,
            AmendFastOptions::FromIndex { paths } => {
                let index = self.get_index()?;
                paths
                    .iter()
                    .filter_map(|path| match index.get_entry(path) {
                        Some(IndexEntry {
                            oid: MaybeZeroOid::Zero,
                            ..
                        }) => {
                            warn!(?path, "index entry was zero");
                            None
                        }
                        Some(IndexEntry {
                            oid: MaybeZeroOid::NonZero(oid),
                            file_mode,
                            ..
                        }) => Some((path.clone(), Some((oid, file_mode)))),
                        None => Some((path.clone(), None)),
                    })
                    .collect::<HashMap<_, _>>()
            }
        };

        // Merge the new path entries into the existing set of parent tree.
        let amended_tree_entries: HashMap<PathBuf, Option<(NonZeroOid, FileMode)>> = changed_paths
            .into_iter()
            .map(|changed_path| {
                let value = match new_tree_entries.get(changed_path) {
                    Some(new_tree_entry) => new_tree_entry.as_ref().copied(),
                    None => match dehydrated_parent_tree.get_path(changed_path) {
                        Ok(Some(entry)) => Some((entry.get_oid(), entry.get_filemode())),
                        Ok(None) => None,
                        Err(err) => return Err(Error::ReadTree(err)),
                    },
                };
                Ok((changed_path.into(), value))
            })
            .collect::<Result<_>>()?;

        let amended_tree_oid =
            hydrate_tree(self, Some(&parent_commit.get_tree()?), amended_tree_entries)
                .map_err(Error::HydrateTree)?;
        let amended_tree = self.find_tree_or_fail(amended_tree_oid)?;

        Ok(amended_tree)
    }
}

/// The signature of a commit, identifying who it was made by and when it was made.
pub struct Signature<'repo> {
    inner: git2::Signature<'repo>,
}

impl std::fmt::Debug for Signature<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<Signature>")
    }
}

impl<'repo> Signature<'repo> {
    #[instrument]
    pub fn automated() -> Result<Self> {
        Ok(Signature {
            inner: git2::Signature::new(
                "git-branchless",
                "git-branchless@example.com",
                &git2::Time::new(0, 0),
            )
            .map_err(Error::CreateSignature)?,
        })
    }

    /// Update the timestamp of this signature to a new time.
    #[instrument]
    pub fn update_timestamp(self, now: SystemTime) -> Result<Signature<'repo>> {
        let seconds: i64 = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(Error::SystemTime)?
            .as_secs()
            .try_into()
            .map_err(Error::IntegerConvert)?;
        let time = git2::Time::new(seconds, self.inner.when().offset_minutes());
        let name = match self.inner.name() {
            Some(name) => name,
            None => {
                return Err(Error::DecodeUtf8 {
                    item: "signature name",
                })
            }
        };
        let email = match self.inner.email() {
            Some(email) => email,
            None => {
                return Err(Error::DecodeUtf8 {
                    item: "signature email",
                })
            }
        };
        let signature = git2::Signature::new(name, email, &time).map_err(Error::CreateSignature)?;
        Ok(Signature { inner: signature })
    }

    /// Get the time when this signature was applied.
    pub fn get_time(&self) -> Time {
        Time {
            inner: self.inner.when(),
        }
    }

    pub fn get_name(&self) -> Option<&str> {
        self.inner.name()
    }

    pub fn get_email(&self) -> Option<&str> {
        self.inner.email()
    }

    /// Return the friendly formatted name and email of the signature.
    pub fn friendly_describe(&self) -> Option<String> {
        let name = self.inner.name();
        let email = self.inner.email().map(|email| format!("<{}>", email));
        match (name, email) {
            (Some(name), Some(email)) => Some(format!("{} {}", name, email)),
            (Some(name), _) => Some(name.into()),
            (_, Some(email)) => Some(email),
            _ => None,
        }
    }
}

/// A checksum of the diff induced by a given commit, used for duplicate commit
/// detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PatchId {
    patch_id: git2::Oid,
}

/// Represents a commit object in the Git object database.
#[derive(Clone, Debug)]
pub struct Commit<'repo> {
    inner: git2::Commit<'repo>,
}

impl<'repo> Commit<'repo> {
    /// Get the object ID of the commit.
    #[instrument]
    pub fn get_oid(&self) -> NonZeroOid {
        NonZeroOid {
            inner: self.inner.id(),
        }
    }

    /// Get the short object ID of the commit.
    #[instrument]
    pub fn get_short_oid(&self) -> Result<String> {
        Ok(String::from_utf8_lossy(
            &self
                .inner
                .clone()
                .into_object()
                .short_id()
                .map_err(Error::Git)?,
        )
        .to_string())
    }

    /// Get the object IDs of the parents of this commit.
    #[instrument]
    pub fn get_parent_oids(&self) -> Vec<NonZeroOid> {
        self.inner.parent_ids().map(make_non_zero_oid).collect()
    }

    /// Get the parent OID of this commit if there is exactly one parent, or
    /// `None` otherwise.
    #[instrument]
    pub fn get_only_parent_oid(&self) -> Option<NonZeroOid> {
        match self.get_parent_oids().as_slice() {
            [] | [_, _, ..] => None,
            [only_parent_oid] => Some(*only_parent_oid),
        }
    }

    /// Get the number of parents of this commit.
    #[instrument]
    pub fn get_parent_count(&self) -> usize {
        self.inner.parent_count()
    }

    /// Get the parent commits of this commit.
    #[instrument]
    pub fn get_parents(&self) -> Vec<Commit<'repo>> {
        self.inner
            .parents()
            .map(|commit| Commit { inner: commit })
            .collect()
    }

    /// Get the parent of this commit if there is exactly one parent, or `None`
    /// otherwise.
    #[instrument]
    pub fn get_only_parent(&self) -> Option<Commit<'repo>> {
        match self.get_parents().as_slice() {
            [] | [_, _, ..] => None,
            [only_parent] => Some(only_parent.clone()),
        }
    }

    /// Get the commit time of this commit.
    #[instrument]
    pub fn get_time(&self) -> Time {
        Time {
            inner: self.inner.time(),
        }
    }

    /// Get the summary (first line) of the commit message.
    #[instrument]
    pub fn get_summary(&self) -> Result<BString> {
        match self.inner.summary_bytes() {
            Some(summary) => Ok(BString::from(summary)),
            None => Err(Error::DecodeUtf8 { item: "summary" }),
        }
    }

    /// Get the commit message with some whitespace trimmed.
    #[instrument]
    pub fn get_message_pretty(&self) -> Result<BString> {
        Ok(BString::from(self.inner.message_bytes()))
    }

    /// Get the commit message, without any whitespace trimmed.
    #[instrument]
    pub fn get_message_raw(&self) -> Result<BString> {
        Ok(BString::from(self.inner.message_raw_bytes()))
    }

    /// Get the author of this commit.
    #[instrument]
    pub fn get_author(&self) -> Signature {
        Signature {
            inner: self.inner.author(),
        }
    }

    /// Get the committer of this commit.
    #[instrument]
    pub fn get_committer(&self) -> Signature {
        Signature {
            inner: self.inner.committer(),
        }
    }

    /// Get the `Tree` object associated with this commit.
    #[instrument]
    pub fn get_tree(&self) -> Result<Tree> {
        let tree = self.inner.tree().map_err(|err| Error::FindTree {
            source: err,
            oid: self.inner.tree_id().into(),
        })?;
        Ok(Tree { inner: tree })
    }

    /// Get the "trailer" metadata from this commit's message. These are strings
    /// like `Signed-off-by: foo` which appear at the end of the commit message.
    #[instrument]
    pub fn get_trailers(&self) -> Result<Vec<(String, String)>> {
        let message = self.get_message_raw()?;
        let message = message.to_str().map_err(|_| Error::DecodeUtf8 {
            item: "raw messgae",
        })?;
        let mut result = Vec::new();
        for (k, v) in message_trailers_bytes(message)
            .map_err(Error::ReadMessageTrailer)?
            .iter()
        {
            if let (Ok(k), Ok(v)) = (std::str::from_utf8(k), std::str::from_utf8(v)) {
                result.push((k.to_owned(), v.to_owned()));
            }
        }
        Ok(result)
    }

    /// Print a one-line description of this commit containing its OID and
    /// summary.
    #[instrument]
    pub fn friendly_describe(&self, glyphs: &Glyphs) -> Result<StyledString> {
        let description = render_node_descriptors(
            glyphs,
            &NodeObject::Commit {
                commit: self.clone(),
            },
            &mut [
                &mut CommitOidDescriptor::new(true).map_err(|err| Error::DescribeCommit {
                    source: err,
                    commit: self.get_oid(),
                })?,
                &mut CommitMessageDescriptor::new(&Redactor::Disabled).map_err(|err| {
                    Error::DescribeCommit {
                        source: err,
                        commit: self.get_oid(),
                    }
                })?,
            ],
        )
        .map_err(|err| Error::DescribeCommit {
            source: err,
            commit: self.get_oid(),
        })?;
        Ok(description)
    }

    /// Get a multi-line description of this commit containing information about
    /// its OID, author, commit time, and message.
    #[instrument]
    pub fn friendly_preview(&self) -> Result<StyledString> {
        let commit_time = self.get_time().to_naive_date_time();
        let preview = StyledStringBuilder::from_lines(vec![
            StyledStringBuilder::new()
                .append_styled(
                    format!("Commit:\t{}", self.get_oid()),
                    BaseColor::Yellow.light(),
                )
                .build(),
            StyledString::styled(
                format!(
                    "Author:\t{}",
                    self.get_author()
                        .friendly_describe()
                        .unwrap_or_else(|| "".into())
                ),
                BaseColor::Magenta.light(),
            ),
            StyledString::styled(format!("Date:\t{}", commit_time), BaseColor::Green.light()),
            StyledString::plain(textwrap::indent(
                &self.get_message_pretty()?.to_str_lossy(),
                "    ",
            )),
        ]);
        Ok(preview)
    }

    /// Determine if the current commit is empty (has no changes compared to its
    /// parent).
    pub fn is_empty(&self) -> bool {
        match self.get_parents().as_slice() {
            [] => false,
            [parent_commit] => self.inner.tree_id() == parent_commit.inner.tree_id(),
            _ => false,
        }
    }

    /// Determine if this commit added, removed, or changed the entry at the
    /// provided file path.
    #[instrument]
    pub fn contains_touched_path(&self, path: &Path) -> Result<Option<bool>> {
        let parent = match self.get_only_parent() {
            None => return Ok(None),
            Some(parent) => parent,
        };
        let parent_tree = parent.get_tree()?;
        let current_tree = self.get_tree()?;
        let parent_oid = parent_tree
            .get_oid_for_path(path)
            .map_err(Error::ReadTreeEntry)?;
        let current_oid = current_tree
            .get_oid_for_path(path)
            .map_err(Error::ReadTreeEntry)?;
        match (parent_oid, current_oid) {
            (None, None) => Ok(Some(false)),
            (None, Some(_)) | (Some(_), None) => Ok(Some(true)),
            (Some(parent_oid), Some(current_oid)) => Ok(Some(parent_oid != current_oid)),
        }
    }

    /// Amend this existing commit.
    /// Returns the OID of the resulting new commit.
    #[instrument]
    pub fn amend_commit(
        &self,
        update_ref: Option<&str>,
        author: Option<&Signature>,
        committer: Option<&Signature>,
        message: Option<&str>,
        tree: Option<&Tree>,
    ) -> Result<NonZeroOid> {
        let oid = self
            .inner
            .amend(
                update_ref,
                author.map(|author| &author.inner),
                committer.map(|committer| &committer.inner),
                None,
                message,
                tree.map(|tree| &tree.inner),
            )
            .map_err(Error::Amend)?;
        Ok(make_non_zero_oid(oid))
    }
}

pub struct Blob<'repo> {
    inner: git2::Blob<'repo>,
}

impl<'repo> Blob<'repo> {
    pub fn get_content(&self) -> &[u8] {
        self.inner.content()
    }
}

/// The target of a reference.
#[derive(Debug, PartialEq, Eq)]
pub enum ReferenceTarget<'a> {
    /// The reference points directly to an object. This is the case for most
    /// references, such as branches.
    Direct {
        /// The OID of the pointed-to object.
        oid: MaybeZeroOid,
    },

    /// The reference points to another reference with the given name.
    Symbolic {
        /// The name of the pointed-to reference.
        reference_name: Cow<'a, OsStr>,
    },
}

#[derive(Debug, Error)]
pub enum ReferenceNameError {
    #[error("reference name was not valid UTF-8: {0}")]
    InvalidUtf8(FromUtf8Error),
}

/// The name of a reference, like `refs/heads/master`.
#[derive(Clone, Debug, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub struct ReferenceName(String);

impl ReferenceName {
    /// Create a reference name from the provided bytestring. Non-UTF-8 references are not supported.
    #[instrument]
    pub fn from_bytes(bytes: Vec<u8>) -> std::result::Result<ReferenceName, ReferenceNameError> {
        let reference_name = String::from_utf8(bytes).map_err(ReferenceNameError::InvalidUtf8)?;
        Ok(Self(reference_name))
    }

    /// View this reference name as a string. (This is a zero-cost conversion.)
    #[instrument]
    pub fn as_str(&self) -> &str {
        let Self(reference_name) = self;
        reference_name
    }
}

impl From<&str> for ReferenceName {
    fn from(s: &str) -> Self {
        ReferenceName(s.to_owned())
    }
}

impl From<String> for ReferenceName {
    fn from(s: String) -> Self {
        ReferenceName(s)
    }
}

impl From<NonZeroOid> for ReferenceName {
    fn from(oid: NonZeroOid) -> Self {
        Self::from(oid.to_string())
    }
}

impl From<MaybeZeroOid> for ReferenceName {
    fn from(oid: MaybeZeroOid) -> Self {
        Self::from(oid.to_string())
    }
}

impl AsRef<str> for ReferenceName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Represents a reference to an object.
pub struct Reference<'repo> {
    inner: git2::Reference<'repo>,
}

impl std::fmt::Debug for Reference<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.inner.name() {
            Some(name) => write!(f, "<Reference name={:?}>", name),
            None => write!(f, "<Reference name={:?}>", self.inner.name_bytes()),
        }
    }
}

impl<'repo> Reference<'repo> {
    /// Determine if the given name is a valid name for a reference.
    pub fn is_valid_name(name: &str) -> bool {
        git2::Reference::is_valid_name(name)
    }

    /// Get the name of this reference.
    #[instrument]
    pub fn get_name(&self) -> Result<ReferenceName> {
        let name = ReferenceName::from_bytes(self.inner.name_bytes().to_vec())?;
        Ok(name)
    }
    /// Get the commit object pointed to by this reference. Returns `None` if
    /// the object pointed to by the reference is a different kind of object.
    #[instrument]
    pub fn peel_to_commit(&self) -> Result<Option<Commit<'repo>>> {
        let object = match self.inner.peel(git2::ObjectType::Commit) {
            Ok(object) => object,
            Err(err) if err.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(err) => return Err(Error::ResolveReference(err)),
        };
        match object.into_commit() {
            Ok(commit) => Ok(Some(Commit { inner: commit })),
            Err(_) => Ok(None),
        }
    }

    /// Delete the reference.
    #[instrument]
    pub fn delete(&mut self) -> Result<()> {
        self.inner.delete().map_err(Error::DeleteReference)?;
        Ok(())
    }
}

/// Determine what kind of branch a reference is, given its name. The returned
/// `suffix` value is converted to a `String` to be rendered to the screen, so
/// it may have lost some information if the reference name had unusual
/// characters.
///
/// FIXME: This abstraction seems uncomfortable and clunky to use; consider
/// revising.
#[derive(Debug)]
pub enum CategorizedReferenceName<'a> {
    /// The reference represents a local branch.
    LocalBranch {
        /// The full name of the reference.
        name: &'a str,

        /// The string `refs/heads/`.
        prefix: &'static str,
    },

    /// The reference represents a remote branch.
    RemoteBranch {
        /// The full name of the reference.
        name: &'a str,

        /// The string `refs/remotes/`.
        prefix: &'static str,
    },

    /// Some other kind of reference which isn't a branch at all.
    OtherRef {
        /// The full name of the reference.
        name: &'a str,
    },
}

impl<'a> CategorizedReferenceName<'a> {
    /// Categorize the provided reference name.
    pub fn new(name: &'a ReferenceName) -> Self {
        let name = name.as_str();
        if name.starts_with("refs/heads/") {
            Self::LocalBranch {
                name,
                prefix: "refs/heads/",
            }
        } else if name.starts_with("refs/remotes/") {
            Self::RemoteBranch {
                name,
                prefix: "refs/remotes/",
            }
        } else {
            Self::OtherRef { name }
        }
    }

    /// Remove the prefix from the reference name. May raise an error if the
    /// result couldn't be encoded as an `String` (shouldn't happen).
    #[instrument]
    pub fn remove_prefix(&self) -> Result<String> {
        let (name, prefix): (_, &'static str) = match self {
            Self::LocalBranch { name, prefix } => (name, prefix),
            Self::RemoteBranch { name, prefix } => (name, prefix),
            Self::OtherRef { name } => (name, ""),
        };
        Ok(name.strip_prefix(prefix).unwrap_or(name).to_owned())
    }

    /// Render the full name of the reference, including its prefix, lossily as
    /// a `String`.
    pub fn render_full(&self) -> String {
        let name = match self {
            Self::LocalBranch { name, prefix: _ } => name,
            Self::RemoteBranch { name, prefix: _ } => name,
            Self::OtherRef { name } => name,
        };
        (*name).to_owned()
    }

    /// Render only the suffix of the reference name lossily as a `String`. The
    /// caller will usually check the type of reference and add additional
    /// information to the reference name.
    pub fn render_suffix(&self) -> String {
        let (name, prefix): (_, &'static str) = match self {
            Self::LocalBranch { name, prefix } => (name, prefix),
            Self::RemoteBranch { name, prefix } => (name, prefix),
            Self::OtherRef { name } => (name, ""),
        };
        name.strip_prefix(prefix).unwrap_or(name).to_owned()
    }

    /// Render the reference name lossily, and prepend a helpful string like
    /// `branch` to the description.
    pub fn friendly_describe(&self) -> String {
        let name = self.render_suffix();
        match self {
            CategorizedReferenceName::LocalBranch { .. } => {
                format!("branch {}", name)
            }
            CategorizedReferenceName::RemoteBranch { .. } => {
                format!("remote branch {}", name)
            }
            CategorizedReferenceName::OtherRef { .. } => format!("ref {}", name),
        }
    }
}

/// Re-export of [`git2::BranchType`]. This might change to be an opaque type later.
pub type BranchType = git2::BranchType;

/// A timestamp as used in a [`Signature`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time {
    inner: git2::Time,
}

impl Time {
    /// Calculate the associated [`SystemTime`].
    pub fn to_system_time(&self) -> Result<SystemTime> {
        Ok(SystemTime::UNIX_EPOCH.add(Duration::from_secs(
            self.inner
                .seconds()
                .try_into()
                .map_err(Error::IntegerConvert)?,
        )))
    }

    /// Calculate the associated [`NaiveDateTime`].
    pub fn to_naive_date_time(&self) -> NaiveDateTime {
        NaiveDateTime::from_timestamp(self.inner.seconds(), 0)
    }
}

/// Represents a Git branch.
pub struct Branch<'repo> {
    repo: &'repo Repo,
    inner: git2::Branch<'repo>,
}

impl std::fmt::Debug for Branch<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "<Branch name={:?}>",
            String::from_utf8_lossy(
                self.inner
                    .name_bytes()
                    .unwrap_or(b"(could not get branch name)")
            ),
        )
    }
}

impl<'repo> Branch<'repo> {
    /// Get the OID pointed to by the branch. Returns `None` if the branch is
    /// not a direct reference (which is unusual).
    pub fn get_oid(&self) -> Result<Option<NonZeroOid>> {
        Ok(self.inner.get().target().map(make_non_zero_oid))
    }

    /// Get the name of this branch, not including any `refs/heads/` prefix.
    #[instrument]
    pub fn get_name(&self) -> eyre::Result<&str> {
        self.inner
            .name()?
            .ok_or_else(|| eyre::eyre!("Could not decode branch name"))
    }

    /// If this branch tracks a remote ("upstream") branch, return that branch.
    #[instrument]
    pub fn get_upstream_branch(&self) -> Result<Option<Branch<'repo>>> {
        match self.inner.upstream() {
            Ok(upstream) => Ok(Some(Branch {
                repo: self.repo,
                inner: upstream,
            })),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => {
                let branch_name = self.inner.name_bytes().map_err(|_err| Error::DecodeUtf8 {
                    item: "branch name",
                })?;
                Err(Error::FindUpstreamBranch {
                    source: err,
                    name: String::from_utf8_lossy(branch_name).into_owned(),
                })
            }
        }
    }

    /// Get the associated remote to push to for this branch. If there is no
    /// associated remote, returns `None`. Note that this never reads the value
    /// of `push.remoteDefault`.
    #[instrument]
    pub fn get_push_remote_name(&self) -> eyre::Result<Option<String>> {
        let branch_name = self
            .inner
            .name()?
            .ok_or_else(|| eyre::eyre!("Branch name was not UTF-8: {self:?}"))?;
        let config = self.repo.get_readonly_config()?;
        if let Some(remote_name) = config.get(format!("branch.{branch_name}.pushRemote"))? {
            Ok(Some(remote_name))
        } else if let Some(remote_name) = config.get(format!("branch.{branch_name}.remote"))? {
            Ok(Some(remote_name))
        } else {
            Ok(None)
        }
    }

    /// Convert the branch into its underlying `Reference`.
    pub fn into_reference(self) -> Reference<'repo> {
        Reference {
            inner: self.inner.into_reference(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::git::status::FileMode;
    use crate::git::FileStatus;
    use crate::testing::make_git;

    use super::*;

    #[test]
    fn test_parse_git_version_output() {
        assert_eq!(
            "git version 12.34.56".parse::<GitVersion>().unwrap(),
            GitVersion(12, 34, 56)
        );
        assert_eq!(
            "git version 12.34.56\n".parse::<GitVersion>().unwrap(),
            GitVersion(12, 34, 56)
        );
        assert_eq!(
            "git version 12.34.56.78.abcdef"
                .parse::<GitVersion>()
                .unwrap(),
            GitVersion(12, 34, 56)
        );

        // See https://github.com/arxanas/git-branchless/issues/69
        assert_eq!(
            "git version 2.33.0-rc0".parse::<GitVersion>().unwrap(),
            GitVersion(2, 33, 0)
        );

        // See https://github.com/arxanas/git-branchless/issues/85
        assert_eq!(
            "git version 2.33.GIT".parse::<GitVersion>().unwrap(),
            GitVersion(2, 33, 0)
        );
    }

    #[test]
    fn test_cherry_pick_fast() -> eyre::Result<()> {
        let git = make_git()?;
        git.init_repo()?;

        git.run(&["checkout", "-b", "foo"])?;
        let test1_oid = git.commit_file_with_contents("test1", 1, "test1 contents")?;
        git.run(&["checkout", "master"])?;
        let initial2_oid =
            git.commit_file_with_contents("initial", 2, "updated initial contents")?;

        let repo = git.get_repo()?;
        let test1_commit = repo.find_commit_or_fail(test1_oid)?;
        let initial2_commit = repo.find_commit_or_fail(initial2_oid)?;
        let tree = repo.cherry_pick_fast(
            &test1_commit,
            &initial2_commit,
            &CherryPickFastOptions {
                reuse_parent_tree_if_possible: false,
            },
        )?;

        insta::assert_debug_snapshot!(tree, @r###"
        Tree {
            inner: Tree {
                id: 367f91ddd5df2d1c18742ce3f09b4944944cac3a,
            },
        }
        "###);

        insta::assert_debug_snapshot!(tree.inner.iter().map(|entry| entry.name().unwrap().to_string()).collect_vec(), @r###"
        [
            "initial.txt",
            "test1.txt",
        ]
        "###);

        Ok(())
    }

    #[test]
    fn test_amend_fast_from_index() -> eyre::Result<()> {
        let git = make_git()?;
        git.init_repo()?;

        git.run(&["checkout", "master"])?;
        let initial_oid = git.commit_file_with_contents("initial", 2, "initial contents")?;
        git.write_file("initial", "updated contents")?;

        let repo = git.get_repo()?;
        let initial_commit = repo.find_commit_or_fail(initial_oid)?;

        let tree = initial_commit.get_tree()?;
        insta::assert_debug_snapshot!(tree, @r###"
        Tree {
            inner: Tree {
                id: 01deb7745d411223bbf6b9cb1abaeed451bb25a0,
            },
        }
        "###);
        insta::assert_debug_snapshot!(tree.inner.iter().map(|entry| (entry.name().unwrap().to_string(), entry.id().to_string())).collect_vec(), @r###"
        [
            (
                "initial.txt",
                "5c41c3d7e736911dbbd53d62c10292b9bc78f838",
            ),
        ]
        "###);

        let tree = repo.amend_fast(
            &initial_commit,
            &AmendFastOptions::FromIndex {
                paths: vec!["initial.txt".into()],
            },
        )?;

        insta::assert_debug_snapshot!(tree, @r###"
        Tree {
            inner: Tree {
                id: 01deb7745d411223bbf6b9cb1abaeed451bb25a0,
            },
        }
        "###);
        insta::assert_debug_snapshot!(tree.inner.iter().map(|entry| (entry.name().unwrap().to_string(), entry.id().to_string())).collect_vec(), @r###"
        [
            (
                "initial.txt",
                "5c41c3d7e736911dbbd53d62c10292b9bc78f838",
            ),
        ]
        "###);

        git.run(&["add", "initial.txt"])?;
        let tree = repo.amend_fast(
            &initial_commit,
            &AmendFastOptions::FromIndex {
                paths: vec!["initial.txt".into()],
            },
        )?;

        insta::assert_debug_snapshot!(tree, @r###"
        Tree {
            inner: Tree {
                id: 1c15b79a72c3285df172fcfdaceedb7259283eb5,
            },
        }
        "###);
        insta::assert_debug_snapshot!(tree.inner.iter().map(|entry| (entry.name().unwrap().to_string(), entry.id().to_string())).collect_vec(), @r###"
        [
            (
                "initial.txt",
                "53cd9398c8a2d92f18d279c6cad3f5dde67235e7",
            ),
        ]
        "###);

        Ok(())
    }

    #[test]
    fn test_amend_fast_from_working_tree() -> eyre::Result<()> {
        let git = make_git()?;
        git.init_repo()?;

        git.run(&["checkout", "master"])?;
        let initial_oid = git.commit_file_with_contents("initial", 2, "initial contents")?;
        git.write_file("initial", "updated contents")?;

        let repo = git.get_repo()?;
        let initial_commit = repo.find_commit_or_fail(initial_oid)?;
        let tree = repo.amend_fast(
            &initial_commit,
            &AmendFastOptions::FromWorkingCopy {
                status_entries: vec![StatusEntry {
                    index_status: FileStatus::Renamed,
                    working_copy_status: FileStatus::Unmodified,
                    working_copy_file_mode: FileMode::Blob,
                    path: "initial.txt".into(),
                    orig_path: None,
                }],
            },
        )?;

        insta::assert_debug_snapshot!(tree, @r###"
        Tree {
            inner: Tree {
                id: 1c15b79a72c3285df172fcfdaceedb7259283eb5,
            },
        }
        "###);
        insta::assert_debug_snapshot!(tree.inner.iter().map(|entry| (entry.name().unwrap().to_string(), entry.id().to_string())).collect_vec(), @r###"
        [
            (
                "initial.txt",
                "53cd9398c8a2d92f18d279c6cad3f5dde67235e7",
            ),
        ]
        "###);

        git.write_file("file2", "another file")?;
        git.write_file("initial", "updated contents again")?;
        let tree = repo.amend_fast(
            &initial_commit,
            &AmendFastOptions::FromWorkingCopy {
                status_entries: vec![StatusEntry {
                    index_status: FileStatus::Unmodified,
                    working_copy_status: FileStatus::Added,
                    working_copy_file_mode: FileMode::Blob,
                    path: "file2.txt".into(),
                    orig_path: None,
                }],
            },
        )?;
        insta::assert_debug_snapshot!(tree, @r###"
        Tree {
            inner: Tree {
                id: 1a9fbbecd825881c3e79f0fb194a1c1e1104fe0f,
            },
        }
        "###);
        insta::assert_debug_snapshot!(tree.inner.iter().map(|entry| (entry.name().unwrap().to_string(), entry.id().to_string())).collect_vec(), @r###"
        [
            (
                "file2.txt",
                "cdcb28483da7783a8b505a074c50632a5481a69b",
            ),
            (
                "initial.txt",
                "5c41c3d7e736911dbbd53d62c10292b9bc78f838",
            ),
        ]
        "###);

        git.delete_file("initial")?;
        let tree = repo.amend_fast(
            &initial_commit,
            &AmendFastOptions::FromWorkingCopy {
                status_entries: vec![StatusEntry {
                    index_status: FileStatus::Unmodified,
                    working_copy_status: FileStatus::Deleted,
                    working_copy_file_mode: FileMode::Blob,
                    path: "initial.txt".into(),
                    orig_path: None,
                }],
            },
        )?;
        insta::assert_debug_snapshot!(tree, @r###"
        Tree {
            inner: Tree {
                id: 4b825dc642cb6eb9a060e54bf8d69288fbee4904,
            },
        }
        "###);
        insta::assert_debug_snapshot!(tree.inner.iter().map(|entry| (entry.name().unwrap().to_string(), entry.id().to_string())).collect_vec(), @"[]");

        Ok(())
    }

    #[test]
    fn test_branch_debug() -> eyre::Result<()> {
        let git = make_git()?;
        git.init_repo()?;

        let repo = git.get_repo()?;
        let branch = repo.find_branch("master", BranchType::Local)?.unwrap();
        insta::assert_debug_snapshot!(branch, @r###"<Branch name="master">"###);

        Ok(())
    }
}
