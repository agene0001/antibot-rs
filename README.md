# antibot-rs

Auto-managed [Byparr](https://github.com/ThePhaseless/byparr) / [FlareSolverr](https://github.com/FlareSolverr/FlareSolverr) client for bypassing bot detection in Rust web scrapers.

## Features

- **Provider-agnostic** — Byparr, FlareSolverr, or any compatible Docker image
- **Docker lifecycle** — auto-pull, start, health-wait, optional auto-restart on failure, optional stop-on-drop
- **Resource limits** — memory / CPU / shm-size caps on the spawned container
- **Rich `SolveRequest`** — GET/POST, custom headers and cookies, per-request proxy, browser fingerprint hints
- **Per-domain session cache** — repeat solves for the same domain are cookie-cache hits
- **Concurrent-solve coalescer** — N parallel solves for one domain → 1 provider call
- **Retry policy** — exponential backoff with jitter
- **Persistent sessions** — `create_session()` returns a `SessionHandle` for multi-step flows
- **Metrics** — lock-free counters: success rate, avg solve time, cache hits, coalesced waits, retries, restarts
- **Debug / replay sink** — dump every solved page + cookie metadata to disk for inspection
- **Multi-instance pool** — round-robin across multiple solver containers
- **`solve_stream`** — bounded-concurrency stream of solved pages for batch jobs
- **Standalone challenge detection** — cheap `detect_challenge(...)` helpers, no solver call

## When to use this

This crate is the right choice when you're scraping from sites that:
- Block requests with **Cloudflare** ("Just a moment…", `cf_clearance` cookie)
- Throw **Turnstile** / **DataDome** / **PerimeterX** / **Akamai** / **Imperva** challenges
- Require a fully-rendered page (JS execution, cookies issued by JS) before serving content

It is **not** the right tool for:
- Sites that block on **TLS fingerprint** (JA3/JA4) — you need [`rquest`](https://crates.io/crates/rquest) or curl-impersonate; the headless browser inside Byparr/FlareSolverr leaks its own TLS shape
- **IP-based blocking** / rate limits — pair this with a proxy via `default_proxy()` or per-request `with_proxy()`
- Sites that require **passing a CAPTCHA** (hCaptcha, reCAPTCHA v2 image grids) — Byparr handles invisible challenges, but image-CAPTCHAs need a paid solver
- Low-latency/high-throughput cases — every uncached solve takes 5–30 seconds because a real browser is loading the page

## Comparison with alternatives

| Tool | Language | Approach | Where it fits |
|---|---|---|---|
| **antibot-rs** *(this crate)* | Rust client → Byparr/FlareSolverr | Headless browser via local Docker | Rust scrapers that need a typed, async, lifecycle-managed solver |
| FlareSolverr (raw HTTP) | n/a | Headless browser | Same as above, but you write your own client / lifecycle |
| Byparr (raw HTTP) | n/a | Modern FlareSolverr fork | Recommended provider; this crate's default |
| `rquest` | Rust | TLS fingerprint impersonation | Sites that block on JA3/JA4 — much faster than a full browser |
| `cloudscraper` (Python) | Python | JS interpreter, no browser | Older, mostly broken against current Cloudflare |
| `undetected-chromedriver` (Python) | Python | Patched Chromedriver | Long-running scrapers in Python |
| Paid services (ScraperAPI, ZenRows, Bright Data) | n/a | Hosted solver pool | When self-hosting Docker isn't an option |

## Quick start

```rust
use antibot_rs::{Antibot, Provider};

let client = Antibot::builder()
    .provider(Provider::Byparr)
    .auto_start(true)
    .enable_session_cache()
    .build()
    .await?;

let solution = client.solve("https://example.com").await?;
println!("HTML length: {}", solution.html().len());
println!("Cookies: {:?}", solution.cookies);
```

## Full-featured client

```rust
use antibot_rs::{
    Antibot, CoalesceKey, Cookie, DebugConfig, DockerLimits, ProxyConfig,
    Provider, RetryPolicy, SolveRequest,
};
use std::time::Duration;

let client = Antibot::builder()
    .provider(Provider::Byparr)
    .auto_start(true)
    .docker_limits(
        DockerLimits::default()
            .memory("2g")
            .cpus("1.5")
            .shm_size("1g"),         // Chrome benefits from 1g of shm
    )
    .enable_session_cache()
    .coalesce_solves(CoalesceKey::Domain)
    .retry(RetryPolicy::default())   // 3 attempts, exponential backoff with jitter
    .default_proxy(ProxyConfig::http("http://proxy.example:8080").with_auth("u", "p"))
    .debug(DebugConfig::new("./antibot-replay"))
    .health_watch(Duration::from_secs(30))   // restart container if it stops responding
    .manage_lifecycle(true)                  // stop container when client drops
    .build()
    .await?;

// POST with custom headers, cookies, and a per-request proxy override.
let solution = client.execute(
    SolveRequest::post("https://site.com/api/login")
        .json(serde_json::json!({"user": "alice"}))
        .with_header("X-Custom", "value")
        .with_cookie(Cookie::new("session", "abc123"))
        .with_proxy(ProxyConfig::http("http://other-proxy:8080"))
).await?;

// Inspect runtime stats.
let m = client.metrics();
println!(
    "{} succeeded / {} attempted, avg {:.0} ms, cache hits: {}, retries: {}",
    m.solves_succeeded, m.solves_attempted, m.avg_solve_time_ms, m.cache_hits, m.retries,
);
```

## Persistent sessions

```rust
use antibot_rs::SolveRequest;

let session = client.create_session().await?;

let _login = session.execute(
    SolveRequest::post("https://site.com/login")
        .form([("user", "alice"), ("pass", "secret")])
).await?;

let dash = session.solve("https://site.com/dashboard").await?;
println!("status: {}", dash.status);

session.destroy().await?;   // or just let it drop
```

## Batch with bounded concurrency

```rust
use antibot_rs::StreamExt;

let urls = vec![
    "https://a.com/p1".into(),
    "https://a.com/p2".into(),
    "https://b.com/p1".into(),
];

let mut stream = client.solve_stream(urls, /* concurrency */ 4);
while let Some((url, result)) = stream.next().await {
    match result {
        Ok(solution) => println!("{}: {} bytes", url, solution.html().len()),
        Err(e)       => eprintln!("{}: {}", url, e),
    }
}
```

## Multi-instance pool

```rust
let client = Antibot::builder()
    .auto_start(true)
    .add_instance("http://other-host:8191")
    .add_instance("http://third-host:8191")
    .build()
    .await?;
// Requests now round-robin across all three URLs.
```

## Challenge detection (no solver call)

Cheap header + body inspection, useful as a gate before invoking the (slow) solver:

```rust
use antibot_rs::{detect_challenge, DetectionInput};
use http::HeaderMap;

let headers = HeaderMap::new();
let body = "<html><body>Just a moment...</body></html>";
let input = DetectionInput::new(403, &headers, body, "https://example.com");

if let Some(kind) = detect_challenge(&input) {
    println!("blocked by: {}", kind.name());
    // ... now hand off to antibot.solve()
}
```

Detects: Cloudflare, Turnstile, DataDome, PerimeterX, Akamai, Imperva, reCAPTCHA.

## Connect to an existing instance

```rust
let client = Antibot::connect("http://localhost:8191");
let solution = client.solve("https://example.com").await?;
```

## Requirements

- Docker installed and accessible (only when `auto_start` is on)
- One of: Byparr, FlareSolverr, or any compatible image with a `/v1` endpoint

## Troubleshooting

**`DockerNotAvailable` on `build()`**
You enabled `auto_start(true)` but the Docker daemon isn't running. Either start Docker Desktop, or
use `Antibot::connect("http://...")` against an externally-managed instance.

**`HealthCheckFailed` after `auto_start`**
The container started but isn't responding on the host port. Most common causes:
1. Port collision — something else is bound to `8191`. Pick another with `.port(...)`.
2. Image is still warming up — bump `.health_check_attempts(30)` (each attempt waits 2 seconds).
3. ARM64 host with an x86-only image — pull a multi-arch tag, or use `Provider::Custom(...)`.

**Container immediately exits on slow / low-memory hosts**
Chrome inside the container needs more shared memory than Docker's default 64MB. Apply
`DockerLimits::default().shm_size("1g")`.

**Solves succeed but cookies don't authenticate downstream requests**
The `Cookie` header alone isn't enough — most modern protections also fingerprint the request's
`User-Agent` and TLS shape. Use `Antibot::build_http_client(&solution.user_agent)?` to copy the
solver's user-agent, and consider `rquest` for TLS fingerprint impersonation.

**Repeated `ChallengeFailed` for one site**
The provider's headless browser was detected. Try:
1. `RetryPolicy::default()` — sometimes the second attempt clears
2. A residential proxy via `default_proxy(...)` — cloud IPs are blocked aggressively
3. Switch providers: `Provider::FlareSolverr` ↔ `Provider::Byparr`

**Build is slow / cargo pulls a lot of crates**
This crate brings in `reqwest` (with rustls), `dashmap`, `futures`, and `wiremock` (dev-only).
The runtime dep tree is comparable to any other reqwest-based HTTP client.

## License

MIT
