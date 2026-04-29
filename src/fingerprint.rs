//! Browser fingerprint configuration for solve requests.
//!
//! Only Byparr-class providers honor these fields; FlareSolverr ignores them.

use serde::Serialize;

#[derive(Debug, Clone, Default)]
pub struct BrowserFingerprint {
    pub user_agent: Option<String>,
    pub locale: Option<String>,
    pub viewport: Option<Viewport>,
    pub timezone: Option<String>,
    pub platform: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

impl BrowserFingerprint {
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }
    pub fn locale(mut self, locale: impl Into<String>) -> Self {
        self.locale = Some(locale.into());
        self
    }
    pub fn viewport(mut self, width: u32, height: u32) -> Self {
        self.viewport = Some(Viewport { width, height });
        self
    }
    pub fn timezone(mut self, timezone: impl Into<String>) -> Self {
        self.timezone = Some(timezone.into());
        self
    }
    pub fn platform(mut self, platform: impl Into<String>) -> Self {
        self.platform = Some(platform.into());
        self
    }

    pub fn is_empty(&self) -> bool {
        self.user_agent.is_none()
            && self.locale.is_none()
            && self.viewport.is_none()
            && self.timezone.is_none()
            && self.platform.is_none()
    }
}
