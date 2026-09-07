//! Rotate through proxies for an upstream that limits by IP.
//!
//! Each identity is a proxy and nothing else. Every proxied identity gets its
//! own inner `reqwest::Client`. The client is built with `Exhausted::Error`, so
//! once both proxies are cooling it returns an error instead of waiting.
//! Run with `cargo run --example proxies`.

use reqwest_rotate::{Client, Error, Exhausted, Identity, Proxy};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Two "proxies". The first is out of quota, the second serves once then is spent.
    let proxy_a = fake_proxy(ResponseTemplate::new(429).insert_header("retry-after", "30")).await;
    let proxy_b = fake_proxy(
        ResponseTemplate::new(200)
            .set_body_string("served via proxy-b")
            .insert_header("x-ratelimit-remaining", "0")
            .insert_header("x-ratelimit-reset", "30"),
    )
    .await;

    let client = Client::builder()
        .identity(
            Identity::builder()
                .proxy(Proxy::all(proxy_a.uri())?)
                .build()?,
        )
        .identity(
            Identity::builder()
                .proxy(Proxy::all(proxy_b.uri())?)
                .build()?,
        )
        .exhausted(Exhausted::Error)
        .build()?;

    // First request: proxy-a says 429, rotate, proxy-b serves. Its response also
    // says the quota is now spent, so proxy-b cools after returning it.
    let body = client
        .get("http://api.example.invalid/things")
        .send()
        .await?
        .text()
        .await?;
    println!("request 1: {body}");

    // Second request: both proxies are cooling, and the policy is Error.
    match client.get("http://api.example.invalid/things").send().await {
        Err(Error::AllExhausted { retry_after }) => {
            println!("request 2: every proxy is exhausted, soonest reset in {retry_after:?}");
        }
        other => println!("request 2: unexpected {other:?}"),
    }

    Ok(())
}

/// A server that answers every proxied request the same way.
async fn fake_proxy(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}
