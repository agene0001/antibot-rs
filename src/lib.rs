//! Auto-managed Byparr/FlareSolverr client for bypassing bot detection.
//!
//! Provides a unified client for the FlareSolverr-compatible API used by both
//! [Byparr](https://github.com/thephaseless/byparr) and
//! [FlareSolverr](https://github.com/FlareSolverr/FlareSolverr).
//!
//! # Features
//! - Automatic Docker container lifecycle (pull, start, health-wait, restart, drop)
//! - Provider-agnostic: Byparr, FlareSolverr, or any compatible image
//! - Builder-style [`SolveRequest`] with GET/POST, headers, cookies, proxy, sessions, fingerprint
//! - Per-domain session/cookie cache so repeat solves are free
//! - Concurrent-solve coalescer that dedupes parallel solves
//! - Retry policy with exponential backoff
//! - Lock-free [`MetricsSnapshot`] for observability
//! - Optional disk-replay sink for debugging
//! - Round-robin across multiple instances
//! - `solve_stream` for batch use with bounded concurrency
//! - Standalone [`detect`] helpers for cheap challenge fingerprinting
//!
//! # Quick start
//! ```no_run
//! use antibot_rs::{Antibot, Provider};
//!
//! # async fn example() -> Result<(), antibot_rs::AntibotError> {
//! let client = Antibot::builder()
//!     .provider(Provider::Byparr)
//!     .auto_start(true)
//!     .enable_session_cache()
//!     .build()
//!     .await?;
//!
//! let solution = client.solve("https://example.com").await?;
//! println!("Got {} bytes of HTML", solution.html().len());
//! # Ok(())
//! # }
//! ```
//!
//! # Full-featured example
//! ```no_run
//! use antibot_rs::{
//!     Antibot, CoalesceKey, Cookie, DebugConfig, DockerLimits, ProxyConfig,
//!     Provider, RetryPolicy, SolveRequest,
//! };
//! use std::time::Duration;
//!
//! # async fn example() -> Result<(), antibot_rs::AntibotError> {
//! let client = Antibot::builder()
//!     .provider(Provider::Byparr)
//!     .auto_start(true)
//!     .docker_limits(DockerLimits::default().memory("2g").cpus("1.5").shm_size("1g"))
//!     .enable_session_cache()
//!     .coalesce_solves(CoalesceKey::Domain)
//!     .retry(RetryPolicy::default())
//!     .default_proxy(ProxyConfig::http("http://proxy.example:8080"))
//!     .debug(DebugConfig::new("./antibot-replay"))
//!     .health_watch(Duration::from_secs(30))
//!     .manage_lifecycle(true)
//!     .build()
//!     .await?;
//!
//! let solution = client.execute(
//!     SolveRequest::post("https://site.com/api/login")
//!         .json(serde_json::json!({"user": "alice"}))
//!         .with_header("X-Custom", "value")
//!         .with_cookie(Cookie::new("session", "abc123"))
//! ).await?;
//! # let _ = solution;
//! # Ok(())
//! # }
//! ```

mod client;
mod coalesce;
mod cookie;
mod debug_replay;
pub mod detect;
mod docker;
mod error;
mod fingerprint;
mod metrics;
mod proxy;
mod request;
mod retry;
mod session_cache;
mod stream;
mod types;
mod wire;

pub use client::{merge_cookies, Antibot, AntibotBuilder, SessionHandle};
pub use coalesce::CoalesceKey;
pub use cookie::{Cookie, SameSite};
pub use debug_replay::DebugConfig;
pub use detect::{detect_challenge, ChallengeKind, DetectionInput};
pub use docker::DockerLimits;
pub use error::AntibotError;
pub use fingerprint::{BrowserFingerprint, Viewport};
pub use metrics::MetricsSnapshot;
pub use proxy::ProxyConfig;
pub use request::{PostBody, SolveMethod, SolveRequest};
pub use retry::RetryPolicy;
pub use session_cache::{CachedSession, SessionCacheConfig};
pub use stream::SolveStream;
pub use types::{Solution, SolutionSource};

/// Re-export of `futures::StreamExt` so callers don't need a direct dep
/// just to consume [`Antibot::solve_stream`].
pub use futures::StreamExt;

/// Docker image provider for the challenge-solving proxy.
#[derive(Debug, Clone, Default)]
pub enum Provider {
    /// Byparr — recommended, actively maintained.
    #[default]
    Byparr,
    /// FlareSolverr — the original implementation.
    FlareSolverr,
    /// Custom Docker image with a FlareSolverr-compatible `/v1` endpoint.
    Custom(String),
}

impl Provider {
    pub fn image(&self) -> &str {
        match self {
            Provider::Byparr => "ghcr.io/thephaseless/byparr:latest",
            Provider::FlareSolverr => "ghcr.io/flaresolverr/flaresolverr:latest",
            Provider::Custom(image) => image,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Provider::Byparr => "Byparr",
            Provider::FlareSolverr => "FlareSolverr",
            Provider::Custom(_) => "Custom",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.label(), self.image())
    }
}
