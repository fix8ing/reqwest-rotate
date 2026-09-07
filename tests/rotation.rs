use std::time::{Duration, Instant};

use reqwest_rotate::header::HeaderMap;
use reqwest_rotate::{Client, Error, Exhausted, Identity, Proxy, StatusCode, Verdict};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockBuilder, MockServer, ResponseTemplate};

fn key(k: &str) -> Identity {
    Identity::builder().header("x-api-key", k).build().unwrap()
}

fn keyed(k: &str) -> MockBuilder {
    Mock::given(method("GET"))
        .and(path("/data"))
        .and(header("x-api-key", k))
}

fn ok(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(body)
}

fn too_many(retry_after_secs: Option<u64>) -> ResponseTemplate {
    let template = ResponseTemplate::new(429);
    match retry_after_secs {
        Some(secs) => template.insert_header("retry-after", secs.to_string().as_str()),
        None => template,
    }
}

async fn fetch(client: &Client, server: &MockServer) -> Result<String, Error> {
    let response = client.get(format!("{}/data", server.uri())).send().await?;
    Ok(response.text().await?)
}

#[tokio::test]
async fn without_identities_behaves_like_reqwest() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/data"))
        .respond_with(ok("hi"))
        .mount(&server)
        .await;

    let client = Client::new();
    let response = client
        .get(format!("{}/data", server.uri()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.url().path(), "/data");
    assert_eq!(response.text().await.unwrap(), "hi");
}

#[tokio::test]
async fn identity_headers_and_configure_are_applied() {
    let server = MockServer::start().await;
    keyed("k1")
        .and(header("user-agent", "rotate-test"))
        .respond_with(ok("k1"))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::builder()
        .identity(key("k1"))
        .configure(|b| b.user_agent("rotate-test"))
        .build()
        .unwrap();
    assert_eq!(fetch(&client, &server).await.unwrap(), "k1");
}

#[tokio::test]
async fn identity_query_appends_and_overrides() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/data"))
        .and(query_param("limit", "5"))
        .and(query_param("api_key", "k1"))
        .and(|req: &wiremock::Request| {
            req.url
                .query_pairs()
                .filter(|(k, _)| k == "api_key")
                .count()
                == 1
        })
        .respond_with(ok("k1"))
        .expect(1)
        .mount(&server)
        .await;

    let identity = Identity::builder().query("api_key", "k1").build().unwrap();
    let client = Client::builder().identity(identity).build().unwrap();
    let response = client
        .get(format!("{}/data", server.uri()))
        .query(&[("limit", "5"), ("api_key", "stale")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "k1");
}

#[tokio::test]
async fn stamp_can_put_the_key_in_the_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/k1/data"))
        .respond_with(ok("k1"))
        .expect(1)
        .mount(&server)
        .await;

    let identity = Identity::builder()
        .stamp(|req| {
            let path = req.url().path().replace("KEY", "k1");
            req.url_mut().set_path(&path);
        })
        .build()
        .unwrap();
    let client = Client::builder().identity(identity).build().unwrap();
    let response = client
        .get(format!("{}/v3/KEY/data", server.uri()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "k1");
}

#[tokio::test]
async fn rotates_on_429_and_stays_rotated() {
    let server = MockServer::start().await;
    keyed("k1")
        .respond_with(too_many(Some(30)))
        .expect(1)
        .mount(&server)
        .await;
    keyed("k2")
        .respond_with(ok("k2"))
        .expect(2)
        .mount(&server)
        .await;

    let client = Client::builder()
        .identity(key("k1"))
        .identity(key("k2"))
        .build()
        .unwrap();
    assert_eq!(fetch(&client, &server).await.unwrap(), "k2");
    assert_eq!(fetch(&client, &server).await.unwrap(), "k2");
}

#[tokio::test]
async fn all_exhausted_error_policy_returns_at_once() {
    let server = MockServer::start().await;
    keyed("k1")
        .respond_with(too_many(Some(30)))
        .expect(1)
        .mount(&server)
        .await;
    keyed("k2")
        .respond_with(too_many(Some(10)))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::builder()
        .identity(key("k1"))
        .identity(key("k2"))
        .exhausted(Exhausted::Error)
        .build()
        .unwrap();
    let err = fetch(&client, &server).await.unwrap_err();
    match err {
        Error::AllExhausted { retry_after } => {
            assert!(retry_after <= Duration::from_secs(10), "{retry_after:?}");
            assert!(retry_after > Duration::from_secs(8), "{retry_after:?}");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn all_exhausted_wait_policy_sleeps_until_reset() {
    let server = MockServer::start().await;
    keyed("k1")
        .respond_with(too_many(Some(1)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    keyed("k2")
        .respond_with(too_many(Some(1)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    keyed("k1").respond_with(ok("k1")).mount(&server).await;
    keyed("k2").respond_with(ok("k2")).mount(&server).await;

    let client = Client::builder()
        .identity(key("k1"))
        .identity(key("k2"))
        .exhausted(Exhausted::Wait)
        .build()
        .unwrap();
    let started = Instant::now();
    let body = fetch(&client, &server).await.unwrap();
    assert!(body == "k1" || body == "k2", "{body}");
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn body_detector_rotates_and_preserves_the_response() {
    let server = MockServer::start().await;
    keyed("k1")
        .respond_with(ResponseTemplate::new(500).set_body_string("quota exhausted"))
        .expect(1)
        .mount(&server)
        .await;
    keyed("k2")
        .respond_with(ok("fine").insert_header("x-custom", "yes"))
        .expect(1)
        .mount(&server)
        .await;

    let detector = |status: StatusCode, _: &HeaderMap, body: &[u8]| {
        if status == StatusCode::INTERNAL_SERVER_ERROR && body.windows(5).any(|w| w == b"quota") {
            Verdict::Exhausted { retry_after: None }
        } else {
            Verdict::Ok
        }
    };
    let client = Client::builder()
        .identity(key("k1"))
        .identity(key("k2"))
        .detector(detector)
        .build()
        .unwrap();
    let response = client
        .get(format!("{}/data", server.uri()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.url().as_str(), format!("{}/data", server.uri()));
    assert_eq!(response.headers()["x-custom"], "yes");
    assert_eq!(response.text().await.unwrap(), "fine");
}

#[tokio::test]
async fn depleted_returns_the_response_then_rotates() {
    let server = MockServer::start().await;
    keyed("k1")
        .respond_with(
            ok("k1")
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "60"),
        )
        .expect(1)
        .mount(&server)
        .await;
    keyed("k2")
        .respond_with(ok("k2"))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::builder()
        .identity(key("k1"))
        .identity(key("k2"))
        .build()
        .unwrap();
    assert_eq!(fetch(&client, &server).await.unwrap(), "k1");
    assert_eq!(fetch(&client, &server).await.unwrap(), "k2");
}

#[tokio::test]
async fn cooldown_readmits_an_identity_without_retry_after() {
    let server = MockServer::start().await;
    keyed("k1")
        .respond_with(too_many(None))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    keyed("k1").respond_with(ok("k1")).mount(&server).await;

    let client = Client::builder()
        .identity(key("k1"))
        .cooldown(Duration::from_secs(1))
        .exhausted(Exhausted::Error)
        .build()
        .unwrap();
    let err = fetch(&client, &server).await.unwrap_err();
    assert!(
        matches!(err, Error::AllExhausted { retry_after } if retry_after <= Duration::from_secs(1))
    );

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(fetch(&client, &server).await.unwrap(), "k1");
}

#[tokio::test]
async fn proxy_identity_routes_through_the_proxy() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/data"))
        .respond_with(ok("via-proxy"))
        .expect(1)
        .mount(&server)
        .await;

    let identity = Identity::builder()
        .proxy(Proxy::all(server.uri()).unwrap())
        .build()
        .unwrap();
    let client = Client::builder().identity(identity).build().unwrap();
    let response = client
        .get("http://example.invalid/data")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "via-proxy");
}

#[tokio::test]
async fn from_reqwest_client_passes_through() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/data"))
        .and(header("user-agent", "wrapped"))
        .respond_with(ok("hi"))
        .expect(1)
        .mount(&server)
        .await;

    let inner = reqwest::Client::builder()
        .user_agent("wrapped")
        .build()
        .unwrap();
    let client = Client::from(inner);
    assert_eq!(fetch(&client, &server).await.unwrap(), "hi");
}
