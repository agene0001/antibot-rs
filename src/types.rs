use crate::cookie::Cookie;
use serde::Deserialize;
use std::time::{Duration, SystemTime};

/// Where this solution came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolutionSource {
    /// A fresh solve was performed by the underlying provider.
    Fresh,
    /// Returned from the in-process session cache.
    Cached { age: Duration },
}

impl SolutionSource {
    pub fn is_cached(&self) -> bool {
        matches!(self, SolutionSource::Cached { .. })
    }
}

/// Result of a solve. `response` is `None` for cache hits when only the
/// cookies/user-agent were preserved; check [`SolutionSource`] to disambiguate.
#[derive(Debug, Clone)]
pub struct Solution {
    pub url: String,
    pub status: u16,
    pub cookies: Vec<Cookie>,
    pub user_agent: String,
    /// Fully rendered HTML of the page. `None` on session-cache hits.
    pub response: Option<String>,
    pub solved_at: SystemTime,
    pub source: SolutionSource,
}

impl Solution {
    /// Format cookies as a `Cookie` header value, skipping any cookie with an
    /// expiry in the past. A non-positive `expires` (e.g. `-1`) is the
    /// FlareSolverr/Byparr convention for a session cookie and is kept.
    pub fn cookie_header(&self) -> String {
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        self.cookies
            .iter()
            .filter(|c| c.expires.is_none_or(|e| e <= 0.0 || e > now))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// HTML body, or empty string if absent.
    pub fn html(&self) -> &str {
        self.response.as_deref().unwrap_or("")
    }

    pub(crate) fn from_wire(s: WireSolution) -> Self {
        Self {
            url: s.url,
            status: s.status,
            cookies: s.cookies,
            user_agent: s.user_agent,
            // Providers omit or blank the body for returnOnlyCookies solves;
            // normalize that to None so callers get one shape to check.
            response: (!s.response.is_empty()).then_some(s.response),
            solved_at: SystemTime::now(),
            source: SolutionSource::Fresh,
        }
    }
}

/// Wire-format solution as returned by FlareSolverr/Byparr `/v1`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireSolution {
    pub url: String,
    pub status: u16,
    pub cookies: Vec<Cookie>,
    #[serde(rename = "userAgent")]
    pub user_agent: String,
    #[serde(default)]
    pub response: String,
}

/// Full response from the `/v1` endpoint.
#[derive(Debug, Deserialize)]
pub(crate) struct ApiResponse {
    pub status: String,
    #[serde(default)]
    pub message: String,
    pub solution: Option<WireSolution>,
    /// Returned for session.create (and similar lifecycle calls).
    #[serde(default)]
    pub session: Option<String>,
}
