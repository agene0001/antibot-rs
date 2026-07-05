# Changelog

All notable changes to this project are documented here. This project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.4]

### Fixed

- **Windows: launching Docker Desktop hung the caller forever and tethered
  Docker Desktop to the caller's lifetime.** The daemon-start command ran via
  `.output()`, which pipes stdout/stderr; `cmd /C start` hands those pipe
  write-handles down to Docker Desktop, so reading to EOF blocked until Docker
  Desktop *exited* — `build()` never returned (0.3.2's probe-timeout fix sits
  after this point and never got the chance to matter), and killing the caller
  took Docker Desktop down with it. The launcher now runs with stdio nulled
  (`.status()`), and on Windows is spawned `CREATE_NO_WINDOW |
  CREATE_BREAKAWAY_FROM_JOB` (falling back without breakaway where the job
  forbids it) so Docker Desktop detaches from the caller's process tree and
  survives it.

### Changed

- **`docker pull` now streams progress instead of running silently.** The
  first-run pull of a large solver image can take minutes; the output was
  captured, so the only visible sign was a single "pulling" log followed by a
  long silence indistinguishable from a hang. Stdout/stderr are now inherited
  so Docker's own progress renders live, and the completion log reports elapsed
  seconds.
- **The daemon-readiness wait loop logs a heartbeat every ~10s** (`waiting for
  Docker daemon to become ready… Ns/240s`) and the ready/pull logs now include
  elapsed time. Combined with the 5s-bounded probe from 0.3.2, a genuinely
  still-booting daemon is now visibly distinct from a wedge — the previous
  loop was silent per-iteration, so a working-but-slow cold boot looked dead.

## [0.3.2]

### Fixed

- **`start_docker_daemon` could hang forever while Docker Desktop was booting.**
  The readiness poll ran `docker info` with no timeout, but while Docker Desktop
  (Windows/macOS) is still bringing up its VM, `docker info` connects to the
  daemon pipe/socket and blocks rather than failing fast. A single hung probe
  wedged the poll loop indefinitely — the loop's deadline check sits after the
  probe `.await`, so `daemon_start_timeout` could never fire. Each probe is now
  bounded by a 5s timeout (with `kill_on_drop`), so the loop keeps polling and
  honors its deadline.

### Changed

- Default `daemon_start_timeout` is now OS-aware: **240s on Windows/macOS**
  (Docker Desktop cold-boots a VM) and **60s on Linux** (native dockerd starts
  in seconds, keeping failure detection fast). Still overridable via
  `AntibotBuilder::daemon_start_timeout`.
- The daemon-not-ready error now points at `daemon_start_timeout` / starting
  Docker beforehand.

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
