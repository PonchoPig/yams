//! Shared Unix fd-pinning helpers.
//!
//! Store, wiki, query-log, embed, and service code should use these openers
//! instead of growing a fourth copy of `O_NOFOLLOW` / exclusive-create /
//! `renameat` `NOREPLACE` / fsync.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{self as rfs, Mode, OFlags, RenameFlags, Stat};
use rustix::io::Errno;
use thiserror::Error;

/// `O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK | O_DIRECTORY`
pub const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);

/// `O_RDWR | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK`
pub const EXISTING_FILE_FLAGS: OFlags = OFlags::RDWR
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);

/// Exclusive create: existing-file flags plus `O_CREAT | O_EXCL`.
pub const CREATE_FILE_FLAGS: OFlags = EXISTING_FILE_FLAGS
    .union(OFlags::CREATE)
    .union(OFlags::EXCL);

/// Device and inode identity for a pinned descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identity {
    /// Filesystem device id.
    pub device: u64,
    /// Inode number.
    pub inode: u64,
}

impl Identity {
    /// Build from a rustix `stat` buffer. Field widths differ by target.
    #[allow(clippy::unnecessary_cast)]
    pub fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        }
    }
}

/// Failure to rename with `NOREPLACE` / `RENAME_EXCL`.
#[derive(Debug, Error)]
pub enum RenameError {
    /// Destination already exists; source was not moved.
    #[error("exclusive rename destination exists: {path}")]
    AlreadyExists { path: PathBuf },
    /// Source or destination path is missing a parent or file name.
    #[error("exclusive rename path is incomplete: {path}")]
    IncompletePath { path: PathBuf },
    /// Kernel or path inspection failed.
    #[error("exclusive rename of {from} to {to}: {source}")]
    Io {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Rename `from` to `to` only if `to` does not already exist.
///
/// Uses `renameat` with `RENAME_NOREPLACE` / `RENAME_EXCL`. A racing creator
/// of `to` cannot be overwritten.
pub fn rename_exclusive(from: &Path, to: &Path) -> Result<(), RenameError> {
    let from_parent = parent_dir(from)?;
    let from_name = file_name(from)?;
    let to_parent = parent_dir(to)?;
    let to_name = file_name(to)?;

    let from_dir = rfs::open(from_parent, DIRECTORY_FLAGS, Mode::empty())
        .map_err(|error| io_rename(from, to, error))?;
    let to_dir = rfs::open(to_parent, DIRECTORY_FLAGS, Mode::empty())
        .map_err(|error| io_rename(from, to, error))?;

    match rfs::renameat_with(
        &from_dir,
        from_name,
        &to_dir,
        to_name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {
            let _ = rfs::fsync(to_dir);
            Ok(())
        }
        Err(Errno::EXIST | Errno::NOTEMPTY) => Err(RenameError::AlreadyExists {
            path: to.to_path_buf(),
        }),
        Err(error) => Err(io_rename(from, to, error)),
    }
}

fn parent_dir(path: &Path) -> Result<&Path, RenameError> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent),
        Some(_) => Ok(Path::new(".")),
        None => Err(RenameError::IncompletePath {
            path: path.to_path_buf(),
        }),
    }
}

fn file_name(path: &Path) -> Result<&OsStr, RenameError> {
    path.file_name().ok_or_else(|| RenameError::IncompletePath {
        path: path.to_path_buf(),
    })
}

fn io_rename(from: &Path, to: &Path, error: Errno) -> RenameError {
    RenameError::Io {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        source: io::Error::from(error),
    }
}

/// Join an existing path's bytes with a suffix without UTF-8 requirements.
pub fn appended_name(path: &OsStr, suffix: impl AsRef<OsStr>) -> std::ffi::OsString {
    let mut bytes = path.as_bytes().to_vec();
    bytes.extend_from_slice(suffix.as_ref().as_bytes());
    std::os::unix::ffi::OsStringExt::from_vec(bytes)
}
