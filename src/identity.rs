//! Identities: the credential a request is sent under.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Proxy, Request};
use thiserror::Error;

/// A hook that edits a request just before it is sent.
pub type Stamp = Arc<dyn Fn(&mut Request) + Send + Sync>;

/// Error from [`IdentityBuilder::build`].
#[derive(Debug, Error)]
pub enum IdentityError {
    /// No header, query parameter, proxy, or stamp was set.
    #[error("identity has no headers, query parameters, proxy, or stamp")]
    Empty,
    /// A header name or value was invalid.
    #[error("invalid header: {0}")]
    Header(#[from] http::Error),
}

/// One credential: everything applied to every request sent under it.
///
/// Headers, query parameters, a proxy, a stamp hook, or any combination.
/// Build one with [`Identity::builder`].
#[derive(Clone)]
pub struct Identity {
    headers: HeaderMap,
    query: Vec<(String, String)>,
    proxy: Option<Proxy>,
    stamp: Option<Stamp>,
}

impl Identity {
    /// Start an identity with nothing set.
    pub fn builder() -> IdentityBuilder {
        IdentityBuilder::default()
    }

    /// The proxy every request under this identity routes through, if any.
    pub fn proxy(&self) -> Option<&Proxy> {
        self.proxy.as_ref()
    }

    /// Stamp this identity onto a request. Headers and query values from the
    /// identity replace any the caller set under the same name.
    pub(crate) fn apply(&self, request: &mut Request) {
        if !self.headers.is_empty() {
            let headers = request.headers_mut();
            for name in self.headers.keys() {
                headers.remove(name);
            }
            for (name, value) in &self.headers {
                headers.append(name.clone(), value.clone());
            }
        }
        if !self.query.is_empty() {
            let url = request.url_mut();
            let overridden: HashSet<&str> = self.query.iter().map(|(k, _)| k.as_str()).collect();
            let kept: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(k, _)| !overridden.contains(k.as_ref()))
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            let mut pairs = url.query_pairs_mut();
            pairs.clear();
            for (k, v) in &kept {
                pairs.append_pair(k, v);
            }
            for (k, v) in &self.query {
                pairs.append_pair(k, v);
            }
        }
        if let Some(stamp) = &self.stamp {
            stamp(request);
        }
    }
}

impl fmt::Debug for Identity {
    /// Prints names only. Header and query values never reach logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field(
                "query",
                &self.query.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("proxy", &self.proxy.is_some())
            .field("stamp", &self.stamp.is_some())
            .finish()
    }
}

/// Builder for [`Identity`]. Every field starts empty.
#[derive(Default)]
pub struct IdentityBuilder {
    headers: HeaderMap,
    query: Vec<(String, String)>,
    proxy: Option<Proxy>,
    stamp: Option<Stamp>,
    error: Option<http::Error>,
}

impl IdentityBuilder {
    /// Add a header sent with every request. Repeatable.
    ///
    /// An invalid name or value is reported by [`build`](Self::build).
    pub fn header<K, V>(mut self, name: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        if self.error.is_some() {
            return self;
        }
        let name = HeaderName::try_from(name).map_err(Into::into);
        let value = HeaderValue::try_from(value).map_err(Into::into);
        match (name, value) {
            (Ok(name), Ok(mut value)) => {
                value.set_sensitive(true);
                self.headers.append(name, value);
            }
            (Err(e), _) | (_, Err(e)) => self.error = Some(e),
        }
        self
    }

    /// Add a query parameter appended to every request URL. Repeatable.
    pub fn query(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((name.into(), value.into()));
        self
    }

    /// Route every request through this proxy.
    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.proxy = Some(proxy);
        self
    }

    /// Run a hook on every request after headers and query are applied.
    /// Use it for keys in the path, the body, or a request signature.
    pub fn stamp(mut self, stamp: impl Fn(&mut Request) + Send + Sync + 'static) -> Self {
        self.stamp = Some(Arc::new(stamp));
        self
    }

    /// Finish the identity. Fails if nothing was set or a header was invalid.
    pub fn build(self) -> Result<Identity, IdentityError> {
        if let Some(e) = self.error {
            return Err(IdentityError::Header(e));
        }
        let empty = self.headers.is_empty()
            && self.query.is_empty()
            && self.proxy.is_none()
            && self.stamp.is_none();
        if empty {
            return Err(IdentityError::Empty);
        }
        Ok(Identity {
            headers: self.headers,
            query: self.query,
            proxy: self.proxy,
            stamp: self.stamp,
        })
    }
}

impl fmt::Debug for IdentityBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityBuilder")
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field(
                "query",
                &self.query.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("proxy", &self.proxy.is_some())
            .field("stamp", &self.stamp.is_some())
            .field("error", &self.error)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use reqwest::{Method, Url};

    fn request(url: &str) -> Request {
        Request::new(Method::GET, Url::parse(url).unwrap())
    }

    #[test]
    fn empty_identity_is_rejected() {
        assert!(matches!(
            Identity::builder().build(),
            Err(IdentityError::Empty)
        ));
    }

    #[test]
    fn invalid_header_is_reported_at_build() {
        let result = Identity::builder().header("bad header", "v").build();
        assert!(matches!(result, Err(IdentityError::Header(_))));
    }

    #[test]
    fn header_replaces_callers_value() {
        let id = Identity::builder()
            .header("x-api-key", "mine")
            .build()
            .unwrap();
        let mut req = request("http://h/p");
        req.headers_mut()
            .insert("x-api-key", "theirs".parse().unwrap());
        id.apply(&mut req);
        let values: Vec<_> = req.headers().get_all("x-api-key").iter().collect();
        assert_eq!(values, ["mine"]);
    }

    #[test]
    fn query_keeps_callers_params_and_overrides_same_name() {
        let id = Identity::builder()
            .query("api_key", "mine")
            .build()
            .unwrap();
        let mut req = request("http://h/p?limit=5&api_key=theirs&page=2");
        id.apply(&mut req);
        assert_eq!(req.url().query(), Some("limit=5&page=2&api_key=mine"));
    }

    #[test]
    fn query_on_bare_url_adds_it() {
        let id = Identity::builder().query("api_key", "k").build().unwrap();
        let mut req = request("http://h/p");
        id.apply(&mut req);
        assert_eq!(req.url().as_str(), "http://h/p?api_key=k");
    }

    #[test]
    fn stamp_runs_last() {
        let id = Identity::builder()
            .query("a", "1")
            .stamp(|req| {
                let path = format!("{}/stamped", req.url().path());
                req.url_mut().set_path(&path);
            })
            .build()
            .unwrap();
        let mut req = request("http://h/p");
        id.apply(&mut req);
        assert_eq!(req.url().as_str(), "http://h/p/stamped?a=1");
    }

    #[test]
    fn debug_hides_values() {
        let id = Identity::builder()
            .header("x-api-key", "secret")
            .query("token", "secret")
            .build()
            .unwrap();
        let text = format!("{id:?}");
        assert!(text.contains("x-api-key"));
        assert!(text.contains("token"));
        assert!(!text.contains("secret"));
    }
}
