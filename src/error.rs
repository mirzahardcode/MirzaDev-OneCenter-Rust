use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("response could not be decoded: {0}")]
    Json(#[from] serde_json::Error),
    #[error("GitHub API returned HTTP {0}")]
    GitHubStatus(reqwest::StatusCode),
    #[error("preferences could not be read: {0}")]
    PreferencesRead(#[source] std::io::Error),
    #[error("preferences could not be saved: {0}")]
    PreferencesWrite(#[source] std::io::Error),
    #[error("application data directory is unavailable")]
    DataDirectoryUnavailable,
}

pub type AppResult<T> = Result<T, AppError>;
