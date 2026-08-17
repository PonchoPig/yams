use yams_core::ExitCode;
use yams_store::{ManagementError, RetrievalError, StoreError, SyncError, VectorError};
use yams_wiki::InitError;

use crate::DirectCompletion;

/// Process-boundary classification of a typed store or init failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fault {
    /// CLI / service exit status.
    pub exit_code: ExitCode,
    /// Stable machine-readable class.
    pub code: &'static str,
    /// True when retrying the same way may succeed.
    pub transient: bool,
    /// Human-readable detail.
    pub message: String,
}

impl Fault {
    pub fn from_store(error: &StoreError) -> Self {
        let transient = error.is_transient_contention();
        let code = match error {
            StoreError::Busy { .. } => "store_busy",
            StoreError::UnsafeSidecar { .. } => "store_sidecar",
            StoreError::Integrity { .. } => "store_integrity",
            StoreError::RacedStorePath { .. } | StoreError::RacedProjectRoot { .. } => {
                "store_raced"
            }
            StoreError::IncompatibleSchema { .. }
            | StoreError::IncompatibleVectorSchema { .. }
            | StoreError::UnsupportedSchema { .. }
            | StoreError::FutureSchema { .. } => "store_schema",
            StoreError::WrongRoot { .. } => "store_wrong_root",
            _ => "store_operational",
        };
        Self {
            exit_code: ExitCode::Operational,
            code,
            transient,
            message: error.to_string(),
        }
    }

    pub fn from_management(error: &ManagementError) -> Self {
        match error {
            ManagementError::Vector(inner) => Self::from_vector(inner),
            ManagementError::Sync(inner) => Self::from_sync(inner),
            ManagementError::UnsafeSidecar { .. } => Self {
                exit_code: ExitCode::Operational,
                code: "store_sidecar",
                transient: true,
                message: error.to_string(),
            },
            ManagementError::MissingIndex { .. } | ManagementError::MissingVectorCache { .. } => {
                Self {
                    exit_code: ExitCode::Operational,
                    code: "store_missing",
                    transient: false,
                    message: error.to_string(),
                }
            }
            ManagementError::NotSqlite { .. } | ManagementError::InvalidMetadata { .. } => Self {
                exit_code: ExitCode::Operational,
                code: "store_corrupt",
                transient: false,
                message: error.to_string(),
            },
            _ => Self {
                exit_code: ExitCode::Operational,
                code: "store_operational",
                transient: error.is_transient_contention(),
                message: error.to_string(),
            },
        }
    }

    pub fn from_vector(error: &VectorError) -> Self {
        match error {
            VectorError::Store(inner) => Self::from_store(inner),
            _ => Self {
                exit_code: ExitCode::Operational,
                code: "store_operational",
                transient: error.is_transient_contention(),
                message: error.to_string(),
            },
        }
    }

    pub fn from_sync(error: &SyncError) -> Self {
        match error {
            SyncError::Store(inner) => Self::from_store(inner),
            SyncError::Vector(inner) => Self::from_vector(inner),
            _ => Self {
                exit_code: ExitCode::Operational,
                code: "store_operational",
                transient: error.is_transient_contention(),
                message: error.to_string(),
            },
        }
    }

    pub fn from_retrieval(error: &RetrievalError) -> Self {
        match error {
            RetrievalError::VectorCache(inner) => Self::from_vector(inner),
            _ => Self {
                exit_code: ExitCode::Operational,
                code: "store_operational",
                transient: error.is_transient_contention(),
                message: error.to_string(),
            },
        }
    }

    pub fn from_init(error: &InitError) -> Self {
        let (exit_code, code) = match error {
            InitError::InvalidRequest(_)
            | InitError::Conflict(_)
            | InitError::Drift(_)
            | InitError::Json(_) => (ExitCode::Usage, "init_usage"),
            _ => (ExitCode::Operational, "init_operational"),
        };
        Self {
            exit_code,
            code,
            transient: false,
            message: error.to_string(),
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self {
            exit_code: ExitCode::Operational,
            code: "operational",
            transient: false,
            message: message.into(),
        }
    }

    pub fn into_completion(self, json: bool) -> DirectCompletion {
        if json {
            DirectCompletion::classified_failure(self)
        } else {
            DirectCompletion::operational(self.message)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Fault;
    use yams_store::{ManagementError, StoreError};
    use yams_wiki::InitError;

    #[test]
    fn sidecar_and_missing_index_are_distinct_codes() {
        let sidecar = Fault::from_management(&ManagementError::UnsafeSidecar {
            path: PathBuf::from("/tmp/x-journal"),
        });
        let missing = Fault::from_management(&ManagementError::MissingIndex {
            path: PathBuf::from("/tmp/missing"),
        });
        assert_eq!(sidecar.code, "store_sidecar");
        assert!(sidecar.transient);
        assert_eq!(missing.code, "store_missing");
        assert!(!missing.transient);
        assert_eq!(
            Fault::from_store(&StoreError::Busy {
                operation: "open",
                path: PathBuf::from("/tmp/busy"),
            })
            .code,
            "store_busy"
        );
        assert_eq!(
            Fault::from_init(&InitError::InvalidRequest("bad".into())).code,
            "init_usage"
        );
    }

    #[test]
    fn missing_index_json_hint_points_at_offline_index() {
        let completion = Fault::from_management(&ManagementError::MissingIndex {
            path: PathBuf::from("/tmp/missing"),
        })
        .into_completion(true);
        assert_eq!(completion.exit_code, yams_core::ExitCode::Operational);
        let value: serde_json::Value = serde_json::from_str(completion.stdout.trim()).unwrap();
        assert_eq!(value["code"], "store_missing");
        assert_eq!(
            value["hint"],
            "run yams --index from the project; add YAMS_ALLOW_NET=1 only if the model cache is empty"
        );
    }

    #[test]
    fn other_persistent_store_faults_keep_the_generic_index_hint() {
        let completion = Fault::from_management(&ManagementError::NotSqlite {
            path: PathBuf::from("/tmp/corrupt"),
        })
        .into_completion(true);
        let value: serde_json::Value = serde_json::from_str(completion.stdout.trim()).unwrap();
        assert_eq!(value["code"], "store_corrupt");
        assert_eq!(value["hint"], "inspect the index and retry");
    }
}
