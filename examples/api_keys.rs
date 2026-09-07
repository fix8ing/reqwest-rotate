//! Rotate through API keys sent as a header.
//!
//! The upstream rejects `key-a` with a `429` and serves `key-b`. The client
//! notices, cools `key-a`, and re-sends under `key-b` without the caller doing
//! anything. Run with `cargo run --example api_keys`.

use reqwest_rotate::{Client, Identity};
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let upstream = fake_upstream().await;

    let client = Client::builder()
        .identity(Identity::builder().header("x-api-key", "key-a").build()?)
        .identity(Identity::builder().header("x-api-key", "key-b").build()?)
        .build()?;

    // Plain reqwest usage from here on. Both requests come back "served by key-b":
    // the first one rotated after key-a's 429, the second started on key-b.
    for n in 1..=2 {
        let body = client
            .get(format!("{}/things", upstream.uri()))
            .send()
            .await?
            .text()
            .await?;
        println!("request {n}: {body}");
    }

    Ok(())
}

/// An upstream that has run out of quota for key-a but not key-b.
async fn fake_upstream() -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(header("x-api-key", "key-a"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "60"))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(header("x-api-key", "key-b"))
        .respond_with(ResponseTemplate::new(200).set_body_string("served by key-b"))
        .mount(&server)
        .await;

    server
}
