//! Wire-format types for the FlareSolverr-compatible `/v1` endpoint.
//!
//! Kept private — public API uses [`crate::request::SolveRequest`].

use crate::cookie::Cookie;
use crate::fingerprint::Viewport;
use crate::proxy::ProxyConfig;
use crate::request::{PostBody, SolveMethod, SolveRequest};
use serde::Serialize;
use std::collections::HashMap;

/// Request body sent to `/v1`.
#[derive(Debug, Serialize)]
pub(crate) struct WireRequest {
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "maxTimeout")]
    pub max_timeout: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "postData")]
    pub post_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookies: Option<Vec<Cookie>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyConfig>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "userAgent")]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Viewport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

impl WireRequest {
    /// Translate a public [`SolveRequest`] into the wire shape, falling back
    /// to `default_timeout_ms` if the request didn't set its own.
    pub fn from_solve(req: &SolveRequest, default_timeout_ms: u64) -> Self {
        let cmd = match &req.method {
            SolveMethod::Get => "request.get".to_string(),
            SolveMethod::Post { .. } => "request.post".to_string(),
        };

        let post_data = match &req.method {
            SolveMethod::Get => None,
            SolveMethod::Post { body } => Some(encode_post_body(body)),
        };

        // request.post needs a Content-Type header for non-form bodies.
        let mut headers = req.headers.clone();
        if let SolveMethod::Post { body } = &req.method {
            let content_type = match body {
                PostBody::Form(_) => "application/x-www-form-urlencoded",
                PostBody::Json(_) => "application/json",
                PostBody::Raw { content_type, .. } => content_type.as_str(),
            };
            let h = headers.get_or_insert_with(HashMap::new);
            h.entry("Content-Type".to_string())
                .or_insert_with(|| content_type.to_string());
        }

        let (user_agent, locale, viewport, timezone, platform) = match &req.fingerprint {
            Some(fp) => (
                fp.user_agent.clone(),
                fp.locale.clone(),
                fp.viewport,
                fp.timezone.clone(),
                fp.platform.clone(),
            ),
            None => (None, None, None, None, None),
        };

        Self {
            cmd,
            url: Some(req.url.clone()),
            max_timeout: req.max_timeout_ms.unwrap_or(default_timeout_ms),
            session: req.session_id.clone(),
            post_data,
            cookies: req.cookies.clone(),
            headers,
            proxy: req.proxy.clone(),
            user_agent,
            locale,
            viewport,
            timezone,
            platform,
        }
    }

    /// `sessions.create` body.
    pub fn sessions_create(session_id: Option<String>, proxy: Option<ProxyConfig>) -> Self {
        Self {
            cmd: "sessions.create".to_string(),
            url: None,
            max_timeout: 60000,
            session: session_id,
            post_data: None,
            cookies: None,
            headers: None,
            proxy,
            user_agent: None,
            locale: None,
            viewport: None,
            timezone: None,
            platform: None,
        }
    }

    /// `sessions.destroy` body.
    pub fn sessions_destroy(session_id: String) -> Self {
        Self {
            cmd: "sessions.destroy".to_string(),
            url: None,
            max_timeout: 60000,
            session: Some(session_id),
            post_data: None,
            cookies: None,
            headers: None,
            proxy: None,
            user_agent: None,
            locale: None,
            viewport: None,
            timezone: None,
            platform: None,
        }
    }
}

fn encode_post_body(body: &PostBody) -> String {
    match body {
        PostBody::Form(map) => url_encode_form(map),
        PostBody::Json(value) => value.to_string(),
        PostBody::Raw { body, .. } => String::from_utf8_lossy(body).into_owned(),
    }
}

fn url_encode_form(map: &HashMap<String, String>) -> String {
    let mut entries: Vec<(&String, &String)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}
