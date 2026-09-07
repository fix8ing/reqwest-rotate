//! Response classification: is the active identity out of quota?

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use reqwest::header::HeaderMap;

/// Values at or above this in `X-RateLimit-Reset` are Unix timestamps, not deltas.
const UNIX_EPOCH_THRESHOLD: f64 = 1_000_000_000.0;

/// What a [`Detector`] concluded about one response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The identity is fine. Return the response.
    Ok,
    /// The identity is out of quota and this response is the rejection.
    /// The client cools the identity and re-sends under another one.
    Exhausted {
        /// Time until the identity is usable again, when the server says.
        retry_after: Option<Duration>,
    },
    /// This response succeeded but used the identity's last unit of quota.
    /// The client returns the response, then cools the identity.
    Depleted {
        /// Time until the identity is usable again, when the server says.
        retry_after: Option<Duration>,
    },
}

/// Classifies responses for the client. See [`DefaultDetector`] for the default.
///
/// Any `Fn(StatusCode, &HeaderMap, &[u8]) -> Verdict` is a detector that needs
/// the body. Wrap a header-only closure in [`HeaderDetector`] to skip buffering.
pub trait Detector: Send + Sync + 'static {
    /// Whether [`classify`](Self::classify) reads the body.
    ///
    /// When `true`, the client buffers every response before deciding and the
    /// caller cannot stream it. When `false`, the response passes through untouched.
    fn needs_body(&self) -> bool {
        true
    }

    /// Classify one response. `body` is empty when [`needs_body`](Self::needs_body) is `false`.
    fn classify(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> Verdict;
}

impl<F> Detector for F
where
    F: Fn(StatusCode, &HeaderMap, &[u8]) -> Verdict + Send + Sync + 'static,
{
    fn classify(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> Verdict {
        self(status, headers, body)
    }
}

/// A detector that decides from status and headers alone, so responses stream through.
#[derive(Debug, Clone, Copy)]
pub struct HeaderDetector<F>(F);

impl<F> HeaderDetector<F>
where
    F: Fn(StatusCode, &HeaderMap) -> Verdict + Send + Sync + 'static,
{
    /// Wrap a header-only classifier.
    pub fn new(classify: F) -> Self {
        Self(classify)
    }
}

impl<F> Detector for HeaderDetector<F>
where
    F: Fn(StatusCode, &HeaderMap) -> Verdict + Send + Sync + 'static,
{
    fn needs_body(&self) -> bool {
        false
    }

    fn classify(&self, status: StatusCode, headers: &HeaderMap, _body: &[u8]) -> Verdict {
        (self.0)(status, headers)
    }
}

/// The default detector. Needs no body.
///
/// - `429` is [`Verdict::Exhausted`].
/// - A `2xx` with `X-RateLimit-Remaining: 0` or `RateLimit-Remaining: 0` is [`Verdict::Depleted`].
/// - Everything else is [`Verdict::Ok`].
///
/// `retry_after` comes from [`retry_after`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultDetector;

impl Detector for DefaultDetector {
    fn needs_body(&self) -> bool {
        false
    }

    fn classify(&self, status: StatusCode, headers: &HeaderMap, _body: &[u8]) -> Verdict {
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Verdict::Exhausted {
                retry_after: retry_after(headers),
            };
        }
        if status.is_success() && remaining_is_zero(headers) {
            return Verdict::Depleted {
                retry_after: retry_after(headers),
            };
        }
        Verdict::Ok
    }
}

/// Time until the quota resets, read from the common headers.
///
/// Checks `Retry-After` (delta seconds), then `RateLimit-Reset` (delta seconds),
/// then `X-RateLimit-Reset` (delta seconds, or a Unix timestamp when the value is
/// at least 10^9). Fractional seconds round up. An HTTP-date `Retry-After` is not
/// parsed and yields `None`.
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    if let Some(secs) = header_number(headers, "retry-after") {
        return Some(seconds(secs));
    }
    if let Some(secs) = header_number(headers, "ratelimit-reset") {
        return Some(seconds(secs));
    }
    let reset = header_number(headers, "x-ratelimit-reset")?;
    if reset >= UNIX_EPOCH_THRESHOLD {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        return Some(seconds(reset - now));
    }
    Some(seconds(reset))
}

fn seconds(value: f64) -> Duration {
    Duration::from_secs(value.max(0.0).ceil() as u64)
}

fn header_number(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn remaining_is_zero(headers: &HeaderMap) -> bool {
    ["x-ratelimit-remaining", "ratelimit-remaining"]
        .iter()
        .any(|name| header_number(headers, name) == Some(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn too_many_requests_is_exhausted_with_retry_after() {
        let verdict = DefaultDetector.classify(
            StatusCode::TOO_MANY_REQUESTS,
            &headers(&[("retry-after", "30")]),
            &[],
        );
        assert_eq!(
            verdict,
            Verdict::Exhausted {
                retry_after: Some(Duration::from_secs(30))
            }
        );
    }

    #[test]
    fn success_with_zero_remaining_is_depleted() {
        let verdict = DefaultDetector.classify(
            StatusCode::OK,
            &headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "12")]),
            &[],
        );
        assert_eq!(
            verdict,
            Verdict::Depleted {
                retry_after: Some(Duration::from_secs(12))
            }
        );
    }

    #[test]
    fn plain_success_is_ok() {
        let verdict = DefaultDetector.classify(
            StatusCode::OK,
            &headers(&[("x-ratelimit-remaining", "7")]),
            &[],
        );
        assert_eq!(verdict, Verdict::Ok);
    }

    #[test]
    fn server_error_without_signal_is_ok() {
        let verdict =
            DefaultDetector.classify(StatusCode::INTERNAL_SERVER_ERROR, &headers(&[]), &[]);
        assert_eq!(verdict, Verdict::Ok);
    }

    #[test]
    fn unix_timestamp_reset_becomes_a_delta() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reset = (now + 90).to_string();
        let delta = retry_after(&headers(&[("x-ratelimit-reset", &reset)])).unwrap();
        assert!((89..=91).contains(&delta.as_secs()), "{delta:?}");
    }

    #[test]
    fn fractional_seconds_round_up() {
        let delta = retry_after(&headers(&[("retry-after", "1.2")])).unwrap();
        assert_eq!(delta, Duration::from_secs(2));
    }

    #[test]
    fn http_date_retry_after_is_ignored() {
        let h = headers(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]);
        assert_eq!(retry_after(&h), None);
    }

    #[test]
    fn header_detector_skips_body() {
        let detector = HeaderDetector::new(|status, _: &HeaderMap| {
            if status == StatusCode::SERVICE_UNAVAILABLE {
                Verdict::Exhausted { retry_after: None }
            } else {
                Verdict::Ok
            }
        });
        assert!(!detector.needs_body());
        assert_eq!(
            detector.classify(StatusCode::SERVICE_UNAVAILABLE, &headers(&[]), &[]),
            Verdict::Exhausted { retry_after: None }
        );
    }

    #[test]
    fn closure_detector_needs_body() {
        let detector = |_: StatusCode, _: &HeaderMap, body: &[u8]| {
            if body == b"quota" {
                Verdict::Exhausted { retry_after: None }
            } else {
                Verdict::Ok
            }
        };
        assert!(Detector::needs_body(&detector));
        assert_eq!(
            detector.classify(StatusCode::OK, &headers(&[]), b"quota"),
            Verdict::Exhausted { retry_after: None }
        );
    }
}
