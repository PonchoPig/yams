use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use yams_fs::{RenameError, appended_name, rename_exclusive};

#[test]
fn exclusive_rename_moves_source_when_destination_is_absent() {
    let directory = tempfile::tempdir().unwrap();
    let from = directory.path().join("source");
    let to = directory.path().join("dest");
    fs::write(&from, b"payload").unwrap();

    rename_exclusive(&from, &to).unwrap();

    assert!(!from.exists());
    assert_eq!(fs::read(&to).unwrap(), b"payload");
}

#[test]
fn exclusive_rename_leaves_both_paths_when_destination_exists() {
    let directory = tempfile::tempdir().unwrap();
    let from = directory.path().join("source");
    let to = directory.path().join("dest");
    fs::write(&from, b"source-bytes").unwrap();
    fs::write(&to, b"dest-bytes").unwrap();

    let error = rename_exclusive(&from, &to).unwrap_err();

    assert!(
        matches!(error, RenameError::AlreadyExists { .. }),
        "{error:?}"
    );
    assert_eq!(fs::read(&from).unwrap(), b"source-bytes");
    assert_eq!(fs::read(&to).unwrap(), b"dest-bytes");
}

#[test]
fn appended_name_preserves_non_utf8_bytes() {
    let base = std::ffi::OsString::from_vec(b"cache-\xff".to_vec());
    let joined = appended_name(&base, ".corrupt");
    assert_eq!(joined.as_os_str().as_bytes(), b"cache-\xff.corrupt");
}
