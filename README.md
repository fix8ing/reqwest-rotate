# reqwest-rotate

A drop-in `reqwest::Client` that rotates identities when the active one is rate-limited.

Give the client a pool of identities. Each request goes out under the active one. When a
detector reports the response as a rate-limit rejection, the client cools that identity,
moves to the next, and re-sends. With no identities, it is a plain `reqwest::Client`.

## Usage

```rust
use reqwest_rotate::{Client, Identity};

let client = Client::builder()
    .identity(Identity::builder().header("x-api-key", "key-a").build()?)
    .identity(Identity::builder().header("x-api-key", "key-b").build()?)
    .build()?;

let things = client
    .get("https://api.example.com/v1/things")
    .send()
    .await?
    .text()
    .await?;
```

## Identities

An identity is whatever the upstream keys usage on. Build one from any combination of:

```rust
Identity::builder()
    .header("x-api-key", key)                       // headers, repeatable
    .query("api_key", key)                          // query params, repeatable
    .proxy(Proxy::all("http://1.2.3.4:8080")?)      // a proxy, one client each
    .stamp(|req| { /* path or body edits */ })      // anything else, runs last
    .build()?
```

Headers and query values from the identity replace any the caller set under the same
name. An identity with nothing set is rejected at `build()`.

## Detectors

The default detector treats `429` as exhausted and a `2xx` with `X-RateLimit-Remaining: 0`
as the last unit of quota. It reads `Retry-After`, `RateLimit-Reset`, and `X-RateLimit-Reset`
for the reset time and never touches the body.

Upstreams that signal quota some other way get a custom detector:

```rust
use reqwest_rotate::{Verdict, HeaderDetector};

// Reads the body. Every response is buffered before it is returned.
let by_body = |status, _headers: &HeaderMap, body: &[u8]| {
    if status == 500 && body.starts_with(b"quota") {
        Verdict::Exhausted { retry_after: None }
    } else {
        Verdict::Ok
    }
};

// Reads headers only. Responses stream through untouched.
let by_header = HeaderDetector::new(|status, headers| { /* ... */ });

Client::builder().detector(by_body)
```

## Rotation

```rust
Client::builder()
    .cooldown(Duration::from_secs(60))   // default 60s, see below
    .exhausted(Exhausted::Wait)          // or Exhausted::Error, default Wait
    .configure(|b| b.timeout(Duration::from_secs(10)))  // applied to every inner client
```

**Cooldown** is how long an exhausted identity stays out of rotation when the server gives
no reset time. It is not a delay before rotating. Rotation is immediate. A `retry_after` from
the detector always takes precedence.

When every identity is cooling, `Exhausted::Wait` sleeps until the soonest reset and tries
again. `Exhausted::Error` returns `Error::AllExhausted` at once.

A request with a streaming body cannot be cloned, so it is sent once and never re-sent.

## Features

Cargo features forward one-to-one to `reqwest`: `json`, `form`, `query`, `multipart`,
`rustls`, `native-tls`, `gzip`, `brotli`, `zstd`, `deflate`, `cookies`, `stream`, `socks`,
`hickory-dns`. The defaults match `reqwest` plus `query`.

See [SPEC.md](SPEC.md) for the full design.
