use crate::{AntibotError, Provider};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tracing::{debug, info, warn};

const CONTAINER_NAME: &str = "antibot-solver";

/// Run the daemon-start command with stdio detached.
///
/// The launcher's stdio must NOT be piped (`.output()`): on Windows,
/// `cmd /C start "" "Docker Desktop.exe"` hands the pipe write-handles down to
/// Docker Desktop itself, so reading the pipes to EOF blocks until Docker
/// Desktop *exits* — the caller hangs forever right after launching it, and
/// Docker Desktop is tethered to the caller's process tree (it dies when the
/// caller is killed). Nulling stdio avoids both.
///
/// On Windows we additionally try `CREATE_BREAKAWAY_FROM_JOB` so Docker
/// Desktop escapes the terminal/scheduler job object and survives the caller
/// exiting; if the job forbids breakaway the spawn fails with access-denied,
/// so we retry without the flag.
async fn run_daemon_start(
    program: &str,
    args: &[String],
) -> std::io::Result<std::process::ExitStatus> {
    fn base(program: &str, args: &[String]) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd
    }

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

        let mut detached = base(program, args);
        detached.creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
        match detached.status().await {
            // Job object without JOB_OBJECT_LIMIT_BREAKAWAY_OK → retry attached.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                debug!("job breakaway denied; launching daemon without it");
                let mut attached = base(program, args);
                attached.creation_flags(CREATE_NO_WINDOW);
                attached.status().await
            }
            other => other,
        }
    }
    #[cfg(not(windows))]
    {
        base(program, args).status().await
    }
}

/// Best-effort command to start the Docker daemon on the current OS.
/// `None` if we don't have a sensible default for this platform.
fn default_daemon_start() -> Option<(String, Vec<String>)> {
    if cfg!(target_os = "macos") {
        // Launches Docker Desktop; returns immediately while the VM boots.
        Some(("open".into(), vec!["-a".into(), "Docker".into()]))
    } else if cfg!(target_os = "windows") {
        Some((
            "cmd".into(),
            vec![
                "/C".into(),
                "start".into(),
                "".into(),
                r"C:\Program Files\Docker\Docker\Docker Desktop.exe".into(),
            ],
        ))
    } else if cfg!(target_os = "linux") {
        // Assumes systemd-managed system Docker; needs privileges. Rootless,
        // Colima, OrbStack, etc. should pass a custom start command.
        Some(("systemctl".into(), vec!["start".into(), "docker".into()]))
    } else {
        None
    }
}

/// Resource caps applied to the spawned container.
///
/// Values are passed verbatim to `docker run --memory=<...> --cpus=<...>`.
#[derive(Debug, Clone, Default)]
pub struct DockerLimits {
    /// e.g. `"2g"`, `"512m"`. `None` for unlimited (Docker default).
    pub memory: Option<String>,
    /// e.g. `"1.5"`. `None` for unlimited.
    pub cpus: Option<String>,
    /// Optional `--shm-size` (Chrome benefits from `1g`). `None` skips it.
    pub shm_size: Option<String>,
}

impl DockerLimits {
    pub fn memory(mut self, memory: impl Into<String>) -> Self {
        self.memory = Some(memory.into());
        self
    }
    pub fn cpus(mut self, cpus: impl Into<String>) -> Self {
        self.cpus = Some(cpus.into());
        self
    }
    pub fn shm_size(mut self, shm_size: impl Into<String>) -> Self {
        self.shm_size = Some(shm_size.into());
        self
    }
}

/// Manages the Docker container lifecycle for the solver service.
#[derive(Clone)]
pub(crate) struct DockerManager {
    provider: Provider,
    port: u16,
    container_name: String,
    limits: DockerLimits,
}

impl DockerManager {
    pub fn new(provider: Provider, port: u16) -> Self {
        Self {
            provider,
            port,
            container_name: CONTAINER_NAME.to_string(),
            limits: DockerLimits::default(),
        }
    }

    pub fn with_container_name(mut self, name: String) -> Self {
        self.container_name = name;
        self
    }

    pub fn with_limits(mut self, limits: DockerLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn container_name(&self) -> &str {
        &self.container_name
    }

    /// Base URL of the service this manager's container exposes.
    pub fn base_url(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Check if Docker is available on this system (CLI present *and* daemon
    /// reachable — `docker info` fails if the daemon isn't running).
    ///
    /// The `docker info` call is bounded by a 5s timeout: while Docker Desktop
    /// is mid-boot (Windows/macOS VM), `docker info` connects to the daemon
    /// pipe/socket and BLOCKS waiting for a response instead of failing fast.
    /// Without the timeout, a single hung probe would wedge the
    /// `ensure_daemon_running` poll loop forever — the loop's deadline check
    /// sits after this `.await`, so it could never fire. `kill_on_drop` reaps
    /// the probe process when the timeout elapses.
    pub async fn is_docker_available(&self) -> bool {
        let probe = Command::new("docker")
            .arg("info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status();
        matches!(
            tokio::time::timeout(Duration::from_secs(5), probe).await,
            Ok(Ok(status)) if status.success()
        )
    }

    /// Whether the `docker` CLI is installed (regardless of daemon state).
    async fn docker_cli_present(&self) -> bool {
        Command::new("docker")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success())
    }

    /// Ensure the Docker daemon is running, starting it if necessary and
    /// waiting up to `max_wait` for it to become ready.
    ///
    /// If `custom_start` is `Some`, that command is used to start the daemon;
    /// otherwise a per-OS default is used (Docker Desktop on macOS/Windows,
    /// `systemctl start docker` on Linux). Returns [`AntibotError::DockerNotAvailable`]
    /// when the `docker` CLI isn't installed (nothing we can start), or
    /// [`AntibotError::DaemonStartFailed`] when the start command fails or the
    /// daemon doesn't come up in time.
    pub async fn ensure_running(
        &self,
        custom_start: Option<&(String, Vec<String>)>,
        max_wait: Duration,
    ) -> Result<(), AntibotError> {
        if self.is_docker_available().await {
            return Ok(());
        }

        if !self.docker_cli_present().await {
            // No docker binary — starting a daemon won't help.
            return Err(AntibotError::DockerNotAvailable);
        }

        let (program, args) = custom_start
            .cloned()
            .or_else(default_daemon_start)
            .ok_or_else(|| {
                AntibotError::DaemonStartFailed(
                    "no default daemon-start command for this OS; pass a custom one".into(),
                )
            })?;

        info!(
            "Docker daemon not running; starting it via `{} {}`",
            program,
            args.join(" ")
        );

        let status = run_daemon_start(&program, &args).await.map_err(|e| {
            AntibotError::DaemonStartFailed(format!("could not run `{program}`: {e}"))
        })?;

        if !status.success() {
            return Err(AntibotError::DaemonStartFailed(format!(
                "`{program}` failed with {status}"
            )));
        }

        // The launcher returns before the daemon is usable (Docker Desktop
        // boots a VM), so poll until `docker info` succeeds. Each probe is
        // 5s-bounded (see `is_docker_available`), so the loop can never wedge;
        // a heartbeat every ~10s makes a genuinely-still-booting daemon
        // visibly distinct from a hang.
        let started = Instant::now();
        let deadline = started + max_wait;
        let mut last_heartbeat = started;
        loop {
            if self.is_docker_available().await {
                info!("Docker daemon is ready ({}s)", started.elapsed().as_secs());
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(AntibotError::DaemonStartFailed(format!(
                    "daemon did not become ready within {}s — Docker Desktop can \
                     take longer to cold-boot; increase the wait via \
                     `AntibotBuilder::daemon_start_timeout`, or start Docker \
                     before running so it is already warm",
                    max_wait.as_secs()
                )));
            }
            if now.duration_since(last_heartbeat) >= Duration::from_secs(10) {
                info!(
                    "waiting for Docker daemon to become ready… {}s/{}s",
                    started.elapsed().as_secs(),
                    max_wait.as_secs()
                );
                last_heartbeat = now;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Check if the container exists (running or stopped).
    pub async fn container_exists(&self) -> Result<bool, AntibotError> {
        let output = Command::new("docker")
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("name=^{}$", self.container_name),
                "--format",
                "{{.Names}}",
            ])
            .output()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.trim() == self.container_name)
    }

    /// Check if the container is currently running.
    pub async fn container_running(&self) -> Result<bool, AntibotError> {
        let output = Command::new("docker")
            .args([
                "ps",
                "--filter",
                &format!("name=^{}$", self.container_name),
                "--filter",
                "status=running",
                "--format",
                "{{.Names}}",
            ])
            .output()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.trim() == self.container_name)
    }

    /// Pull the provider's Docker image.
    ///
    /// Stdout/stderr are **inherited** so Docker's own pull progress streams to
    /// the terminal. A first-run pull of a large solver image can take minutes;
    /// capturing the output (as this used to) left a silent gap between the
    /// "pulling" log and completion that was indistinguishable from a hang.
    pub async fn pull_image(&self) -> Result<(), AntibotError> {
        let image = self.provider.image();
        let started = Instant::now();
        info!("pulling Docker image (progress streams below): {}", image);

        let status = Command::new("docker")
            .args(["pull", image])
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        if !status.success() {
            return Err(AntibotError::PullFailed {
                image: image.to_string(),
                reason: format!(
                    "`docker pull` exited with {status} (see streamed output above)"
                ),
            });
        }

        info!("pulled image {} in {}s", image, started.elapsed().as_secs());
        Ok(())
    }

    /// Check if the image exists locally.
    pub async fn image_exists(&self) -> Result<bool, AntibotError> {
        let output = Command::new("docker")
            .args(["image", "inspect", self.provider.image()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        Ok(output.success())
    }

    /// Check that an existing container was created from this manager's image
    /// and publishes the expected host port. A leftover container from an old
    /// configuration would otherwise be reused silently and fail health checks
    /// with no hint as to why.
    async fn container_config_matches(&self) -> Result<bool, AntibotError> {
        let output = Command::new("docker")
            .args([
                "inspect",
                "--format",
                "{{.Config.Image}}|{{range $p, $b := .HostConfig.PortBindings}}{{$p}}={{(index $b 0).HostPort}};{{end}}",
                &self.container_name,
            ])
            .output()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        if !output.status.success() {
            return Ok(false);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut parts = stdout.trim().splitn(2, '|');
        let image_ok = parts.next() == Some(self.provider.image());
        let port_ok = parts
            .next()
            .is_some_and(|p| p.contains(&format!("8191/tcp={};", self.port)));
        Ok(image_ok && port_ok)
    }

    async fn remove_container(&self) -> Result<(), AntibotError> {
        let output = Command::new("docker")
            .args(["rm", "-f", &self.container_name])
            .output()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AntibotError::StartFailed(stderr.trim().to_string()));
        }
        Ok(())
    }

    /// Start the container. Creates it if it doesn't exist, starts it if
    /// stopped, and recreates it if it exists with a different image or port.
    pub async fn start(&self) -> Result<(), AntibotError> {
        if self.container_exists().await? {
            if self.container_config_matches().await? {
                if self.container_running().await? {
                    debug!("container '{}' is already running", self.container_name);
                    return Ok(());
                }

                info!("starting existing container '{}'", self.container_name);
                let output = Command::new("docker")
                    .args(["start", &self.container_name])
                    .output()
                    .await
                    .map_err(|_| AntibotError::DockerNotAvailable)?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(AntibotError::StartFailed(stderr.trim().to_string()));
                }

                return Ok(());
            }

            warn!(
                "container '{}' exists with a different image or port mapping; recreating",
                self.container_name
            );
            self.remove_container().await?;
        }

        if !self.image_exists().await? {
            self.pull_image().await?;
        }

        info!(
            "creating container '{}' from {} on port {} (mem={:?} cpus={:?})",
            self.container_name,
            self.provider.label(),
            self.port,
            self.limits.memory,
            self.limits.cpus,
        );

        let port_mapping = format!("{}:8191", self.port);
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            self.container_name.clone(),
            "-p".into(),
            port_mapping,
        ];

        if let Some(mem) = &self.limits.memory {
            args.push(format!("--memory={}", mem));
        }
        if let Some(cpus) = &self.limits.cpus {
            args.push(format!("--cpus={}", cpus));
        }
        if let Some(shm) = &self.limits.shm_size {
            args.push(format!("--shm-size={}", shm));
        }

        args.push(self.provider.image().to_string());

        let output = Command::new("docker")
            .args(&args)
            .output()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AntibotError::StartFailed(stderr.trim().to_string()));
        }

        info!("container '{}' started", self.container_name);
        Ok(())
    }

    /// Restart the container. Unlike [`DockerManager::start`], this bounces a
    /// container that is running but unresponsive (hung browser); `start`
    /// would see it running and do nothing. Falls back to `start` when the
    /// container is missing or its config no longer matches.
    pub async fn restart(&self) -> Result<(), AntibotError> {
        if self.container_exists().await? && self.container_config_matches().await? {
            info!("restarting container '{}'", self.container_name);
            let output = Command::new("docker")
                .args(["restart", "-t", "10", &self.container_name])
                .output()
                .await
                .map_err(|_| AntibotError::DockerNotAvailable)?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(AntibotError::StartFailed(stderr.trim().to_string()));
            }
            return Ok(());
        }

        self.start().await
    }

    /// Stop the container (best-effort, with a short timeout).
    pub async fn stop(&self) -> Result<(), AntibotError> {
        let output = Command::new("docker")
            .args(["stop", "-t", "10", &self.container_name])
            .output()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AntibotError::StartFailed(stderr.trim().to_string()));
        }
        info!("container '{}' stopped", self.container_name);
        Ok(())
    }

    /// Wait for the service to become healthy (respond to HTTP).
    pub async fn wait_healthy(&self, max_attempts: u32) -> Result<(), AntibotError> {
        let url = format!("http://localhost:{}", self.port);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| AntibotError::StartFailed(e.to_string()))?;

        for attempt in 1..=max_attempts {
            debug!("health check attempt {}/{}", attempt, max_attempts);

            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!("service healthy on port {}", self.port);
                    return Ok(());
                }
                Ok(resp) => {
                    warn!("health check returned {}, retrying...", resp.status());
                }
                Err(_) => {
                    debug!("service not ready yet, waiting...");
                }
            }

            if attempt < max_attempts {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }

        Err(AntibotError::HealthCheckFailed {
            url,
            attempts: max_attempts,
        })
    }
}
