//! Optional debug/replay sink: writes solved HTML + cookie metadata to disk so
//! callers can inspect / diff what the solver actually returned.

use crate::types::Solution;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct DebugConfig {
    /// Directory under which artifacts are written. Created on first solve.
    pub directory: PathBuf,
    /// If `true`, also dump the request URL/method as JSON next to the HTML.
    pub include_metadata: bool,
}

impl DebugConfig {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            include_metadata: true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct DebugSink {
    config: DebugConfig,
    counter: Arc<AtomicU64>,
}

impl DebugSink {
    pub fn new(config: DebugConfig) -> Self {
        Self {
            config,
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Best-effort write. Errors are logged at WARN and swallowed — debug
    /// replay must never break the solve path.
    pub async fn write(&self, url: &str, solution: &Solution) {
        if let Err(e) = self.write_inner(url, solution).await {
            warn!("debug sink write failed: {}", e);
        }
    }

    async fn write_inner(&self, url: &str, solution: &Solution) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.config.directory).await?;

        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let stem = format!("{:06}_{}", n, slug_from_url(url));
        let html_path = self.config.directory.join(format!("{}.html", stem));
        let meta_path = self.config.directory.join(format!("{}.json", stem));

        if let Some(html) = &solution.response {
            tokio::fs::write(&html_path, html.as_bytes()).await?;
        }

        if self.config.include_metadata {
            let meta = ReplayMetadata {
                url: url.to_string(),
                status: solution.status,
                user_agent: solution.user_agent.clone(),
                cookies: solution
                    .cookies
                    .iter()
                    .map(|c| ReplayCookie {
                        name: c.name.clone(),
                        value: c.value.clone(),
                        domain: c.domain.clone(),
                        path: c.path.clone(),
                    })
                    .collect(),
                source: source_label(solution),
                solved_at: chrono_unix(&solution.solved_at),
            };
            let json = serde_json::to_vec_pretty(&meta).unwrap_or_default();
            tokio::fs::write(&meta_path, json).await?;
        }

        Ok(())
    }

}

#[derive(Serialize)]
struct ReplayMetadata {
    url: String,
    status: u16,
    user_agent: String,
    cookies: Vec<ReplayCookie>,
    source: &'static str,
    solved_at: u64,
}

#[derive(Serialize)]
struct ReplayCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
}

fn source_label(solution: &Solution) -> &'static str {
    match solution.source {
        crate::types::SolutionSource::Fresh => "fresh",
        crate::types::SolutionSource::Cached { .. } => "cached",
    }
}

fn chrono_unix(t: &std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn slug_from_url(url: &str) -> String {
    let mut s = String::with_capacity(url.len().min(80));
    for c in url.chars().take(80) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    if s.is_empty() {
        "page".to_string()
    } else {
        s
    }
}
