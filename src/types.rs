use serde::{Deserialize, Serialize};

/// Cookie returned after solving a challenge.
#[derive(Debug, Clone, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub expires: Option<f64>,
    #[serde(default, rename = "httpOnly")]
    pub http_only: bool,
    #[serde(default)]
    pub secure: bool,
}

/// Solution returned after solving a challenge.
#[derive(Debug, Clone, Deserialize)]
pub struct Solution {
    pub url: String,
    pub status: u16,
    pub cookies: Vec<Cookie>,
    #[serde(rename = "userAgent")]
    pub user_agent: String,
    /// Fully rendered HTML of the page.
    pub response: String,
}

/// Full response from the /v1 endpoint.
#[derive(Debug, Deserialize)]
pub(crate) struct ApiResponse {
    pub status: String,
    #[serde(default)]
    pub message: String,
    pub solution: Option<Solution>,
}

/// Request body for the /v1 endpoint.
#[derive(Debug, Serialize)]
pub(crate) struct ApiRequest {
    pub cmd: String,
    pub url: String,
    #[serde(rename = "maxTimeout")]
    pub max_timeout: u64,
}

impl Solution {
    /// Format cookies as a `Cookie` header value.
    pub fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }
}
