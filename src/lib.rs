//! Auto-managed Byparr/FlareSolverr client for bypassing bot detection.
//!
//! Provides a unified client for the FlareSolverr-compatible API used by both
//! [Byparr](https://github.com/sergerdn/byparr) and
//! [FlareSolverr](https://github.com/FlareSolverr/FlareSolverr).
//!
//! # Features
//! - Automatic Docker container lifecycle (pull, start, health-wait)
//! - Provider-agnostic: works with Byparr, FlareSolverr, or any compatible image
//! - Simple `solve(url)` API that returns rendered HTML + cookies
//!
//! # Example
//! ```no_run
//! use antibot::{Antibot, Provider};
//!
//! # async fn example() -> Result<(), antibot::AntibotError> {
//! let client = Antibot::builder()
//!     .provider(Provider::Byparr)
//!     .auto_start(true)
//!     .build()
//!     .await?;
//!
//! let solution = client.solve("https://example.com").await?;
//! println!("Got {} bytes of HTML", solution.response.len());
//! # Ok(())
//! # }
//! ```

mod client;
mod docker;
mod error;
mod types;

pub use client::{Antibot, AntibotBuilder};
pub use error::AntibotError;
pub use types::*;

/// Docker image provider for the challenge-solving proxy.
#[derive(Debug, Clone)]
pub enum Provider {
    /// Byparr — recommended, actively maintained.
    Byparr,
    /// FlareSolverr — the original implementation.
    FlareSolverr,
    /// Custom Docker image with a FlareSolverr-compatible `/v1` endpoint.
    Custom(String),
}

impl Provider {
    /// Docker image reference for this provider.
    pub fn image(&self) -> &str {
        match self {
            Provider::Byparr => "ghcr.io/sergerdn/byparr:latest",
            Provider::FlareSolverr => "ghcr.io/flaresolverr/flaresolverr:latest",
            Provider::Custom(image) => image,
        }
    }

    /// Human-readable label.
    pub fn label(&self) -> &str {
        match self {
            Provider::Byparr => "Byparr",
            Provider::FlareSolverr => "FlareSolverr",
            Provider::Custom(_) => "Custom",
        }
    }
}

impl Default for Provider {
    fn default() -> Self {
        Provider::Byparr
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.label(), self.image())
    }
}
