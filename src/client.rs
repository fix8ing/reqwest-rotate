//! The drop-in client, its builder, and the request builder.

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Body, IntoUrl, Method, Request, Response, ResponseBuilderExt, Version};
use tracing::{debug, warn};

use crate::detector::{DefaultDetector, Detector, Verdict};
use crate::error::Error;
use crate::identity::Identity;

/// How long an exhausted identity stays out of rotation when the server gives no reset time.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

/// What `send` does when every identity is cooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Exhausted {
    /// Sleep until the soonest identity resets, then try again.
    #[default]
    Wait,
    /// Return [`Error::AllExhausted`] at once.
    Error,
}

type Configure = Box<dyn Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync>;

struct Slot {
    identity: Identity,
    client: reqwest::Client,
    cooling_until: Mutex<Option<Instant>>,
}

struct Inner {
    /// The client for requests with no proxy. Also builds every `RequestBuilder`.
    direct: reqwest::Client,
    slots: Vec<Slot>,
    /// Index of the identity the next request starts from.
    active: AtomicUsize,
    detector: Box<dyn Detector>,
    cooldown: Duration,
    on_exhausted: Exhausted,
}

/// A `reqwest::Client` that rotates identities when the active one is rate-limited.
///
/// With no identities it is a plain `reqwest::Client`. See the crate docs.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    /// A client with no identities and default settings.
    ///
    /// # Panics
    ///
    /// Panics if the TLS backend cannot initialise, like `reqwest::Client::new`.
    /// Use [`Client::builder`] to handle the error instead.
    pub fn new() -> Self {
        ClientBuilder::new().build().expect("Client::new()")
    }

    /// Start configuring a client.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Start a `GET` request.
    pub fn get<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    /// Start a `POST` request.
    pub fn post<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// Start a `PUT` request.
    pub fn put<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    /// Start a `PATCH` request.
    pub fn patch<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }

    /// Start a `DELETE` request.
    pub fn delete<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }

    /// Start a `HEAD` request.
    pub fn head<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }

    /// Start a request with any method.
    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            inner: self.inner.direct.request(method, url),
        }
    }

    /// Send a built request under the active identity, rotating on exhaustion.
    ///
    /// A request whose body cannot be cloned is sent once and never re-sent,
    /// even if the response is the rate-limit rejection.
    pub async fn execute(&self, request: Request) -> Result<Response, Error> {
        if self.inner.slots.is_empty() {
            return Ok(self.inner.direct.execute(request).await?);
        }
        let Some(template) = request.try_clone() else {
            let idx = self.pick().await?;
            let (response, verdict) = self.attempt(idx, request).await?;
            match verdict {
                Verdict::Ok => {}
                Verdict::Depleted { retry_after } => self.cool(idx, retry_after),
                Verdict::Exhausted { retry_after } => {
                    self.cool(idx, retry_after);
                    warn!(
                        identity = idx,
                        "request body is not replayable; returning the rate-limited response"
                    );
                }
            }
            return Ok(response);
        };
        let mut next = request;
        loop {
            let idx = self.pick().await?;
            let (response, verdict) = self.attempt(idx, next).await?;
            match verdict {
                Verdict::Ok => return Ok(response),
                Verdict::Depleted { retry_after } => {
                    self.cool(idx, retry_after);
                    return Ok(response);
                }
                Verdict::Exhausted { retry_after } => {
                    self.cool(idx, retry_after);
                    next = template
                        .try_clone()
                        .expect("template body was cloned once already");
                }
            }
        }
    }

    /// Send one request under one identity and classify the response.
    async fn attempt(
        &self,
        idx: usize,
        mut request: Request,
    ) -> Result<(Response, Verdict), Error> {
        let slot = &self.inner.slots[idx];
        slot.identity.apply(&mut request);
        let response = slot.client.execute(request).await?;
        let detector = &self.inner.detector;
        if !detector.needs_body() {
            let verdict = detector.classify(response.status(), response.headers(), &[]);
            return Ok((response, verdict));
        }
        let status = response.status();
        let version = response.version();
        let url = response.url().clone();
        let headers = response.headers().clone();
        let extensions = response.extensions().clone();
        let body = response.bytes().await?;
        let verdict = detector.classify(status, &headers, &body);
        let mut rebuilt = http::Response::builder()
            .status(status)
            .version(version)
            .url(url)
            .body(body)
            .expect("status and version come from a received response");
        *rebuilt.headers_mut() = headers;
        rebuilt.extensions_mut().extend(extensions);
        Ok((Response::from(rebuilt), verdict))
    }

    /// The next identity that is not cooling, honouring the exhausted policy.
    async fn pick(&self) -> Result<usize, Error> {
        loop {
            match self.next_ready() {
                Ok(idx) => return Ok(idx),
                Err(soonest) => {
                    let retry_after = soonest.saturating_duration_since(Instant::now());
                    match self.inner.on_exhausted {
                        Exhausted::Error => return Err(Error::AllExhausted { retry_after }),
                        Exhausted::Wait => {
                            debug!(?retry_after, "every identity is cooling; waiting");
                            tokio::time::sleep(retry_after).await;
                        }
                    }
                }
            }
        }
    }

    /// Round-robin from `active`. `Err` carries the soonest reset when all are cooling.
    fn next_ready(&self) -> Result<usize, Instant> {
        let inner = &*self.inner;
        let now = Instant::now();
        let count = inner.slots.len();
        let start = inner.active.load(Ordering::Relaxed);
        let mut soonest: Option<Instant> = None;
        for offset in 0..count {
            let idx = (start + offset) % count;
            let mut cooling = lock(&inner.slots[idx].cooling_until);
            match *cooling {
                Some(until) if until > now => {
                    soonest = Some(soonest.map_or(until, |s| s.min(until)));
                }
                _ => {
                    *cooling = None;
                    drop(cooling);
                    inner.active.store(idx, Ordering::Relaxed);
                    return Ok(idx);
                }
            }
        }
        Err(soonest.expect("every slot is cooling, so one reset is soonest"))
    }

    /// Take an identity out of rotation and move `active` past it.
    fn cool(&self, idx: usize, retry_after: Option<Duration>) {
        let inner = &*self.inner;
        let wait = retry_after.unwrap_or(inner.cooldown);
        *lock(&inner.slots[idx].cooling_until) = Some(Instant::now() + wait);
        inner
            .active
            .store((idx + 1) % inner.slots.len(), Ordering::Relaxed);
        debug!(
            identity = idx,
            cooldown_secs = wait.as_secs(),
            "identity cooling"
        );
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl From<reqwest::Client> for Client {
    /// Wrap an existing client with no identities. Every request passes straight through.
    fn from(direct: reqwest::Client) -> Self {
        Self {
            inner: Arc::new(Inner {
                direct,
                slots: Vec::new(),
                active: AtomicUsize::new(0),
                detector: Box::new(DefaultDetector),
                cooldown: DEFAULT_COOLDOWN,
                on_exhausted: Exhausted::default(),
            }),
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("identities", &self.inner.slots.len())
            .field("cooldown", &self.inner.cooldown)
            .field("on_exhausted", &self.inner.on_exhausted)
            .finish()
    }
}

/// Builder for [`Client`].
pub struct ClientBuilder {
    identities: Vec<Identity>,
    detector: Box<dyn Detector>,
    cooldown: Duration,
    on_exhausted: Exhausted,
    configure: Option<Configure>,
}

impl ClientBuilder {
    /// A builder with no identities, the [`DefaultDetector`], a 60 second cooldown,
    /// and [`Exhausted::Wait`].
    pub fn new() -> Self {
        Self {
            identities: Vec::new(),
            detector: Box::new(DefaultDetector),
            cooldown: DEFAULT_COOLDOWN,
            on_exhausted: Exhausted::default(),
            configure: None,
        }
    }

    /// Add one identity to the pool. Order is rotation order.
    pub fn identity(mut self, identity: Identity) -> Self {
        self.identities.push(identity);
        self
    }

    /// Add several identities to the pool.
    pub fn identities(mut self, identities: impl IntoIterator<Item = Identity>) -> Self {
        self.identities.extend(identities);
        self
    }

    /// Replace the [`DefaultDetector`].
    pub fn detector(mut self, detector: impl Detector) -> Self {
        self.detector = Box::new(detector);
        self
    }

    /// How long an exhausted identity stays out of rotation when the server
    /// gives no reset time. Default 60 seconds.
    ///
    /// This is not a delay before rotating. Rotation is immediate. It only
    /// decides when the exhausted identity is tried again. A `retry_after`
    /// from the detector always takes precedence.
    pub fn cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    /// What to do when every identity is cooling. Default [`Exhausted::Wait`].
    pub fn exhausted(mut self, policy: Exhausted) -> Self {
        self.on_exhausted = policy;
        self
    }

    /// Configure every `reqwest::Client` the pool builds: timeouts, TLS,
    /// default headers, and so on. Runs once per distinct proxy plus once
    /// for the direct client.
    pub fn configure(
        mut self,
        configure: impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync + 'static,
    ) -> Self {
        self.configure = Some(Box::new(configure));
        self
    }

    /// Build the client. Fails only if `reqwest` fails to build a client.
    pub fn build(self) -> Result<Client, reqwest::Error> {
        let Self {
            identities,
            detector,
            cooldown,
            on_exhausted,
            configure,
        } = self;
        let configure = |builder: reqwest::ClientBuilder| match &configure {
            Some(f) => f(builder),
            None => builder,
        };
        let direct = configure(reqwest::ClientBuilder::new()).build()?;
        let mut slots = Vec::with_capacity(identities.len());
        for identity in identities {
            let client = match identity.proxy() {
                Some(proxy) => configure(reqwest::ClientBuilder::new())
                    .proxy(proxy.clone())
                    .build()?,
                None => direct.clone(),
            };
            slots.push(Slot {
                identity,
                client,
                cooling_until: Mutex::new(None),
            });
        }
        Ok(Client {
            inner: Arc::new(Inner {
                direct,
                slots,
                active: AtomicUsize::new(0),
                detector,
                cooldown,
                on_exhausted,
            }),
        })
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("identities", &self.identities)
            .field("cooldown", &self.cooldown)
            .field("on_exhausted", &self.on_exhausted)
            .field("configure", &self.configure.is_some())
            .finish()
    }
}

/// A request under construction. Mirrors `reqwest::RequestBuilder`.
pub struct RequestBuilder {
    client: Client,
    inner: reqwest::RequestBuilder,
}

impl RequestBuilder {
    /// Assemble a builder from a client and an already-built request.
    pub fn from_parts(client: Client, request: Request) -> Self {
        let inner = reqwest::RequestBuilder::from_parts(client.inner.direct.clone(), request);
        Self { client, inner }
    }

    /// Add a header.
    pub fn header<K, V>(self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        self.map(|b| b.header(key, value))
    }

    /// Add a set of headers.
    pub fn headers(self, headers: HeaderMap) -> Self {
        self.map(|b| b.headers(headers))
    }

    /// Enable HTTP basic authentication.
    pub fn basic_auth<U, P>(self, username: U, password: Option<P>) -> Self
    where
        U: fmt::Display,
        P: fmt::Display,
    {
        self.map(|b| b.basic_auth(username, password))
    }

    /// Enable HTTP bearer authentication.
    pub fn bearer_auth<T: fmt::Display>(self, token: T) -> Self {
        self.map(|b| b.bearer_auth(token))
    }

    /// Set the request body.
    pub fn body<T: Into<Body>>(self, body: T) -> Self {
        self.map(|b| b.body(body))
    }

    /// Set a per-request timeout.
    pub fn timeout(self, timeout: Duration) -> Self {
        self.map(|b| b.timeout(timeout))
    }

    /// Set the HTTP version.
    pub fn version(self, version: Version) -> Self {
        self.map(|b| b.version(version))
    }

    /// Append query parameters.
    #[cfg(feature = "query")]
    pub fn query<T: serde::Serialize + ?Sized>(self, query: &T) -> Self {
        self.map(|b| b.query(query))
    }

    /// Send a form body.
    #[cfg(feature = "form")]
    pub fn form<T: serde::Serialize + ?Sized>(self, form: &T) -> Self {
        self.map(|b| b.form(form))
    }

    /// Send a JSON body.
    #[cfg(feature = "json")]
    pub fn json<T: serde::Serialize + ?Sized>(self, json: &T) -> Self {
        self.map(|b| b.json(json))
    }

    /// Send a multipart form body.
    #[cfg(feature = "multipart")]
    pub fn multipart(self, form: reqwest::multipart::Form) -> Self {
        self.map(|b| b.multipart(form))
    }

    /// Build the request without sending it.
    pub fn build(self) -> Result<Request, reqwest::Error> {
        self.inner.build()
    }

    /// Clone the builder. `None` if the body is a stream.
    pub fn try_clone(&self) -> Option<Self> {
        self.inner.try_clone().map(|inner| Self {
            client: self.client.clone(),
            inner,
        })
    }

    /// Build and send the request. See [`Client::execute`].
    pub async fn send(self) -> Result<Response, Error> {
        let request = self.inner.build()?;
        self.client.execute(request).await
    }

    fn map(self, f: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> Self {
        Self {
            client: self.client,
            inner: f(self.inner),
        }
    }
}

impl fmt::Debug for RequestBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
