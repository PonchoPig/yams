//! Invocation-scoped time capture threaded through direct execution.

/// Invocation-scoped time values. The CLI captures one per top-level
/// invocation; the service captures one per accepted request; tests inject.
#[derive(Clone, Debug)]
pub struct InvocationTime {
    /// Process-local civil date consumed by `write` (YYYY-MM-DD).
    pub civil_date: String,
    /// Exact `YYYY-MM-DDTHH:MM:SS.mmmZ` form consumed by query logging.
    pub utc_timestamp: String,
}

impl InvocationTime {
    /// Capture both values from the running process clock.
    pub fn capture() -> Self {
        Self {
            civil_date: chrono::Local::now().date_naive().to_string(),
            utc_timestamp: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
        }
    }
}
