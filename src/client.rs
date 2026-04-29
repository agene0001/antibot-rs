use crate::coalesce::{CoalesceKey, SolveCoalescer};
use crate::cookie::Cookie;
use crate::debug_replay::{DebugConfig, DebugSink};
use crate::docker::{DockerLimits, DockerManager};
use crate::error::AntibotError;
use crate::metrics::{Metrics, MetricsSnapshot};
use crate::proxy::ProxyConfig;
use crate::request::SolveRequest;
use crate::retry::RetryPolicy;
use crate::session_cache::{extract_domain, SessionCache, SessionCacheConfig};
use crate::types::{ApiResponse, Solution, SolutionSource};
use crate::wire::WireRequest;
use crate::Provider;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Client for solving bot-detection challenges via Byparr/FlareSolverr.
#[derive(Clone)]
pub struct Antibot {
    inner: Arc<AntibotInner>,
}

struct AntibotInner {
    http: reqwest::Client,
    instances: Vec<String>,
    instance_cursor: AtomicUsize,
    max_timeout_ms: u64,
    default_proxy: Option<ProxyConfig>,
    session_cache: Option<SessionCache>,
    coalescer: Option<SolveCoalescer>,
    retry_policy: RetryPolicy,
    metrics: Metrics,
    debug_sink: Option<DebugSink>,

    // Lifecycle / health
    docker_manager: Option<Arc<DockerManager>>,
    manage_lifecycle: bool,
    health_check_attempts_on_recovery: u32,
    watchdog: StdMutex<Option<JoinHandle<()>>>,
    shutdown: Arc<AtomicBool>,
}

impl Antibot {
    /// Create a builder for configuring the client.
    pub fn builder() -> AntibotBuilder {
        AntibotBuilder::default()
    }

    /// Quick constructor that connects to an already-running instance.
    /// Does NOT auto-start Docker.
    pub fn connect(base_url: &str) -> Self {
        Self::connect_many(vec![base_url.to_string()])
    }

    /// Connect to a pool of instances. Requests round-robin across them.
    pub fn connect_many(base_urls: Vec<String>) -> Self {
        let instances: Vec<String> = base_urls
            .into_iter()
            .map(|u| u.trim_end_matches('/').to_string())
            .collect();
        assert!(!instances.is_empty(), "connect_many: empty instance list");

        Self {
            inner: Arc::new(AntibotInner {
                http: build_http_client(),
                instances,
                instance_cursor: AtomicUsize::new(0),
                max_timeout_ms: 60000,
                default_proxy: None,
                session_cache: None,
                coalescer: None,
                retry_policy: RetryPolicy::no_retries(),
                metrics: Metrics::new(),
                debug_sink: None,
                docker_manager: None,
                manage_lifecycle: false,
                health_check_attempts_on_recovery: 15,
                watchdog: StdMutex::new(None),
                shutdown: Arc::new(AtomicBool::new(false)),
            }),
        }
    }

    /// Check if the underlying service is reachable on at least one instance.
    pub async fn is_available(&self) -> bool {
        for url in &self.inner.instances {
            if matches!(self.inner.http.get(url).send().await, Ok(r) if r.status().is_success()) {
                return true;
            }
        }
        false
    }

    /// Convenience: solve a URL with a simple GET.
    pub async fn solve(&self, url: &str) -> Result<Solution, AntibotError> {
        self.execute(SolveRequest::get(url)).await
    }

    /// Force a fresh solve, bypassing the session cache for this URL.
    pub async fn solve_fresh(&self, url: &str) -> Result<Solution, AntibotError> {
        self.execute(SolveRequest::get(url).bypass_cache()).await
    }

    /// Manually drop the cached session for `domain`.
    pub fn invalidate_session(&self, domain: &str) {
        if let Some(cache) = &self.inner.session_cache {
            cache.invalidate(domain);
        }
    }

    /// Drop every cached session.
    pub fn clear_session_cache(&self) {
        if let Some(cache) = &self.inner.session_cache {
            cache.clear();
        }
    }

    /// Number of cached sessions, or 0 if caching is disabled.
    pub fn session_cache_size(&self) -> usize {
        self.inner
            .session_cache
            .as_ref()
            .map(|c| c.len())
            .unwrap_or(0)
    }

    /// Snapshot of all atomic counters.
    pub fn metrics(&self) -> MetricsSnapshot {
        self.inner.metrics.snapshot()
    }

    /// Unified entry point: cache check → coalesce → retry-wrapped dispatch → cache write.
    pub async fn execute(&self, mut request: SolveRequest) -> Result<Solution, AntibotError> {
        if request.proxy.is_none() {
            request.proxy = self.inner.default_proxy.clone();
        }

        let cacheable = request.session_id.is_none()
            && !request.bypass_cache
            && matches!(request.method, crate::request::SolveMethod::Get)
            && request.cookies.is_none();

        if cacheable {
            if let Some(cache) = &self.inner.session_cache {
                if let Some(domain) = extract_domain(&request.url) {
                    if let Some(hit) = cache.get(&domain) {
                        debug!("session cache hit for {}", domain);
                        self.inner.metrics.record_cache_hit();
                        let age = hit.age();
                        return Ok(Solution {
                            url: request.url.clone(),
                            status: 200,
                            cookies: hit.cookies,
                            user_agent: hit.user_agent,
                            response: None,
                            solved_at: hit.solved_at_system,
                            source: SolutionSource::Cached { age },
                        });
                    }
                }
            }
        }

        let coalesce_key = self
            .inner
            .coalescer
            .as_ref()
            .and_then(|c| c.key_for(&request.url));

        let solver = || async { self.execute_uncoalesced(&request, cacheable).await };

        match (&self.inner.coalescer, coalesce_key) {
            (Some(coalescer), Some(key)) => {
                self.inner.metrics.record_coalesced_wait();
                coalescer.solve_or_wait(key, solver).await
            }
            _ => solver().await,
        }
    }

    /// Hit the provider with retries, update the session cache on success.
    async fn execute_uncoalesced(
        &self,
        request: &SolveRequest,
        cacheable: bool,
    ) -> Result<Solution, AntibotError> {
        let policy = &self.inner.retry_policy;
        let mut last_err: Option<AntibotError> = None;

        for attempt in 1..=policy.max_attempts {
            let backoff = policy.backoff_for_attempt(attempt);
            if !backoff.is_zero() {
                tokio::time::sleep(backoff).await;
                self.inner.metrics.record_retry();
            }

            self.inner.metrics.record_attempt();
            let started = Instant::now();
            match self.dispatch(request).await {
                Ok(solution) => {
                    let elapsed = started.elapsed().as_millis() as u64;
                    self.inner.metrics.record_success(elapsed);

                    if cacheable {
                        if let Some(cache) = &self.inner.session_cache {
                            if let Some(domain) = extract_domain(&request.url) {
                                cache.insert(
                                    domain,
                                    solution.cookies.clone(),
                                    solution.user_agent.clone(),
                                );
                            }
                        }
                    }

                    if let Some(sink) = &self.inner.debug_sink {
                        sink.write(&request.url, &solution).await;
                    }

                    return Ok(solution);
                }
                Err(e) => {
                    self.inner.metrics.record_failure();
                    let retryable = policy.is_retryable(&e) && attempt < policy.max_attempts;
                    if retryable {
                        warn!(
                            "solve attempt {}/{} failed: {} (retrying)",
                            attempt, policy.max_attempts, e
                        );
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            AntibotError::UnexpectedResponse("retry loop exited without result".into())
        }))
    }

    /// Pick the next instance round-robin and round-trip to its `/v1`.
    fn next_instance_url(&self) -> &str {
        let n = self.inner.instances.len();
        let idx = if n == 1 {
            0
        } else {
            self.inner.instance_cursor.fetch_add(1, Ordering::Relaxed) % n
        };
        &self.inner.instances[idx]
    }

    async fn dispatch(&self, request: &SolveRequest) -> Result<Solution, AntibotError> {
        let wire = WireRequest::from_solve(request, self.inner.max_timeout_ms);
        let base = self.next_instance_url();

        info!(
            "[{}] solving {} ({} cookies pre-seeded, proxy={})",
            base,
            request.url,
            request.cookies.as_ref().map(|c| c.len()).unwrap_or(0),
            request.proxy.is_some()
        );

        let resp = self
            .inner
            .http
            .post(format!("{}/v1", base))
            .json(&wire)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AntibotError::UnexpectedResponse(format!(
                "HTTP {}: {}",
                status,
                &body[..body.len().min(500)]
            )));
        }

        let api_resp: ApiResponse = resp.json().await?;

        if api_resp.status != "ok" {
            error!("challenge failed: {}", api_resp.message);
            return Err(AntibotError::ChallengeFailed {
                url: request.url.clone(),
                reason: api_resp.message,
            });
        }

        let wire_solution = api_resp.solution.ok_or_else(|| {
            AntibotError::UnexpectedResponse("status ok but no solution returned".into())
        })?;

        let solution = Solution::from_wire(wire_solution);
        debug!(
            "solved with {} cookies, status={}",
            solution.cookies.len(),
            solution.status
        );
        info!("solved {} — status {}", request.url, solution.status);

        Ok(solution)
    }

    /// Create a persistent browser session on the provider.
    pub async fn create_session(&self) -> Result<SessionHandle, AntibotError> {
        self.create_session_with(None, None).await
    }

    pub async fn create_session_with(
        &self,
        session_id: Option<String>,
        proxy: Option<ProxyConfig>,
    ) -> Result<SessionHandle, AntibotError> {
        let wire = WireRequest::sessions_create(session_id.clone(), proxy);
        let base = self.next_instance_url().to_string();

        let resp = self
            .inner
            .http
            .post(format!("{}/v1", base))
            .json(&wire)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AntibotError::UnexpectedResponse(format!(
                "HTTP {}: {}",
                status,
                &body[..body.len().min(500)]
            )));
        }

        let api_resp: ApiResponse = resp.json().await?;
        if api_resp.status != "ok" {
            return Err(AntibotError::ChallengeFailed {
                url: "<sessions.create>".to_string(),
                reason: api_resp.message,
            });
        }

        let id = api_resp
            .session
            .or(session_id)
            .ok_or_else(|| AntibotError::UnexpectedResponse("no session id returned".into()))?;

        info!("created session {}", id);

        Ok(SessionHandle {
            id,
            antibot: self.clone(),
            destroyed: false,
        })
    }

    /// Tear down a provider session by id.
    pub async fn destroy_session(&self, id: &str) -> Result<(), AntibotError> {
        let wire = WireRequest::sessions_destroy(id.to_string());
        let base = self.next_instance_url().to_string();

        let resp = self
            .inner
            .http
            .post(format!("{}/v1", base))
            .json(&wire)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AntibotError::UnexpectedResponse(format!(
                "HTTP {}: {}",
                status,
                &body[..body.len().min(500)]
            )));
        }

        let api_resp: ApiResponse = resp.json().await?;
        if api_resp.status != "ok" {
            return Err(AntibotError::SessionNotFound(api_resp.message));
        }
        info!("destroyed session {}", id);
        Ok(())
    }

    /// Build a reqwest `Client` pre-configured with a solved user-agent.
    pub fn build_http_client(user_agent: &str) -> Result<reqwest::Client, AntibotError> {
        use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE};

        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
        );
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));

        reqwest::Client::builder()
            .user_agent(user_agent)
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(AntibotError::Http)
    }

    /// Spawn a watchdog task that pings each instance every `interval` and
    /// triggers a container restart on failure (only when this client owns
    /// the lifecycle).
    fn spawn_health_watchdog(&self, interval: Duration) {
        let inner = self.inner.clone();

        let handle = tokio::spawn(async move {
            let watchdog_client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    warn!("watchdog: failed to build http client: {}", e);
                    return;
                }
            };

            loop {
                if inner.shutdown.load(Ordering::Relaxed) {
                    return;
                }

                tokio::time::sleep(interval).await;

                if inner.shutdown.load(Ordering::Relaxed) {
                    return;
                }

                let mut any_unhealthy = false;
                for url in &inner.instances {
                    match watchdog_client.get(url).send().await {
                        Ok(r) if r.status().is_success() => {}
                        Ok(r) => {
                            warn!("watchdog: {} returned {}", url, r.status());
                            any_unhealthy = true;
                        }
                        Err(_) => {
                            warn!("watchdog: {} unreachable", url);
                            any_unhealthy = true;
                        }
                    }
                }

                if !any_unhealthy {
                    continue;
                }

                let Some(manager) = &inner.docker_manager else {
                    continue;
                };

                warn!(
                    "watchdog: restarting container '{}'",
                    manager.container_name()
                );
                inner.metrics.record_container_restart();

                if let Err(e) = manager.start().await {
                    warn!("watchdog: failed to (re)start: {}", e);
                    continue;
                }
                if let Err(e) = manager
                    .wait_healthy(inner.health_check_attempts_on_recovery)
                    .await
                {
                    warn!("watchdog: still unhealthy after restart: {}", e);
                }
            }
        });

        if let Ok(mut slot) = self.inner.watchdog.lock() {
            *slot = Some(handle);
        }
    }
}

fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("failed to build HTTP client")
}

impl Drop for AntibotInner {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);

        if let Ok(mut slot) = self.watchdog.lock() {
            if let Some(handle) = slot.take() {
                handle.abort();
            }
        }

        if !self.manage_lifecycle {
            return;
        }
        let Some(manager) = self.docker_manager.clone() else {
            return;
        };

        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                tokio::spawn(async move {
                    if let Err(e) = manager.stop().await {
                        warn!("failed to stop container on drop: {}", e);
                    }
                });
            }
            Err(_) => {
                debug!("dropping outside tokio runtime; container will leak");
            }
        }
    }
}

/// Handle to a provider-side persistent session. Drops auto-destroy the session
/// on a background task; call [`SessionHandle::destroy`] explicitly to await.
pub struct SessionHandle {
    id: String,
    antibot: Antibot,
    destroyed: bool,
}

impl SessionHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn execute(&self, request: SolveRequest) -> Result<Solution, AntibotError> {
        let req = request.with_session(self.id.clone()).bypass_cache();
        self.antibot.execute(req).await
    }

    pub async fn solve(&self, url: &str) -> Result<Solution, AntibotError> {
        self.execute(SolveRequest::get(url)).await
    }

    pub async fn destroy(mut self) -> Result<(), AntibotError> {
        self.destroyed = true;
        self.antibot.destroy_session(&self.id).await
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        if self.destroyed {
            return;
        }
        let id = self.id.clone();
        let antibot = self.antibot.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                tokio::spawn(async move {
                    if let Err(e) = antibot.destroy_session(&id).await {
                        warn!("failed to destroy session {} on drop: {}", id, e);
                    }
                });
            }
            Err(_) => {
                debug!(
                    "session {} dropped outside a tokio runtime; provider will GC it",
                    id
                );
            }
        }
    }
}

/// Apply additional cookies to an existing solution.
pub fn merge_cookies(base: &mut Vec<Cookie>, extra: Vec<Cookie>) {
    for c in extra {
        if let Some(existing) = base
            .iter_mut()
            .find(|b| b.name == c.name && b.domain == c.domain)
        {
            *existing = c;
        } else {
            base.push(c);
        }
    }
}

/// Builder for configuring and initializing an [`Antibot`] client.
pub struct AntibotBuilder {
    provider: Provider,
    port: u16,
    auto_start: bool,
    container_name: Option<String>,
    max_timeout_ms: u64,
    health_check_attempts: u32,
    default_proxy: Option<ProxyConfig>,
    session_cache_config: Option<SessionCacheConfig>,
    coalesce_key: Option<CoalesceKey>,
    retry_policy: RetryPolicy,
    debug_config: Option<DebugConfig>,
    docker_limits: DockerLimits,
    extra_instances: Vec<String>,
    manage_lifecycle: bool,
    health_watch_interval: Option<Duration>,
}

impl Default for AntibotBuilder {
    fn default() -> Self {
        Self {
            provider: Provider::Byparr,
            port: 8191,
            auto_start: false,
            container_name: None,
            max_timeout_ms: 60000,
            health_check_attempts: 15,
            default_proxy: None,
            session_cache_config: None,
            coalesce_key: None,
            retry_policy: RetryPolicy::no_retries(),
            debug_config: None,
            docker_limits: DockerLimits::default(),
            extra_instances: Vec::new(),
            manage_lifecycle: false,
            health_watch_interval: None,
        }
    }
}

impl AntibotBuilder {
    pub fn provider(mut self, provider: Provider) -> Self {
        self.provider = provider;
        self
    }

    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn auto_start(mut self, enabled: bool) -> Self {
        self.auto_start = enabled;
        self
    }

    pub fn container_name(mut self, name: impl Into<String>) -> Self {
        self.container_name = Some(name.into());
        self
    }

    pub fn max_timeout_ms(mut self, ms: u64) -> Self {
        self.max_timeout_ms = ms;
        self
    }

    pub fn health_check_attempts(mut self, attempts: u32) -> Self {
        self.health_check_attempts = attempts;
        self
    }

    pub fn default_proxy(mut self, proxy: ProxyConfig) -> Self {
        self.default_proxy = Some(proxy);
        self
    }

    /// Enable session caching with default config (30 min TTL, 1000 entries).
    pub fn enable_session_cache(mut self) -> Self {
        self.session_cache_config = Some(SessionCacheConfig::default());
        self
    }

    pub fn session_cache(mut self, config: SessionCacheConfig) -> Self {
        self.session_cache_config = Some(config);
        self
    }

    /// Coalesce concurrent solves that share the same key.
    pub fn coalesce_solves(mut self, key: CoalesceKey) -> Self {
        self.coalesce_key = Some(key);
        self
    }

    /// Wrap each provider call in a retry policy.
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Enable disk dumps of every solved page.
    pub fn debug(mut self, config: DebugConfig) -> Self {
        self.debug_config = Some(config);
        self
    }

    /// Apply Docker resource caps when creating a managed container.
    pub fn docker_limits(mut self, limits: DockerLimits) -> Self {
        self.docker_limits = limits;
        self
    }

    /// Add an additional pre-existing instance URL. Combined with the
    /// `auto_start`/`port` instance, requests round-robin across all of them.
    pub fn add_instance(mut self, base_url: impl Into<String>) -> Self {
        self.extra_instances
            .push(base_url.into().trim_end_matches('/').to_string());
        self
    }

    /// Stop the spawned container when the client is dropped.
    pub fn manage_lifecycle(mut self, enabled: bool) -> Self {
        self.manage_lifecycle = enabled;
        self
    }

    /// Run a background watchdog that restarts the container if a health check
    /// fails. Only takes effect when `auto_start` is on.
    pub fn health_watch(mut self, interval: Duration) -> Self {
        self.health_watch_interval = Some(interval);
        self
    }

    pub async fn build(self) -> Result<Antibot, AntibotError> {
        let primary_url = format!("http://localhost:{}", self.port);
        let mut docker_manager: Option<Arc<DockerManager>> = None;

        if self.auto_start {
            let mut manager = DockerManager::new(self.provider, self.port);
            if let Some(name) = self.container_name {
                manager = manager.with_container_name(name);
            }
            manager = manager.with_limits(self.docker_limits);

            if !manager.is_docker_available().await {
                return Err(AntibotError::DockerNotAvailable);
            }

            manager.start().await?;
            manager.wait_healthy(self.health_check_attempts).await?;
            docker_manager = Some(Arc::new(manager));
        }

        let mut instances = vec![primary_url];
        instances.extend(self.extra_instances);

        let inner = AntibotInner {
            http: build_http_client(),
            instances,
            instance_cursor: AtomicUsize::new(0),
            max_timeout_ms: self.max_timeout_ms,
            default_proxy: self.default_proxy,
            session_cache: self.session_cache_config.map(SessionCache::new),
            coalescer: self.coalesce_key.map(SolveCoalescer::new),
            retry_policy: self.retry_policy,
            metrics: Metrics::new(),
            debug_sink: self.debug_config.map(DebugSink::new),
            docker_manager,
            manage_lifecycle: self.manage_lifecycle,
            health_check_attempts_on_recovery: self.health_check_attempts,
            watchdog: StdMutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
        };

        let client = Antibot {
            inner: Arc::new(inner),
        };

        if let Some(interval) = self.health_watch_interval {
            if client.inner.docker_manager.is_some() {
                client.spawn_health_watchdog(interval);
            } else {
                debug!("health_watch ignored: no docker_manager (auto_start disabled)");
            }
        }

        Ok(client)
    }
}
