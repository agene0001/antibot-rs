use serde::{Deserialize, Serialize};

/// HTTP cookie used in solve requests and returned in solutions.
///
/// `Deserialize` parses cookies from FlareSolverr/Byparr responses, and
/// `Serialize` lets the caller pre-seed cookies on outgoing solve requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub domain: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    #[serde(default, rename = "httpOnly", skip_serializing_if = "is_false")]
    pub http_only: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub secure: bool,
    #[serde(default, rename = "sameSite", skip_serializing_if = "Option::is_none")]
    pub same_site: Option<SameSite>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl Cookie {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            domain: String::new(),
            path: String::new(),
            expires: None,
            http_only: false,
            secure: false,
            same_site: None,
        }
    }

    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = domain.into();
        self
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    pub fn with_expires(mut self, expires: f64) -> Self {
        self.expires = Some(expires);
        self
    }

    pub fn with_http_only(mut self, http_only: bool) -> Self {
        self.http_only = http_only;
        self
    }

    pub fn with_secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    pub fn with_same_site(mut self, same_site: SameSite) -> Self {
        self.same_site = Some(same_site);
        self
    }
}
