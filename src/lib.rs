//! A drop-in `reqwest::Client` that rotates identities when the active one is rate-limited.
//!
//! An [`Identity`] is a set of headers, query parameters, a proxy, a stamp hook, or any
//! combination. The [`Client`] sends each request under the active identity. When the
//! [`Detector`] reports the response as [`Verdict::Exhausted`], the client cools that
//! identity, moves to the next one, and re-sends. With no identities the client is a plain
//! `reqwest::Client`.
//!
//! ```no_run
//! use reqwest_rotate::{Client, Identity};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::builder()
//!     .identity(Identity::builder().header("x-api-key", "key-a").build()?)
//!     .identity(Identity::builder().header("x-api-key", "key-b").build()?)
//!     .build()?;
//!
//! let body = client
//!     .get("https://api.example.com/v1/things")
//!     .send()
//!     .await?
//!     .text()
//!     .await?;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod client;
mod detector;
mod error;
mod identity;

pub use client::{Client, ClientBuilder, DEFAULT_COOLDOWN, Exhausted, RequestBuilder};
pub use detector::{DefaultDetector, Detector, HeaderDetector, Verdict, retry_after};
pub use error::Error;
pub use identity::{Identity, IdentityBuilder, IdentityError, Stamp};

pub use reqwest;
pub use reqwest::{
    Body, IntoUrl, Method, Proxy, Request, Response, StatusCode, Url, Version, header,
};
