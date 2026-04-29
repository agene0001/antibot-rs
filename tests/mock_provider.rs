//! Integration tests against a `wiremock` server impersonating Byparr/FlareSolverr.
//! No Docker required.

use antibot_rs::{
    Antibot, CoalesceKey, Cookie, DebugConfig, RetryPolicy, SolveRequest, SolutionSource,
    StreamExt,
};
use serde_json::json;
use std::time::Duration;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ok_solution(url: &str) -> serde_json::Value {
    json!({
        "status": "ok",
        "message": "",
        "solution": {
            "url": url,
            "status": 200,
            "cookies": [
                {
                    "name": "cf_clearance",
                    "value": "abc123",
                    "domain": "example.com",
                    "path": "/",
                    "expires": -1,
                    "httpOnly": false,
                    "secure": true,
                }
            ],
            "userAgent": "Mozilla/5.0 (X11; Linux x86_64) Chrome/120",
            "response": "<html>solved</html>"
        }
    })
}

#[tokio::test]
async fn execute_get_round_trips() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .and(body_partial_json(json!({"cmd": "request.get"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_solution("https://example.com")))
        .mount(&server)
        .await;

    let client = Antibot::connect(&server.uri());
    let solution = client.solve("https://example.com").await.unwrap();

    assert_eq!(solution.status, 200);
    assert_eq!(solution.cookies.len(), 1);
    assert_eq!(solution.cookies[0].name, "cf_clearance");
    assert_eq!(solution.html(), "<html>solved</html>");
    assert_eq!(solution.source, SolutionSource::Fresh);
}

#[tokio::test]
async fn post_with_json_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .and(body_partial_json(json!({
            "cmd": "request.post",
            "url": "https://site.com/api/login",
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ok_solution("https://site.com/api/login")),
        )
        .mount(&server)
        .await;

    let client = Antibot::connect(&server.uri());
    let req = SolveRequest::post("https://site.com/api/login")
        .json(json!({"user": "alice"}))
        .with_header("X-Test", "value")
        .with_cookie(Cookie::new("session", "abc"));

    let result = client.execute(req).await;
    assert!(result.is_ok(), "post failed: {:?}", result);
}

#[tokio::test]
async fn session_cache_hits_skip_provider_after_first_solve() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_solution("https://walmart.com")))
        .expect(1)
        .mount(&server)
        .await;

    let port = port_from_uri(&server.uri());
    let client = Antibot::builder()
        .port(port)
        .enable_session_cache()
        .build()
        .await
        .unwrap();

    let first = client.solve("https://walmart.com").await.unwrap();
    assert_eq!(first.source, SolutionSource::Fresh);

    let second = client.solve("https://walmart.com").await.unwrap();
    assert!(matches!(second.source, SolutionSource::Cached { .. }));
}

fn port_from_uri(uri: &str) -> u16 {
    uri.rsplit(':')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .expect("uri must end with :PORT")
}

#[tokio::test]
async fn retry_recovers_after_transient_failure() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .up_to_n_times(2)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_solution("https://x.com")))
        .mount(&server)
        .await;

    let port = port_from_uri(&server.uri());

    let client = Antibot::builder()
        .port(port)
        .retry(RetryPolicy {
            max_attempts: 4,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(50),
            multiplier: 2.0,
            jitter: false,
        })
        .build()
        .await
        .unwrap();

    let solution = client.solve("https://x.com").await.unwrap();
    assert_eq!(solution.status, 200);

    let m = client.metrics();
    assert!(m.retries >= 2, "expected retries >= 2, got {}", m.retries);
    assert_eq!(m.solves_succeeded, 1);
}

#[tokio::test]
async fn metrics_tracks_attempts_and_cache_hits() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_solution("https://m.com")))
        .mount(&server)
        .await;

    let port = port_from_uri(&server.uri());

    let client = Antibot::builder()
        .port(port)
        .enable_session_cache()
        .build()
        .await
        .unwrap();

    let _ = client.solve("https://m.com").await.unwrap();
    let _ = client.solve("https://m.com").await.unwrap();

    let m = client.metrics();
    assert_eq!(m.solves_attempted, 1, "second call should hit cache");
    assert_eq!(m.cache_hits, 1);
}

#[tokio::test]
async fn debug_sink_writes_html_and_metadata() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_solution("https://d.com/page")))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();

    let port = port_from_uri(&server.uri());

    let client = Antibot::builder()
        .port(port)
        .debug(DebugConfig::new(dir.path()))
        .build()
        .await
        .unwrap();

    let _ = client.solve("https://d.com/page").await.unwrap();

    // Allow async write to flush.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.iter().any(|e| e.path().extension().is_some_and(|x| x == "html")),
        "expected an .html dump"
    );
    assert!(
        entries.iter().any(|e| e.path().extension().is_some_and(|x| x == "json")),
        "expected a .json metadata file"
    );
}

#[tokio::test]
async fn coalescer_dedupes_concurrent_solves() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_solution("https://c.com"))
                .set_delay(Duration::from_millis(120)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let port = port_from_uri(&server.uri());

    let client = Antibot::builder()
        .port(port)
        .coalesce_solves(CoalesceKey::Domain)
        .build()
        .await
        .unwrap();

    let mut handles = Vec::new();
    for _ in 0..8 {
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            c.solve_fresh("https://c.com/page").await
        }));
    }

    for h in handles {
        let r = h.await.unwrap();
        assert!(r.is_ok(), "{:?}", r);
    }
}

#[tokio::test]
async fn solve_stream_runs_with_bounded_concurrency() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_solution("https://s.com")))
        .mount(&server)
        .await;

    let port = port_from_uri(&server.uri());

    let client = Antibot::builder().port(port).build().await.unwrap();

    let urls: Vec<String> = (0..5).map(|i| format!("https://s.com/{}", i)).collect();
    let mut stream = client.solve_stream(urls, 3);
    let mut count = 0;
    while let Some((_url, res)) = stream.next().await {
        assert!(res.is_ok(), "{:?}", res);
        count += 1;
    }
    assert_eq!(count, 5);
}
