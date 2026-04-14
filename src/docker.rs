use crate::{AntibotError, Provider};
use tokio::process::Command;
use tracing::{debug, info, warn};

const CONTAINER_NAME: &str = "antibot-solver";

/// Manages the Docker container lifecycle for the solver service.
pub(crate) struct DockerManager {
    provider: Provider,
    port: u16,
    container_name: String,
}

impl DockerManager {
    pub fn new(provider: Provider, port: u16) -> Self {
        Self {
            provider,
            port,
            container_name: CONTAINER_NAME.to_string(),
        }
    }

    pub fn with_container_name(mut self, name: String) -> Self {
        self.container_name = name;
        self
    }

    /// Check if Docker is available on this system.
    pub async fn is_docker_available(&self) -> bool {
        Command::new("docker")
            .arg("info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success())
    }

    /// Check if the container exists (running or stopped).
    pub async fn container_exists(&self) -> Result<bool, AntibotError> {
        let output = Command::new("docker")
            .args(["ps", "-a", "--filter", &format!("name=^{}$", self.container_name), "--format", "{{.Names}}"])
            .output()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.trim() == self.container_name)
    }

    /// Check if the container is currently running.
    pub async fn container_running(&self) -> Result<bool, AntibotError> {
        let output = Command::new("docker")
            .args(["ps", "--filter", &format!("name=^{}$", self.container_name), "--filter", "status=running", "--format", "{{.Names}}"])
            .output()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.trim() == self.container_name)
    }

    /// Pull the provider's Docker image.
    pub async fn pull_image(&self) -> Result<(), AntibotError> {
        let image = self.provider.image();
        info!("pulling Docker image: {}", image);

        let output = Command::new("docker")
            .args(["pull", image])
            .output()
            .await
            .map_err(|_| AntibotError::DockerNotAvailable)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AntibotError::PullFailed {
                image: image.to_string(),
                reason: stderr.trim().to_string(),
            });
        }

        info!("pulled image: {}", image);
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

    /// Start the container. Creates it if it doesn't exist, starts it if stopped.
    pub async fn start(&self) -> Result<(), AntibotError> {
        if self.container_running().await? {
            debug!("container '{}' is already running", self.container_name);
            return Ok(());
        }

        if self.container_exists().await? {
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

        // Container doesn't exist — pull image if needed and create it
        if !self.image_exists().await? {
            self.pull_image().await?;
        }

        info!(
            "creating container '{}' from {} on port {}",
            self.container_name,
            self.provider.label(),
            self.port
        );

        let port_mapping = format!("{}:8191", self.port);
        let output = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &self.container_name,
                "-p",
                &port_mapping,
                self.provider.image(),
            ])
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

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        Err(AntibotError::HealthCheckFailed {
            url,
            attempts: max_attempts,
        })
    }
}
