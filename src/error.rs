use thiserror::Error;

#[derive(Debug, Error)]
pub enum AntibotError {
    #[error("Docker is not installed or not running")]
    DockerNotAvailable,

    #[error("failed to pull image {image}: {reason}")]
    PullFailed { image: String, reason: String },

    #[error("failed to start container: {0}")]
    StartFailed(String),

    #[error("service not reachable at {url} after {attempts} attempts")]
    HealthCheckFailed { url: String, attempts: u32 },

    #[error("challenge failed for {url}: {reason}")]
    ChallengeFailed { url: String, reason: String },

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),
}
