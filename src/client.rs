use crate::docker::DockerManager;
use crate::types::{ApiRequest, ApiResponse};
use crate::{AntibotError, Provider, Solution};
use tracing::{debug, error, info};

/// Client for solving bot-detection challenges via Byparr/FlareSolverr.
pub struct Antibot {
    http: reqwest::Client,
    base_url: String,
    max_timeout_ms: u64,
}

impl Antibot {
    /// Create a builder for configuring the client.
    pub fn builder() -> AntibotBuilder {
        AntibotBuilder::default()
    }

    /// Quick constructor that connects to an already-running instance.
    /// Does NOT auto-start Docker.
    pub fn connect(base_url: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("failed to build HTTP client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            max_timeout_ms: 60000,
        }
    }

    /// Check if the service is reachable.
    pub async fn is_available(&self) -> bool {
        match self.http.get(&self.base_url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Solve a challenge for the given URL and return the rendered page.
    pub async fn solve(&self, url: &str) -> Result<Solution, AntibotError> {
        self.solve_with_timeout(url, self.max_timeout_ms).await
    }

    /// Solve with a custom timeout (milliseconds).
    pub async fn solve_with_timeout(
        &self,
        url: &str,
        max_timeout_ms: u64,
    ) -> Result<Solution, AntibotError> {
        let request = ApiRequest {
            cmd: "request.get".to_string(),
            url: url.to_string(),
            max_timeout: max_timeout_ms,
        };

        info!("solving challenge for {}", url);

        let resp = self
            .http
            .post(format!("{}/v1", self.base_url))
            .json(&request)
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
                url: url.to_string(),
                reason: api_resp.message,
            });
        }

        let solution = api_resp.solution.ok_or_else(|| {
            AntibotError::UnexpectedResponse("status ok but no solution returned".into())
        })?;

        debug!(
            "solved with {} cookies, status={}",
            solution.cookies.len(),
            solution.status
        );
        info!("solved {} — status {}", url, solution.status);

        Ok(solution)
    }

    /// Build a reqwest Client pre-configured with a solved user-agent.
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
}

/// Builder for configuring and initializing an [`Antibot`] client.
pub struct AntibotBuilder {
    provider: Provider,
    port: u16,
    auto_start: bool,
    container_name: Option<String>,
    max_timeout_ms: u64,
    health_check_attempts: u32,
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
        }
    }
}

impl AntibotBuilder {
    /// Set the provider (Byparr, FlareSolverr, or Custom).
    pub fn provider(mut self, provider: Provider) -> Self {
        self.provider = provider;
        self
    }

    /// Set the host port to map to the container's 8191. Default: 8191.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Enable auto-start: pull image and start container if not running.
    pub fn auto_start(mut self, enabled: bool) -> Self {
        self.auto_start = enabled;
        self
    }

    /// Custom Docker container name. Default: "antibot-solver".
    pub fn container_name(mut self, name: impl Into<String>) -> Self {
        self.container_name = Some(name.into());
        self
    }

    /// Default timeout per solve request in milliseconds. Default: 60000.
    pub fn max_timeout_ms(mut self, ms: u64) -> Self {
        self.max_timeout_ms = ms;
        self
    }

    /// Number of health check attempts after starting (2s apart). Default: 15.
    pub fn health_check_attempts(mut self, attempts: u32) -> Self {
        self.health_check_attempts = attempts;
        self
    }

    /// Build the client. If `auto_start` is enabled, ensures the container is
    /// running and healthy before returning.
    pub async fn build(self) -> Result<Antibot, AntibotError> {
        let base_url = format!("http://localhost:{}", self.port);

        if self.auto_start {
            let mut manager = DockerManager::new(self.provider, self.port);
            if let Some(name) = self.container_name {
                manager = manager.with_container_name(name);
            }

            if !manager.is_docker_available().await {
                return Err(AntibotError::DockerNotAvailable);
            }

            manager.start().await?;
            manager.wait_healthy(self.health_check_attempts).await?;
        }

        let client = Antibot {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("failed to build HTTP client"),
            base_url,
            max_timeout_ms: self.max_timeout_ms,
        };

        Ok(client)
    }
}
