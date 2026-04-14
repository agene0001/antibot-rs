# antibot-rs

Auto-managed [Byparr](https://github.com/sergerdn/byparr) / [FlareSolverr](https://github.com/FlareSolverr/FlareSolverr) client for bypassing bot detection in Rust web scrapers.

## Features

- **Provider-agnostic** — works with Byparr, FlareSolverr, or any compatible Docker image
- **Docker lifecycle management** — auto-pulls images, starts containers, and waits for health checks
- **Simple API** — `solve(url)` returns rendered HTML + cookies after challenges are cleared
- **Builder pattern** — configure port, timeout, container name, and health check behavior

## Usage

```rust
use antibot::{Antibot, Provider};

let client = Antibot::builder()
    .provider(Provider::Byparr)
    .auto_start(true)
    .build()
    .await?;

let solution = client.solve("https://example.com").await?;
println!("HTML length: {}", solution.response.len());
println!("Cookies: {:?}", solution.cookies);
```

### Connect to an existing instance

```rust
let client = Antibot::connect("http://localhost:8191");
let solution = client.solve("https://example.com").await?;
```

### Builder options

```rust
Antibot::builder()
    .provider(Provider::FlareSolverr)  // or Provider::Custom("my-image:latest".into())
    .port(9000)                        // host port (default: 8191)
    .auto_start(true)                  // pull & start container if needed
    .container_name("my-solver")       // Docker container name
    .max_timeout_ms(90000)             // per-request timeout (default: 60s)
    .health_check_attempts(20)         // retries after start (default: 15)
    .build()
    .await?;
```

## Requirements

- Docker must be installed and accessible (for `auto_start`)
- One of: Byparr, FlareSolverr, or a compatible image with a `/v1` endpoint

## License

MIT
