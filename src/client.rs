use crate::Provider;
use crate::coalesce::{CoalesceKey, SolveCoalescer};
use crate::cookie::Cookie;
use crate::debug_replay::{DebugConfig, DebugSink};
use crate::docker::{DockerLimits, DockerManager};
use crate::error::AntibotError;
use crate::metrics::{Metrics, MetricsSnapshot};
use crate::proxy::ProxyConfig;
use crate::request::{PostBody, SolveMethod, SolveRequest};
use crate::retry::RetryPolicy;
use crate::session_cache::{SessionCache, SessionCacheConfig, extract_domain};
use crate::types::{ApiResponse, Solution, SolutionSource};
use crate::wire::WireRequest;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// One solver endpoint plus its live load accounting, used to route to the
/// least-busy instance and optionally cap concurrent solves per instance.
struct Instance {
    base_url: String,
    /// Solves currently dispatched to (or queued for) this instance.
    inflight: AtomicUsize,
    /// Present when a per-instance concurrency cap is configured; provides
    /// backpressure so a single browser isn't overwhelmed.
    sem: Option<Arc<Semaphore>>,
}

impl Instance {
    fn new(base_url: String, max_inflight: Option<usize>) -> Self {
        Self {
            base_url,
            inflight: AtomicUsize::new(0),
            sem: max_inflight.map(|n| Arc::new(Semaphore::new(n.max(1)))),
        }
    }
}

/// Held for the duration of a dispatch: keeps the instance's inflight count
/// raised and (when capped) holds the concurrency permit. Both are released on
/// drop, including on cancellation.
struct InstanceLease {
    inner: Arc<AntibotInner>,
    idx: usize,
    _permit: Option<OwnedSemaphorePermit>,
}

impl InstanceLease {
    fn base_url(&self) -> &str {
        &self.inner.instances[self.idx].base_url
    }
}

impl Drop for InstanceLease {
    fn drop(&mut self) {
        self.inner.instances[self.idx]
            .inflight
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// Client for solving bot-detection challenges via Byparr/FlareSolverr.
#[derive(Clone)]
pub struct Antibot {
    inner: Arc<AntibotInner>,
}

struct AntibotInner {
    http: reqwest::Client,
    instances: Vec<Instance>,
    /// Rotating offset for tie-breaking among equally-loaded instances, so
    /// idle pools still round-robin instead of always picking index 0.
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

    /// Set when this client started the container itself, so we know which
    /// provider is on the other end and can warn about compat gaps.
    provider_hint: Option<Provider>,
    provider_compat_warned: AtomicBool,
}

/// Extra slack on top of the solver's own `maxTimeout` before the HTTP call
/// to the provider is abandoned.
const PROVIDER_TIMEOUT_MARGIN: Duration = Duration::from_secs(30);

/// Truncate to at most `max` bytes without splitting a UTF-8 code point.
fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl Antibot {
    /// Create a builder for configuring the client.
    pub fn builder() -> AntibotBuilder {
        AntibotBuilder::default()
    }

    /// Quick constructor that connects to an already-running instance.
    /// Does NOT auto-start Docker.
    ///
    /// No provider is assumed, so compatibility warnings are off. Use
    /// [`Antibot::connect_with`] to opt into per-provider checks when you know
    /// what's behind the URL.
    pub fn connect(base_url: &str) -> Self {
        Self::from_instances(vec![base_url.trim_end_matches('/').to_string()], None)
    }

    /// Like [`Antibot::connect`], but declares which provider is behind the URL
    /// so per-provider compatibility checks (warnings and the Byparr feature
    /// errors) apply to a remote instance you didn't auto-start.
    pub fn connect_with(base_url: &str, provider: Provider) -> Self {
        Self::from_instances(
            vec![base_url.trim_end_matches('/').to_string()],
            Some(provider),
        )
    }

    /// Connect to a pool of instances. Requests round-robin across them.
    ///
    /// Returns [`AntibotError::InvalidConfig`] when `base_urls` is empty.
    pub fn connect_many(base_urls: Vec<String>) -> Result<Self, AntibotError> {
        Self::connect_many_inner(base_urls, None)
    }

    /// Like [`Antibot::connect_many`], but declares the provider behind the
    /// pool for per-provider compatibility checks.
    pub fn connect_many_with(
        base_urls: Vec<String>,
        provider: Provider,
    ) -> Result<Self, AntibotError> {
        Self::connect_many_inner(base_urls, Some(provider))
    }

    fn connect_many_inner(
        base_urls: Vec<String>,
        provider_hint: Option<Provider>,
    ) -> Result<Self, AntibotError> {
        let instances: Vec<String> = base_urls
            .into_iter()
            .map(|u| u.trim_end_matches('/').to_string())
            .collect();
        if instances.is_empty() {
            return Err(AntibotError::InvalidConfig(
                "connect_many: empty instance list".into(),
            ));
        }
        Ok(Self::from_instances(instances, provider_hint))
    }

    /// Internal constructor; `instances` must be non-empty.
    fn from_instances(instances: Vec<String>, provider_hint: Option<Provider>) -> Self {
        Self {
            inner: Arc::new(AntibotInner {
                http: build_http_client(),
                instances: instances
                    .into_iter()
                    .map(|u| Instance::new(u, None))
                    .collect(),
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
                provider_hint,
                provider_compat_warned: AtomicBool::new(false),
            }),
        }
    }

    /// Check if the underlying service is reachable on at least one instance.
    pub async fn is_available(&self) -> bool {
        for instance in &self.inner.instances {
            let probe = self
                .inner
                .http
                .get(&instance.base_url)
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            if matches!(probe, Ok(r) if r.status().is_success()) {
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

    /// Gracefully shut down: stops the health watchdog and, when this client
    /// manages the container lifecycle, stops the container and waits for it.
    ///
    /// `Drop` performs the same cleanup on a fire-and-forget background task,
    /// which is usually *dropped unfinished* when the tokio runtime is itself
    /// shutting down (the common case at the end of `main`). Call this for
    /// deterministic teardown. Affects all clones of this client.
    pub async fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);

        if let Ok(mut slot) = self.inner.watchdog.lock()
            && let Some(handle) = slot.take()
        {
            handle.abort();
        }

        if self.inner.manage_lifecycle
            && let Some(manager) = &self.inner.docker_manager
            && let Err(e) = manager.stop().await
        {
            warn!("shutdown: failed to stop container: {}", e);
        }
    }

    /// Unified entry point: cache check → coalesce → retry-wrapped dispatch → cache write.
    pub async fn execute(&self, request: SolveRequest) -> Result<Solution, AntibotError> {
        self.execute_inner(request, None).await
    }

    async fn execute_inner(
        &self,
        mut request: SolveRequest,
        pinned_instance: Option<&str>,
    ) -> Result<Solution, AntibotError> {
        // Upstream Byparr silently runs a POST as a GET (its model is GET-only),
        // so a "submitted" form would never actually be sent. Fail fast like
        // create_session does, rather than warn-and-mislead.
        if matches!(self.inner.provider_hint, Some(Provider::Byparr))
            && matches!(request.method, SolveMethod::Post { .. })
        {
            return Err(AntibotError::UnsupportedFeature {
                provider: "Byparr".to_string(),
                feature: "POST requests".to_string(),
            });
        }

        let explicit_proxy = request.proxy.is_some();
        if request.proxy.is_none() {
            request.proxy = self.inner.default_proxy.clone();
        }

        // A request may share a solve with strangers only when nothing about
        // it is caller-specific: plain GET, no session, no pre-seeded cookies,
        // no per-request proxy or fingerprint. Coalescing anything else hands
        // the waiter a result that was produced under different conditions
        // (or, for POSTs, silently skips sending the waiter's body).
        let shareable = request.session_id.is_none()
            && matches!(request.method, SolveMethod::Get)
            && request.cookies.is_none()
            && request.fingerprint.is_none()
            && !explicit_proxy
            && pinned_instance.is_none();

        // `bypass_cache` skips the read but NOT the write: a fresh solve must
        // replace the (presumably stale) cached entry, otherwise the next
        // plain solve would serve the very cookies the caller just rejected.
        let cache_read = shareable && !request.bypass_cache;
        let cache_write = shareable;

        if cache_read
            && let Some(cache) = &self.inner.session_cache
            && let Some(domain) = extract_domain(&request.url)
            && let Some(hit) = cache.get(&domain)
        {
            debug!("session cache hit for {}", domain);
            self.inner.metrics.record_cache_hit();
            let age = hit.age();
            return Ok(Solution {
                url: request.url.clone(),
                status: hit.status,
                cookies: hit.cookies,
                user_agent: hit.user_agent,
                response: None,
                solved_at: hit.solved_at_system,
                source: SolutionSource::Cached { age },
            });
        }

        let coalesce_key = if shareable {
            self.inner
                .coalescer
                .as_ref()
                .and_then(|c| c.key_for(&request.url))
        } else {
            None
        };

        let solver = || async {
            self.execute_uncoalesced(&request, cache_write, pinned_instance)
                .await
        };

        match (&self.inner.coalescer, coalesce_key) {
            (Some(coalescer), Some(key)) => coalescer.solve_or_wait(key, solver).await,
            _ => solver().await,
        }
    }

    /// Hit the provider with retries, update the session cache on success.
    async fn execute_uncoalesced(
        &self,
        request: &SolveRequest,
        cache_write: bool,
        pinned_instance: Option<&str>,
    ) -> Result<Solution, AntibotError> {
        let policy = &self.inner.retry_policy;
        let mut last_err: Option<AntibotError> = None;

        for attempt in 1..=policy.max_attempts {
            if attempt > 1 {
                self.inner.metrics.record_retry();
                let backoff = policy.backoff_for_attempt(attempt);
                if !backoff.is_zero() {
                    tokio::time::sleep(backoff).await;
                }
            }

            self.inner.metrics.record_attempt();
            let started = Instant::now();
            match self.dispatch(request, pinned_instance).await {
                Ok(solution) => {
                    let elapsed = started.elapsed().as_millis() as u64;
                    self.inner.metrics.record_success(elapsed);

                    if cache_write
                        && let Some(cache) = &self.inner.session_cache
                        && let Some(domain) = extract_domain(&request.url)
                    {
                        cache.insert(
                            domain,
                            solution.cookies.clone(),
                            solution.user_agent.clone(),
                            solution.status,
                        );
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

    /// Index of the least-loaded instance, breaking ties round-robin so an
    /// all-idle pool still spreads evenly.
    fn pick_instance(&self) -> usize {
        let instances = &self.inner.instances;
        let n = instances.len();
        if n == 1 {
            return 0;
        }
        let start = self.inner.instance_cursor.fetch_add(1, Ordering::Relaxed) % n;
        let mut best = start;
        let mut best_load = instances[start].inflight.load(Ordering::Relaxed);
        for k in 1..n {
            let i = (start + k) % n;
            let load = instances[i].inflight.load(Ordering::Relaxed);
            if load < best_load {
                best = i;
                best_load = load;
            }
        }
        best
    }

    /// Reserve an instance for a dispatch: pick the target (the pinned one for
    /// sessions, else least-loaded), raise its inflight count, and acquire its
    /// concurrency permit if a per-instance cap is set (awaiting under
    /// backpressure). The returned lease releases both on drop.
    async fn lease_instance(&self, pinned_instance: Option<&str>) -> InstanceLease {
        let idx = match pinned_instance {
            Some(url) => self
                .inner
                .instances
                .iter()
                .position(|i| i.base_url == url)
                .unwrap_or_else(|| self.pick_instance()),
            None => self.pick_instance(),
        };

        // Count the request as load before (possibly) blocking on a permit, so
        // concurrent pickers route around a saturated instance.
        self.inner.instances[idx]
            .inflight
            .fetch_add(1, Ordering::Relaxed);

        let permit = match &self.inner.instances[idx].sem {
            Some(sem) => Some(
                sem.clone()
                    .acquire_owned()
                    .await
                    .expect("instance semaphore is never closed"),
            ),
            None => None,
        };

        InstanceLease {
            inner: self.inner.clone(),
            idx,
            _permit: permit,
        }
    }

    /// POST a wire request to an instance's `/v1`, with a timeout derived from
    /// the solver-side `maxTimeout` plus a margin (overrides the client-level
    /// timeout, so per-request timeouts above 120s work).
    async fn post_v1(&self, base: &str, wire: &WireRequest) -> Result<ApiResponse, AntibotError> {
        let timeout = Duration::from_millis(wire.max_timeout) + PROVIDER_TIMEOUT_MARGIN;
        let resp = self
            .inner
            .http
            .post(format!("{}/v1", base))
            .timeout(timeout)
            .json(wire)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(AntibotError::ProviderHttp {
                status,
                body: truncate_utf8(&body, 500).to_string(),
            });
        }

        Ok(resp.json().await?)
    }

    /// Warn (once per client) when the request uses features the configured
    /// provider is known to ignore server-side.
    fn warn_provider_compat(&self, request: &SolveRequest) {
        let unsupported: Vec<&str> = match &self.inner.provider_hint {
            Some(Provider::FlareSolverr) => {
                let mut v = Vec::new();
                if request.headers.is_some() {
                    v.push("custom headers");
                }
                if matches!(
                    &request.method,
                    SolveMethod::Post {
                        body: PostBody::Json(_) | PostBody::Raw { .. }
                    }
                ) {
                    v.push("non-form POST bodies");
                }
                if request.fingerprint.is_some() {
                    v.push("browser fingerprints");
                }
                v
            }
            // Upstream Byparr's request model accepts only cmd/url/max_timeout
            // ("currently only supports GET requests"); everything else is
            // silently dropped by the server. (POST and sessions are hard
            // errors handled before dispatch, so they're not listed here.)
            Some(Provider::Byparr) => {
                let mut v = Vec::new();
                if request.headers.is_some() {
                    v.push("custom headers");
                }
                if request.cookies.is_some() {
                    v.push("pre-seeded cookies");
                }
                if request.proxy.is_some() {
                    v.push("proxies");
                }
                if request.fingerprint.is_some() {
                    v.push("browser fingerprints");
                }
                if request.session_id.is_some() {
                    v.push("sessions");
                }
                if request.return_only_cookies {
                    v.push("returnOnlyCookies");
                }
                v
            }
            _ => return,
        };

        if unsupported.is_empty()
            || self
                .inner
                .provider_compat_warned
                .swap(true, Ordering::Relaxed)
        {
            return;
        }
        warn!(
            "{} does not support: {} — these request fields will be ignored \
             by the server (warning logged once)",
            self.inner
                .provider_hint
                .as_ref()
                .map(|p| p.label())
                .unwrap_or("provider"),
            unsupported.join(", ")
        );
    }

    async fn dispatch(
        &self,
        request: &SolveRequest,
        pinned_instance: Option<&str>,
    ) -> Result<Solution, AntibotError> {
        let wire = WireRequest::from_solve(request, self.inner.max_timeout_ms);
        let lease = self.lease_instance(pinned_instance).await;
        let base = lease.base_url();

        self.warn_provider_compat(request);

        info!(
            "[{}] solving {} ({} cookies pre-seeded, proxy={})",
            base,
            request.url,
            request.cookies.as_ref().map(|c| c.len()).unwrap_or(0),
            request.proxy.is_some()
        );

        let api_resp = self.post_v1(base, &wire).await?;

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
        // Upstream Byparr has no sessions.create; it would solve its default
        // URL and return no session id. Fail fast with a clear error instead.
        if matches!(self.inner.provider_hint, Some(Provider::Byparr)) {
            return Err(AntibotError::UnsupportedFeature {
                provider: "Byparr".to_string(),
                feature: "sessions".to_string(),
            });
        }

        let wire = WireRequest::sessions_create(session_id.clone(), proxy);
        // Sessions live inside a single provider instance, so the handle pins
        // every subsequent request to the instance that created it.
        let lease = self.lease_instance(None).await;
        let base = lease.base_url().to_string();

        let api_resp = self.post_v1(&base, &wire).await?;
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

        info!("created session {} on {}", id, base);

        Ok(SessionHandle {
            id,
            instance_url: base,
            antibot: self.clone(),
            destroyed: false,
        })
    }

    /// Tear down a provider session by id.
    ///
    /// Note: with multiple instances this picks one least-loaded; prefer
    /// [`SessionHandle::destroy`], which targets the instance that owns the
    /// session.
    pub async fn destroy_session(&self, id: &str) -> Result<(), AntibotError> {
        let base = {
            let lease = self.lease_instance(None).await;
            lease.base_url().to_string()
        };
        self.destroy_session_on(&base, id).await
    }

    async fn destroy_session_on(&self, base: &str, id: &str) -> Result<(), AntibotError> {
        let wire = WireRequest::sessions_destroy(id.to_string());

        let api_resp = self.post_v1(base, &wire).await?;
        if api_resp.status != "ok" {
            return Err(AntibotError::SessionNotFound(api_resp.message));
        }
        info!("destroyed session {}", id);
        Ok(())
    }

    /// Build a reqwest `Client` pre-configured with a solved user-agent.
    pub fn build_http_client(user_agent: &str) -> Result<reqwest::Client, AntibotError> {
        use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue};

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

                let Some(manager) = &inner.docker_manager else {
                    return;
                };

                // Only probe the instance this client's container actually
                // backs; extra remote instances being down is not a reason to
                // restart the local container.
                let url = manager.base_url();
                let unhealthy = match watchdog_client.get(&url).send().await {
                    Ok(r) if r.status().is_success() => false,
                    Ok(r) => {
                        warn!("watchdog: {} returned {}", url, r.status());
                        true
                    }
                    Err(_) => {
                        warn!("watchdog: {} unreachable", url);
                        true
                    }
                };

                if !unhealthy {
                    continue;
                }

                warn!(
                    "watchdog: restarting container '{}'",
                    manager.container_name()
                );
                inner.metrics.record_container_restart();

                // restart(), not start(): a hung container is still "running",
                // so start() would be a no-op.
                if let Err(e) = manager.restart().await {
                    warn!("watchdog: failed to restart: {}", e);
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
        // If `shutdown()` already ran, the watchdog is aborted and (when we
        // manage the lifecycle) the container is already stopped; don't spawn
        // a redundant `docker stop`.
        let already_shut_down = self.shutdown.swap(true, Ordering::Relaxed);

        if let Ok(mut slot) = self.watchdog.lock()
            && let Some(handle) = slot.take()
        {
            handle.abort();
        }

        if already_shut_down || !self.manage_lifecycle {
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
///
/// All requests through the handle are pinned to the instance that created the
/// session — provider sessions are not shared across instances.
pub struct SessionHandle {
    id: String,
    instance_url: String,
    antibot: Antibot,
    destroyed: bool,
}

impl SessionHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Base URL of the instance that owns this session.
    pub fn instance_url(&self) -> &str {
        &self.instance_url
    }

    pub async fn execute(&self, request: SolveRequest) -> Result<Solution, AntibotError> {
        let req = request.with_session(self.id.clone()).bypass_cache();
        self.antibot
            .execute_inner(req, Some(&self.instance_url))
            .await
    }

    pub async fn solve(&self, url: &str) -> Result<Solution, AntibotError> {
        self.execute(SolveRequest::get(url)).await
    }

    pub async fn destroy(mut self) -> Result<(), AntibotError> {
        self.destroyed = true;
        self.antibot
            .destroy_session_on(&self.instance_url, &self.id)
            .await
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        if self.destroyed {
            return;
        }
        let id = self.id.clone();
        let base = self.instance_url.clone();
        let antibot = self.antibot.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                tokio::spawn(async move {
                    if let Err(e) = antibot.destroy_session_on(&base, &id).await {
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

/// Apply additional cookies to an existing solution. Cookies are matched by
/// the RFC 6265 identity tuple (name, domain, path) so `/` and `/api` cookies
/// of the same name coexist instead of clobbering each other.
pub fn merge_cookies(base: &mut Vec<Cookie>, extra: Vec<Cookie>) {
    for c in extra {
        if let Some(existing) = base
            .iter_mut()
            .find(|b| b.name == c.name && b.domain == c.domain && b.path == c.path)
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
    max_inflight_per_instance: Option<usize>,
    start_docker_daemon: bool,
    daemon_start_override: Option<(String, Vec<String>)>,
    daemon_start_timeout: Duration,
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
            max_inflight_per_instance: None,
            start_docker_daemon: false,
            daemon_start_override: None,
            daemon_start_timeout: Duration::from_secs(60),
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

    /// Number of health-check polls (2s apart) to wait for the service on
    /// startup. Clamped to a minimum of 1.
    pub fn health_check_attempts(mut self, attempts: u32) -> Self {
        self.health_check_attempts = attempts.max(1);
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

    /// When `auto_start` is on and the Docker daemon isn't running, attempt to
    /// start it (and wait for it to become ready) before creating the
    /// container. Off by default.
    ///
    /// Uses a per-OS default: Docker Desktop on macOS/Windows, `systemctl start
    /// docker` on Linux. The Linux default needs privileges — for rootless
    /// Docker, Colima, OrbStack, etc. supply your own command with
    /// [`docker_daemon_start_command`](Self::docker_daemon_start_command). Has
    /// no effect without `auto_start`.
    pub fn start_docker_daemon(mut self, enabled: bool) -> Self {
        self.start_docker_daemon = enabled;
        self
    }

    /// Override the command used to start the Docker daemon (implies
    /// [`start_docker_daemon(true)`](Self::start_docker_daemon)). For example,
    /// `("colima", ["start"])` or `("sudo", ["systemctl", "start", "docker"])`.
    pub fn docker_daemon_start_command(
        mut self,
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.start_docker_daemon = true;
        self.daemon_start_override =
            Some((program.into(), args.into_iter().map(Into::into).collect()));
        self
    }

    /// How long to wait for the daemon to become ready after starting it
    /// (default 60s — Docker Desktop boots a VM).
    pub fn daemon_start_timeout(mut self, timeout: Duration) -> Self {
        self.daemon_start_timeout = timeout;
        self
    }

    /// Add an additional pre-existing instance URL. Combined with the
    /// `auto_start`/`port` instance, requests are routed least-loaded-first
    /// across all of them.
    pub fn add_instance(mut self, base_url: impl Into<String>) -> Self {
        self.extra_instances
            .push(base_url.into().trim_end_matches('/').to_string());
        self
    }

    /// Cap the number of solves dispatched concurrently to any single instance.
    /// Since each solver runs one headless browser, a low value (1–2) keeps the
    /// browser busy without thrashing; excess solves wait (backpressure) rather
    /// than piling onto the server's internal queue. Unset = no client-side cap.
    pub fn max_inflight_per_instance(mut self, max: usize) -> Self {
        self.max_inflight_per_instance = Some(max.max(1));
        self
    }

    /// Stop the spawned container when the client is dropped (best-effort;
    /// prefer [`Antibot::shutdown`] for deterministic teardown).
    ///
    /// Caution: if two clients `auto_start` with the same container name,
    /// dropping one with this enabled stops the container out from under the
    /// other. Give each client a distinct
    /// [`container_name`](AntibotBuilder::container_name) (and port) instead.
    pub fn manage_lifecycle(mut self, enabled: bool) -> Self {
        self.manage_lifecycle = enabled;
        self
    }

    /// Run a background watchdog that restarts the container if a health check
    /// fails. Only takes effect when `auto_start` is on. The interval is
    /// clamped to a minimum of 1s to avoid a hot poll loop.
    pub fn health_watch(mut self, interval: Duration) -> Self {
        self.health_watch_interval = Some(interval.max(Duration::from_secs(1)));
        self
    }

    pub async fn build(self) -> Result<Antibot, AntibotError> {
        let primary_url = format!("http://localhost:{}", self.port);
        let mut docker_manager: Option<Arc<DockerManager>> = None;
        let mut provider_hint = None;

        if self.auto_start {
            let mut manager = DockerManager::new(self.provider.clone(), self.port);
            if let Some(name) = self.container_name {
                manager = manager.with_container_name(name);
            }
            manager = manager.with_limits(self.docker_limits);

            if self.start_docker_daemon {
                manager
                    .ensure_running(
                        self.daemon_start_override.as_ref(),
                        self.daemon_start_timeout,
                    )
                    .await?;
            } else if !manager.is_docker_available().await {
                return Err(AntibotError::DockerNotAvailable);
            }

            manager.start().await?;
            manager.wait_healthy(self.health_check_attempts).await?;
            docker_manager = Some(Arc::new(manager));
            // We started the container, so we know what's behind the URL.
            provider_hint = Some(self.provider);
        }

        let mut instance_urls = vec![primary_url];
        instance_urls.extend(self.extra_instances);
        let cap = self.max_inflight_per_instance;
        let instances: Vec<Instance> = instance_urls
            .into_iter()
            .map(|u| Instance::new(u, cap))
            .collect();

        let metrics = Metrics::new();
        let inner = AntibotInner {
            http: build_http_client(),
            instances,
            instance_cursor: AtomicUsize::new(0),
            max_timeout_ms: self.max_timeout_ms,
            default_proxy: self.default_proxy,
            session_cache: self.session_cache_config.map(SessionCache::new),
            coalescer: self
                .coalesce_key
                .map(|key| SolveCoalescer::new(key, metrics.clone())),
            retry_policy: self.retry_policy,
            metrics,
            debug_sink: self.debug_config.map(DebugSink::new),
            docker_manager,
            manage_lifecycle: self.manage_lifecycle,
            health_check_attempts_on_recovery: self.health_check_attempts,
            watchdog: StdMutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
            provider_hint,
            provider_compat_warned: AtomicBool::new(false),
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
