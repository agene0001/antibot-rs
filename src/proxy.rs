use serde::Serialize;

/// HTTP/SOCKS proxy passed through to the underlying solver (Byparr/FlareSolverr).
#[derive(Debug, Clone, Serialize)]
pub struct ProxyConfig {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl ProxyConfig {
    /// Create a proxy with a full URL (may already include credentials).
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            username: None,
            password: None,
        }
    }

    /// Convenience for HTTP proxies.
    pub fn http(url: impl Into<String>) -> Self {
        Self::new(url)
    }

    /// Attach credentials (preferred over embedding them in the URL).
    pub fn with_auth(mut self, user: impl Into<String>, pass: impl Into<String>) -> Self {
        self.username = Some(user.into());
        self.password = Some(pass.into());
        self
    }
}
