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
    /// HTTP status of the solve that produced this session.
    pub status: u16,
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
    /// Hard cap on cached domains. Once exceeded, entries are evicted in
    /// DashMap shard-iteration order (not LRU) until back under the cap.
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
        if let Some(expires_at) = entry.expires_at
            && Instant::now() >= expires_at
        {
            let key = entry.key().clone();
            drop(entry);
            // Re-check expiry under the removal lock so a fresh session
            // inserted by a racing solve isn't deleted.
            self.entries.remove_if(&key, |_, v| {
                v.expires_at.is_some_and(|e| Instant::now() >= e)
            });
            return None;
        }
        Some(entry.clone())
    }

    pub fn insert(&self, domain: String, cookies: Vec<Cookie>, user_agent: String, status: u16) {
        let expires_at = self.compute_expiry(&cookies);
        let now = Instant::now();
        let session = CachedSession {
            cookies,
            user_agent,
            status,
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
                let from_cookie = Instant::now() + Duration::from_secs_f64(secs_remaining);
                Some(from_cookie.min(default_deadline))
            }
            None => Some(default_deadline),
        }
    }

    fn evict_if_needed(&self) {
        // Cheap eviction: drop the first key the iterator yields until back
        // under the cap. DashMap iteration order is shard-dependent, which is
        // good enough for bounding memory without doing a real LRU.
        while self.entries.len() > self.config.max_entries {
            let Some(victim) = self.entries.iter().next().map(|e| e.key().clone()) else {
                return;
            };
            self.entries.remove(&victim);
        }
    }
}

/// Extract the registrable domain (eTLD+1, e.g. `walmart.com` for
/// `www.walmart.com`) from a URL, so cookies solved on one subdomain are
/// reused across siblings. Falls back to the full host for IPs, `localhost`,
/// and unknown suffixes. Returns `None` if `url` can't be parsed or has no
/// host.
pub(crate) fn extract_domain(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    match parsed.host()? {
        url::Host::Domain(d) => {
            let host = d.to_ascii_lowercase();
            Some(match psl::domain_str(&host) {
                Some(registrable) => registrable.to_string(),
                None => host,
            })
        }
        // IP literals have no registrable domain (and psl would mangle them,
        // e.g. 127.0.0.1 → "0.1"); key on the address itself.
        ip => Some(ip.to_string().to_ascii_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::extract_domain;

    #[test]
    fn registrable_domain_collapses_subdomains() {
        assert_eq!(
            extract_domain("https://www.walmart.com/cart"),
            Some("walmart.com".to_string())
        );
        assert_eq!(
            extract_domain("https://walmart.com/"),
            Some("walmart.com".to_string())
        );
        assert_eq!(
            extract_domain("https://a.b.example.co.uk/x"),
            Some("example.co.uk".to_string())
        );
    }

    #[test]
    fn non_registrable_hosts_fall_back_to_full_host() {
        assert_eq!(
            extract_domain("http://localhost:8191/"),
            Some("localhost".to_string())
        );
        assert_eq!(
            extract_domain("http://127.0.0.1/"),
            Some("127.0.0.1".to_string())
        );
    }

    #[test]
    fn unparseable_urls_return_none() {
        assert_eq!(extract_domain("not a url"), None);
    }
}
