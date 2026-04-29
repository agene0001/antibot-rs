use crate::cookie::Cookie;
use crate::fingerprint::BrowserFingerprint;
use crate::proxy::ProxyConfig;
use std::collections::HashMap;

/// A request to solve a challenge.
///
/// Use [`SolveRequest::get`] / [`SolveRequest::post`] to construct, then chain
/// `with_*` methods to attach headers, cookies, a session, or a proxy.
#[derive(Debug, Clone)]
pub struct SolveRequest {
    pub url: String,
    pub method: SolveMethod,
    pub headers: Option<HashMap<String, String>>,
    pub cookies: Option<Vec<Cookie>>,
    pub max_timeout_ms: Option<u64>,
    pub session_id: Option<String>,
    pub proxy: Option<ProxyConfig>,
    /// If true, bypass the session cache for this request only.
    pub bypass_cache: bool,
    /// Optional browser fingerprint hints (only honored by Byparr-class providers).
    pub fingerprint: Option<BrowserFingerprint>,
}

#[derive(Debug, Clone)]
pub enum SolveMethod {
    Get,
    Post { body: PostBody },
}

#[derive(Debug, Clone)]
pub enum PostBody {
    /// `application/x-www-form-urlencoded` body.
    Form(HashMap<String, String>),
    /// JSON body sent as `application/json`.
    Json(serde_json::Value),
    /// Raw body with caller-specified content type.
    Raw {
        content_type: String,
        body: Vec<u8>,
    },
}

impl SolveRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: SolveMethod::Get,
            headers: None,
            cookies: None,
            max_timeout_ms: None,
            session_id: None,
            proxy: None,
            bypass_cache: false,
            fingerprint: None,
        }
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: SolveMethod::Post {
                body: PostBody::Form(HashMap::new()),
            },
            headers: None,
            cookies: None,
            max_timeout_ms: None,
            session_id: None,
            proxy: None,
            bypass_cache: false,
            fingerprint: None,
        }
    }

    /// Set a form body. Replaces any existing body.
    pub fn form<K, V, I>(mut self, fields: I) -> Self
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        let map = fields.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        self.method = SolveMethod::Post {
            body: PostBody::Form(map),
        };
        self
    }

    /// Set a JSON body. Replaces any existing body.
    pub fn json(mut self, value: serde_json::Value) -> Self {
        self.method = SolveMethod::Post {
            body: PostBody::Json(value),
        };
        self
    }

    /// Set a raw body. Replaces any existing body.
    pub fn raw_body(mut self, content_type: impl Into<String>, body: Vec<u8>) -> Self {
        self.method = SolveMethod::Post {
            body: PostBody::Raw {
                content_type: content_type.into(),
                body,
            },
        };
        self
    }

    pub fn with_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = Some(headers);
        self
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .get_or_insert_with(HashMap::new)
            .insert(name.into(), value.into());
        self
    }

    pub fn with_cookies(mut self, cookies: Vec<Cookie>) -> Self {
        self.cookies = Some(cookies);
        self
    }

    pub fn with_cookie(mut self, cookie: Cookie) -> Self {
        self.cookies.get_or_insert_with(Vec::new).push(cookie);
        self
    }

    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.max_timeout_ms = Some(ms);
        self
    }

    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_proxy(mut self, proxy: ProxyConfig) -> Self {
        self.proxy = Some(proxy);
        self
    }

    pub fn bypass_cache(mut self) -> Self {
        self.bypass_cache = true;
        self
    }

    pub fn with_fingerprint(mut self, fingerprint: BrowserFingerprint) -> Self {
        self.fingerprint = Some(fingerprint);
        self
    }
}
