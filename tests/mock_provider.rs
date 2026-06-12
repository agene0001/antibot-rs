//! Integration tests against a `wiremock` server impersonating Byparr/FlareSolverr.
//! No Docker required.

use antibot_rs::{
    Antibot, CoalesceKey, Cookie, DebugConfig, RetryPolicy, SolutionSource, SolveRequest, StreamExt,
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
        entries
            .iter()
            .any(|e| e.path().extension().is_some_and(|x| x == "html")),
        "expected an .html dump"
    );
    assert!(
        entries
            .iter()
            .any(|e| e.path().extension().is_some_and(|x| x == "json")),
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
async fn coalescer_recovers_when_leader_is_cancelled() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_solution("https://cancel.com"))
                .set_delay(Duration::from_millis(200)),
        )
        .mount(&server)
        .await;

    let port = port_from_uri(&server.uri());
    let client = Antibot::builder()
        .port(port)
        .coalesce_solves(CoalesceKey::Domain)
        .build()
        .await
        .unwrap();

    // Cancel the leader mid-solve; its inflight entry must be cleaned up.
    let leader = tokio::time::timeout(
        Duration::from_millis(50),
        client.solve("https://cancel.com/a"),
    )
    .await;
    assert!(leader.is_err(), "leader should have been cancelled");

    // A follow-up solve must not hang on the dead leader's entry.
    let second = tokio::time::timeout(Duration::from_secs(5), client.solve("https://cancel.com/b"))
        .await
        .expect("solve hung after leader cancellation")
        .unwrap();
    assert_eq!(second.status, 200);
}

#[tokio::test]
async fn posts_are_never_coalesced() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .and(body_partial_json(json!({"cmd": "request.post"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_solution("https://p.com/submit"))
                .set_delay(Duration::from_millis(100)),
        )
        .expect(2)
        .mount(&server)
        .await;

    let port = port_from_uri(&server.uri());
    let client = Antibot::builder()
        .port(port)
        .coalesce_solves(CoalesceKey::Url)
        .build()
        .await
        .unwrap();

    // Two concurrent POSTs to the same URL must both reach the provider.
    let a = client.execute(SolveRequest::post("https://p.com/submit").form([("k", "1")]));
    let b = client.execute(SolveRequest::post("https://p.com/submit").form([("k", "2")]));
    let (ra, rb) = tokio::join!(a, b);
    assert!(ra.is_ok(), "{:?}", ra);
    assert!(rb.is_ok(), "{:?}", rb);
}

#[test]
fn connect_many_rejects_empty_list() {
    assert!(Antibot::connect_many(Vec::new()).is_err());
}

#[tokio::test]
async fn byparr_post_is_rejected_without_hitting_provider() {
    use antibot_rs::Provider;

    let server = MockServer::start().await;
    // A POST reaching the provider would fail this test.
    Mock::given(method("POST"))
        .and(path("/v1"))
        .and(body_partial_json(json!({"cmd": "request.post"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_solution("https://b.com")))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .and(body_partial_json(json!({"cmd": "request.get"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_solution("https://b.com")))
        .mount(&server)
        .await;

    let client = Antibot::connect_with(&server.uri(), Provider::Byparr);
    let err = client
        .execute(SolveRequest::post("https://b.com/login").form([("u", "x")]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, antibot_rs::AntibotError::UnsupportedFeature { .. }),
        "expected UnsupportedFeature, got {err:?}"
    );

    // A GET against the same Byparr-hinted client still works.
    assert!(client.solve("https://b.com").await.is_ok());
}

#[tokio::test]
async fn solve_fresh_writes_through_to_cache() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_solution("https://wt.com")))
        .expect(2)
        .mount(&server)
        .await;

    let port = port_from_uri(&server.uri());
    let client = Antibot::builder()
        .port(port)
        .enable_session_cache()
        .build()
        .await
        .unwrap();

    // Seed the cache, force a fresh solve, then confirm the *fresh* result
    // was cached (third call must not reach the provider: expect(2) above).
    let _ = client.solve("https://wt.com").await.unwrap();
    let fresh = client.solve_fresh("https://wt.com").await.unwrap();
    assert_eq!(fresh.source, SolutionSource::Fresh);

    let third = client.solve("https://wt.com").await.unwrap();
    assert!(matches!(third.source, SolutionSource::Cached { .. }));
    assert_eq!(client.metrics().solves_attempted, 2);
}

#[tokio::test]
async fn provider_4xx_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
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

    let err = client.solve("https://bad.com").await.unwrap_err();
    assert!(
        matches!(
            err,
            antibot_rs::AntibotError::ProviderHttp { status: 400, .. }
        ),
        "expected ProviderHttp 400, got {err:?}"
    );
    assert_eq!(
        client.metrics().solves_attempted,
        1,
        "deterministic 4xx must not be retried"
    );
}

#[tokio::test]
async fn cache_is_shared_across_subdomains() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_solution("https://www.sd.com")))
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

    let first = client.solve("https://www.sd.com/page").await.unwrap();
    assert_eq!(first.source, SolutionSource::Fresh);

    // Same registrable domain, different subdomain → cache hit.
    let second = client.solve("https://sd.com/other").await.unwrap();
    assert!(matches!(second.source, SolutionSource::Cached { .. }));
}

#[tokio::test]
async fn return_only_cookies_sends_flag_and_yields_no_html() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .and(body_partial_json(json!({"returnOnlyCookies": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ok",
            "message": "",
            "solution": {
                "url": "https://roc.com",
                "status": 200,
                "cookies": [{"name": "cf_clearance", "value": "xyz"}],
                "userAgent": "Mozilla/5.0",
                // no "response" field, as providers omit it for returnOnlyCookies
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = Antibot::connect(&server.uri());
    let solution = client
        .execute(SolveRequest::get("https://roc.com").return_only_cookies())
        .await
        .unwrap();

    assert!(solution.response.is_none());
    assert_eq!(solution.cookies.len(), 1);
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

#[tokio::test]
async fn requests_spread_across_instances() {
    // Two separate instances; concurrent load should reach both, not pile onto
    // one. Each instance counts its own hits via `expect` ranges.
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    for srv in [&a, &b] {
        Mock::given(method("POST"))
            .and(path("/v1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(ok_solution("https://spread.com"))
                    .set_delay(Duration::from_millis(80)),
            )
            // With least-loaded routing and 6 concurrent solves over 2 idle
            // instances, each should take a meaningful share.
            .expect(1..=5)
            .mount(srv)
            .await;
    }

    let client = Antibot::connect_many(vec![a.uri(), b.uri()]).unwrap();

    let mut handles = Vec::new();
    for i in 0..6 {
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            c.solve(&format!("https://spread.com/{i}")).await
        }));
    }
    for h in handles {
        assert!(h.await.unwrap().is_ok());
    }
    // `expect(1..=5)` on each server is verified on drop.
}

#[tokio::test]
async fn per_instance_cap_serializes_concurrent_solves() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_solution("https://cap.com"))
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;

    let port = port_from_uri(&server.uri());
    let client = Antibot::builder()
        .port(port)
        .max_inflight_per_instance(1)
        .build()
        .await
        .unwrap();

    // Three 100ms solves capped at 1-in-flight must run serially (~300ms),
    // not concurrently (~100ms).
    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for i in 0..3 {
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            c.solve(&format!("https://cap.com/{i}")).await
        }));
    }
    for h in handles {
        assert!(h.await.unwrap().is_ok());
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(250),
        "expected serialized ~300ms, got {elapsed:?}"
    );
}
