# reqwest-rotate

## Goal

A rate-limit-aware, drop-in replacement for `reqwest::Client`. It holds a pool of
identities, sends each request under the active one, and rotates to the next identity
when a detector reports the current one is exhausted. The caller sees one client and
never handles rate limits itself.

With no identities configured, the client is a plain `reqwest::Client`. Same request
builder, same `reqwest::Response`, no buffering, no detection.

Standalone crate. No dependency on any private code.

## Identity

An identity is everything that distinguishes one credential from another, applied to
every request sent under it. Built with a builder whose defaults are all empty:

```rust
let id = Identity::builder()
    .header("x-api-key", key)
    .query("api_key", key)
    .proxy(Proxy::all("http://1.2.3.4:8080")?)
    .stamp(|req| { /* path or body edits */ })
    .build()?;
```

- `header(name, value)` merges into every request. Repeatable. Replaces any header of
  the same name the caller set on the request.
- `query(name, value)` appends to every request URL. Repeatable. Keeps the caller's own
  parameters. If the caller set a parameter of the same name, the identity's value wins,
  since rotation is the whole point.
- `proxy(Proxy)` routes every request. `reqwest` sets proxies at client level, so each
  distinct proxy gets its own `reqwest::Client` inside the pool. Proxy-less identities
  share one direct client.
- `stamp(Fn(&mut Request))` runs last, for path keys, body fields, or signatures.
- `build()` fails if nothing was set. An empty identity is indistinguishable from any
  other. It also fails on an invalid header name or value.

`Debug` on an identity prints header and query names only. Values never reach logs.

## Detector

A caller-supplied classifier for each response:

```rust
pub enum Verdict {
    /// The identity is fine. Return the response.
    Ok,
    /// The identity is out of quota and this response is the rejection.
    /// Cool the identity and re-send the request under another one.
    Exhausted { retry_after: Option<Duration> },
    /// This response succeeded but used the last of the identity's quota.
    /// Return the response, then cool the identity before its next request.
    Depleted { retry_after: Option<Duration> },
}

pub trait Detector: Send + Sync + 'static {
    /// Whether `classify` reads the body. Default `true`.
    fn needs_body(&self) -> bool;
    fn classify(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> Verdict;
}
```

`Depleted` exists because a `200` with `X-RateLimit-Remaining: 0` must not be re-sent.
It succeeded. Only the next request needs a different identity.

- Any `Fn(StatusCode, &HeaderMap, &[u8]) -> Verdict` is a detector that needs the body.
- `HeaderDetector::new(Fn(StatusCode, &HeaderMap) -> Verdict)` is one that does not.
- `DefaultDetector` needs no body. `429` is `Exhausted`. A `2xx` with
  `X-RateLimit-Remaining: 0` or `RateLimit-Remaining: 0` is `Depleted`. `retry_after`
  is parsed from `Retry-After` (seconds), `RateLimit-Reset` (seconds), or
  `X-RateLimit-Reset` (seconds, or a Unix timestamp when the value is large). Everything
  else is `Ok`. The parser is public as `retry_after(&HeaderMap)` so custom detectors can
  reuse it.

When the detector needs the body, the client buffers the response before deciding, then
rebuilds a `reqwest::Response` with the same status, version, headers, extensions, URL,
and body. The caller cannot stream that response. When the detector does not need the
body, the response passes through untouched.

## Pool and rotation

```rust
let client = Client::builder()
    .identity(id_a)
    .identity(id_b)
    .detector(my_detector)              // default: DefaultDetector
    .cooldown(Duration::from_secs(60))  // default: 60s
    .exhausted(Exhausted::Wait)         // default: Wait
    .configure(|b| b.timeout(Duration::from_secs(10)))
    .build()?;
```

- Identities rotate round-robin. On `Exhausted` the active identity enters cooldown, the
  client moves to the next identity that is not cooling, and re-sends the same request.
  On `Depleted` the identity enters cooldown and the response is returned.
- **Cooldown** is how long an exhausted identity stays out of rotation when the server
  gives no reset time. It is not a delay before rotating. Rotation is immediate. When the
  detector supplies `retry_after`, that value is used instead. Default 60 seconds.
- When every identity is cooling, `Exhausted::Wait` sleeps until the soonest reset and
  tries again. `Exhausted::Error` returns `Error::AllExhausted { retry_after }` at once.
- A request whose body cannot be cloned (a stream) is sent once under the active identity
  and never re-sent. The detector still runs and the identity still cools. The response
  is returned as-is, even if it is the rejection.
- `configure` applies to every `reqwest::Client` the pool builds, so timeouts, TLS, and
  default headers are set once.
- Per-identity state is one mutex-guarded `Option<Instant>`. No background tasks.

## Client surface

Mirrors `reqwest::Client`:

```rust
let resp = client.get(url).query(&[("limit", 50)]).send().await?;
```

- `Client::new()`, `Client::builder()`, `Client::from(reqwest::Client)`.
- `get`, `post`, `put`, `patch`, `delete`, `head`, `request(Method, url)`,
  `execute(Request)`.
- `RequestBuilder` wraps `reqwest::RequestBuilder` and exposes the same methods:
  `header`, `headers`, `basic_auth`, `bearer_auth`, `body`, `timeout`, `version`,
  `query`, `form`, `json`, `multipart`, `build`, `try_clone`, `send`. The last five
  are feature-gated exactly as in `reqwest`.
- `send()` and `execute()` return `Result<reqwest::Response, Error>`. `Error` wraps
  `reqwest::Error` transparently and adds `AllExhausted`. This is the one type that
  differs from `reqwest`, because `reqwest::Error` cannot be constructed outside
  `reqwest`.
- Cargo features forward one-to-one to `reqwest` (`json`, `rustls`, `gzip`, ...).

## Observability

`tracing` events on every rotation and cooldown, carrying the identity index and the
verdict. Identity values are never logged. Metrics are out of scope for v1.

## Non-goals

- Not a general retry library. Only `Exhausted` triggers a re-send. Timeouts, `5xx`
  without a quota signal, and network errors bubble up unchanged.
- Not a credential store. The caller builds every identity.
- Not an evasion toolkit. It rotates credentials the caller owns and does nothing to
  hide that from the upstream.
- No proactive local throttling in v1. `Depleted` covers the common case where the
  server announces the last unit of quota. Local quotas can come later without changing
  the surface.

## Dependencies

`reqwest`, `http`, `bytes`, `tokio` (time only), `thiserror`, `tracing`. `serde` only
behind the `query`, `form`, and `json` features, mirroring `reqwest`.
