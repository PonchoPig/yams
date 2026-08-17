use std::path::{Path, PathBuf};

const STORE_FORMAT_DIRECTORY: &str = "rust-v1";

/// Paths belonging to the isolated Rust-generated store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreHome {
    version_dir: PathBuf,
}

impl StoreHome {
    pub fn new(base: impl AsRef<Path>) -> Self {
        Self {
            version_dir: base.as_ref().join(STORE_FORMAT_DIRECTORY),
        }
    }

    pub fn indexes_dir(&self) -> PathBuf {
        self.version_dir.join("indexes")
    }

    pub fn vectors_path(&self) -> PathBuf {
        self.version_dir.join("vectors.sqlite3")
    }

    pub fn models_dir(&self) -> PathBuf {
        self.version_dir.join("models")
    }

    pub(crate) fn version_dir(&self) -> &Path {
        &self.version_dir
    }
}
