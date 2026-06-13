use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AntibotError {
    #[error("Docker is not installed or not running")]
    DockerNotAvailable,

    #[error("failed to start the Docker daemon: {0}")]
    DaemonStartFailed(String),

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

    /// The provider's `/v1` endpoint answered with a non-success HTTP status.
    /// Retried only for 429 and 5xx; 4xx is treated as deterministic.
    #[error("provider returned HTTP {status}: {body}")]
    ProviderHttp { status: u16, body: String },

    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),

    /// The configured provider does not implement this feature (e.g. sessions
    /// on upstream Byparr).
    #[error("{provider} does not support {feature}")]
    UnsupportedFeature { provider: String, feature: String },

    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// A coalesced peer's solve failed; the original error is stringified
    /// because the underlying error type isn't Clone.
    #[error("coalesced solve failed: {0}")]
    CoalescedFailure(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),
}
