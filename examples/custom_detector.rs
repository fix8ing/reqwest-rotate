//! A key in the query string and an upstream that signals quota its own way.
//!
//! This upstream answers a spent key with `500` and the text "quota exhausted"
//! instead of a `429`. The default detector would pass that through, so a
//! closure that reads the body replaces it. Run with
//! `cargo run --example custom_detector`.

use reqwest_rotate::header::HeaderMap;
use reqwest_rotate::{Client, Identity, StatusCode, Verdict};
use wiremock::matchers::{method, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let upstream = fake_upstream().await;

    // Reading the body means every response is buffered before it is returned.
    let quota_in_body = |status: StatusCode, _: &HeaderMap, body: &[u8]| {
        if status == StatusCode::INTERNAL_SERVER_ERROR && body.starts_with(b"quota exhausted") {
            Verdict::Exhausted { retry_after: None }
        } else {
            Verdict::Ok
        }
    };

    let client = Client::builder()
        .identity(Identity::builder().query("api_key", "key-a").build()?)
        .identity(Identity::builder().query("api_key", "key-b").build()?)
        .detector(quota_in_body)
        .build()?;

    // The caller's own query params survive. The identity appends api_key.
    let body = client
        .get(format!("{}/things", upstream.uri()))
        .query(&[("limit", "10")])
        .send()
        .await?
        .text()
        .await?;
    println!("{body}");

    Ok(())
}

/// An upstream that reports a spent key as a 500 with a text body.
async fn fake_upstream() -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(query_param("api_key", "key-a"))
        .respond_with(ResponseTemplate::new(500).set_body_string("quota exhausted for key-a"))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(query_param("api_key", "key-b"))
        .and(query_param("limit", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_string("served by key-b, limit=10"))
        .mount(&server)
        .await;

    server
}
