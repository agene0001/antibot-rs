//! Per-domain session cache so previously-solved cookies can be reused
//! without round-tripping through the (slow) solver.

use crate::cookie::Cookie;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct CachedSession {
    pub cookies: Vec<Cookie>,
    pub user_agent: String,
    pub solved_at: Instant,
    pub solved_at_system: SystemTime,
    pub expires_at: Option<Instant>,
}

impl CachedSession {
    pub fn age(&self) -> Duration {
        self.solved_at.elapsed()
    }
}

#[derive(Debug, Clone)]
pub struct SessionCacheConfig {
    /// TTL applied when the underlying cookies have no usable expiry.
    pub default_ttl: Duration,
    /// Hard cap on cached domains. Soft eviction via random sampling once hit.
    pub max_entries: usize,
    /// If `true`, derive `expires_at` from the soonest cookie expiry.
    pub respect_cookie_expiry: bool,
}

impl Default for SessionCacheConfig {
    fn default() -> Self {
        Self {
            default_ttl: Duration::from_secs(30 * 60),
            max_entries: 1000,
            respect_cookie_expiry: true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SessionCache {
    entries: Arc<DashMap<String, CachedSession>>,
    config: SessionCacheConfig,
}

impl SessionCache {
    pub fn new(config: SessionCacheConfig) -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
            config,
        }
    }

    pub fn get(&self, domain: &str) -> Option<CachedSession> {
        let entry = self.entries.get(domain)?;
        if let Some(expires_at) = entry.expires_at {
            if Instant::now() >= expires_at {
                let key = entry.key().clone();
                drop(entry);
                self.entries.remove(&key);
                return None;
            }
        }
        Some(entry.clone())
    }

    pub fn insert(&self, domain: String, cookies: Vec<Cookie>, user_agent: String) {
        let expires_at = self.compute_expiry(&cookies);
        let now = Instant::now();
        let session = CachedSession {
            cookies,
            user_agent,
            solved_at: now,
            solved_at_system: SystemTime::now(),
            expires_at,
        };
        self.entries.insert(domain, session);
        self.evict_if_needed();
    }

    pub fn invalidate(&self, domain: &str) {
        self.entries.remove(domain);
    }

    pub fn clear(&self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn compute_expiry(&self, cookies: &[Cookie]) -> Option<Instant> {
        let default_deadline = Instant::now() + self.config.default_ttl;

        if !self.config.respect_cookie_expiry {
            return Some(default_deadline);
        }

        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        let earliest_cookie = cookies
            .iter()
            .filter_map(|c| c.expires)
            .filter(|&exp| exp > now_unix)
            .fold(None, |acc: Option<f64>, exp| {
                Some(acc.map_or(exp, |a| a.min(exp)))
            });

        match earliest_cookie {
            Some(exp) => {
                let secs_remaining = (exp - now_unix).max(0.0);
                let from_cookie =
                    Instant::now() + Duration::from_secs_f64(secs_remaining);
                Some(from_cookie.min(default_deadline))
            }
            None => Some(default_deadline),
        }
    }

    fn evict_if_needed(&self) {
        if self.entries.len() <= self.config.max_entries {
            return;
        }

        // Cheap eviction: take the first key the iterator yields and drop it.
        // DashMap iteration order is shard-dependent, which is good enough for
        // bounding memory without doing a real LRU.
        if let Some(victim) = self.entries.iter().next().map(|e| e.key().clone()) {
            self.entries.remove(&victim);
        }
    }
}

/// Extract the registrable domain from a URL. Returns `None` if `url` can't be
/// parsed or has no host.
pub(crate) fn extract_domain(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed.host_str().map(|s| s.to_ascii_lowercase())
}
