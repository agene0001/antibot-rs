# Changelog

All notable changes to this project are documented here. This project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.1] - Unreleased

### Added

- Optional Docker **daemon** auto-start. With `auto_start` on,
  `AntibotBuilder::start_docker_daemon(true)` starts the Docker daemon if it
  isn't running and waits for it to become ready before creating the container.
  Per-OS default: Docker Desktop on macOS/Windows, `systemctl start docker` on
  Linux. `docker_daemon_start_command(program, args)` overrides the command (for
  rootless Docker, Colima, OrbStack, etc.), and `daemon_start_timeout(..)` tunes
  the readiness wait (default 60s). New `AntibotError::DaemonStartFailed`. If the
  `docker` CLI isn't installed, `DockerNotAvailable` is still returned.

## [0.3.0] - Unreleased

### Breaking

- `Antibot::connect_many` now returns `Result<Self, AntibotError>` instead of
  panicking on an empty instance list.
- `PostBody::Raw.body` is now `String` instead of `Vec<u8>`, and
  `SolveRequest::raw_body` takes `impl Into<String>`. The FlareSolverr wire
  protocol is JSON and cannot carry binary bodies.
- `AntibotError`, `Provider`, and `ChallengeKind` are now `#[non_exhaustive]`.
  Downstream `match`es on them must add a wildcard arm. (Done now so future
  variant additions are minor, not major, bumps.)
- New `AntibotError` variants: `ProviderHttp { status, body }`,
  `UnsupportedFeature { provider, feature }`, `InvalidConfig`. Provider HTTP
  failures now surface as `ProviderHttp` rather than `UnexpectedResponse`.
- `SolveRequest` gained a public `return_only_cookies` field; `CachedSession`
  gained a public `status` field. Code constructing these literally must set
  them.

### Fixed

- **Coalescer lost-wakeup hang**: waiters now register for notification before
  checking the result, and a cancelled leader's inflight entry is cleaned up by
  a drop guard, so a timed-out/aborted leader can no longer deadlock a key.
- **POSTs and session/cookie/proxy requests are no longer coalesced**, so a
  waiter never receives a result produced under different request conditions
  (previously a concurrent POST could be silently dropped).
- **Sessions work across a multi-instance pool**: a `SessionHandle` pins every
  request — including its `Drop` cleanup — to the instance that created it.
- **Health watchdog actually restarts a hung container** via a new
  `docker restart`, instead of a no-op `start()` on an already-running
  container, and only probes the locally managed instance.
- **`solve_fresh` writes through to the cache**, replacing the stale entry
  instead of leaving it for the next plain `solve()`.
- Error-body truncation is UTF-8-boundary-safe (no panic on multi-byte chars).
- Per-request HTTP timeout derives from `maxTimeout + 30s`, so solve timeouts
  above the old hardcoded 120s work.
- Retries are limited to transient failures (429/5xx, transport); deterministic
  4xx is no longer retried.
- Session-cache expiry removal uses `remove_if` so a racing fresh insert isn't
  deleted; cache hits report the original solve status instead of a fabricated
  200.
- Stale Docker containers with a mismatched image or port mapping are recreated
  instead of silently reused.

### Added

- `Antibot::shutdown()` for deterministic teardown (stops the watchdog and, when
  managed, the container) — `Drop` remains best-effort and is skipped after an
  explicit shutdown.
- `SolveRequest::return_only_cookies()` (sends `returnOnlyCookies`) to skip the
  rendered HTML when only warming the session cache.
- Per-provider compatibility checks: a hard `UnsupportedFeature` error for
  `create_session()` **and POST requests** on Byparr (whose API is GET-only, so
  a "submitted" POST would silently run as a GET), plus once-logged warnings for
  fields harmlessly ignored server-side. See the README compatibility table.
- `Antibot::connect_with` / `connect_many_with` to declare the provider behind a
  pre-running instance, so the compatibility checks apply without `auto_start`.
- `extract_domain` now keys the cache and coalescer on the registrable domain
  (eTLD+1 via the `psl` crate), so subdomains of one site share a session.
- Least-loaded instance routing (replacing blind round-robin) and an optional
  `AntibotBuilder::max_inflight_per_instance` cap that applies backpressure per
  solver instead of overrunning a single browser's request queue.

### Changed

- The wire request sends `max_timeout` (seconds) alongside `maxTimeout` (ms) so
  Byparr, which has no camelCase alias and reads seconds, honors the configured
  timeout instead of its 60s default.
- Jitter uses a per-call splitmix64 mix so concurrent retries spread out instead
  of computing near-identical delays.
- Debug-sink artifacts are prefixed with a per-run timestamp so a new run can't
  overwrite a previous run's dumps.
- `health_watch` and `health_check_attempts` clamp nonsensical values
  (interval ≥ 1s, attempts ≥ 1).
- `merge_cookies` matches on the full (name, domain, path) identity tuple;
  `Solution::cookie_header()` skips genuinely-expired cookies.
