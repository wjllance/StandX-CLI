//! Validated StandX REST and WebSocket endpoint selection.

use crate::error::{Error, Result};
use std::net::IpAddr;
use url::{Host, Url};

pub const DEFAULT_BASE_URL: &str = "https://perps.standx.com";

/// All StandX transport endpoints derived from one REST base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandXEndpoints {
    base_url: String,
    stream_url: String,
    order_response_url: String,
}

impl Default for StandXEndpoints {
    fn default() -> Self {
        Self::new(DEFAULT_BASE_URL).expect("the production endpoint must be valid")
    }
}

impl StandXEndpoints {
    /// Validate a root-level HTTP(S) base URL and derive its WebSocket URLs.
    pub fn new(base_url: impl AsRef<str>) -> Result<Self> {
        let raw = base_url.as_ref().trim();
        let mut parsed = Url::parse(raw).map_err(|error| endpoint_error(error.to_string()))?;

        if parsed.host_str().is_none() {
            return Err(endpoint_error("a host is required"));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(endpoint_error("userinfo is not allowed"));
        }
        if parsed.query().is_some() {
            return Err(endpoint_error("query parameters are not allowed"));
        }
        if parsed.fragment().is_some() {
            return Err(endpoint_error("fragments are not allowed"));
        }
        if !matches!(parsed.path(), "" | "/") {
            return Err(endpoint_error("the base URL path must be empty or '/'"));
        }

        match parsed.scheme() {
            "https" => {}
            "http" if is_loopback(&parsed) => {}
            "http" => {
                return Err(endpoint_error(
                    "plain HTTP is allowed only for localhost or loopback addresses",
                ));
            }
            _ => {
                return Err(endpoint_error(
                    "scheme must be https (or http for loopback testing)",
                ));
            }
        }

        parsed.set_path("");
        let base_url = parsed.as_str().trim_end_matches('/').to_string();
        let ws_scheme = if parsed.scheme() == "https" {
            "wss"
        } else {
            "ws"
        };
        parsed
            .set_scheme(ws_scheme)
            .map_err(|_| endpoint_error("could not derive WebSocket scheme"))?;

        parsed.set_path("/ws-stream/v1");
        let stream_url = parsed.to_string();
        parsed.set_path("/ws-api/v1");
        let order_response_url = parsed.to_string();

        Ok(Self {
            base_url,
            stream_url,
            order_response_url,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn stream_url(&self) -> &str {
        &self.stream_url
    }

    pub fn order_response_url(&self) -> &str {
        &self.order_response_url
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => IpAddr::V4(address).is_loopback(),
        Some(Host::Ipv6(address)) => IpAddr::V6(address).is_loopback(),
        None => false,
    }
}

fn endpoint_error(detail: impl std::fmt::Display) -> Error {
    Error::Config {
        message: format!("invalid StandX endpoint: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_production_and_custom_transport_urls() {
        let production = StandXEndpoints::default();
        assert_eq!(production.base_url(), "https://perps.standx.com");
        assert_eq!(
            production.stream_url(),
            "wss://perps.standx.com/ws-stream/v1"
        );
        assert_eq!(
            production.order_response_url(),
            "wss://perps.standx.com/ws-api/v1"
        );

        let custom = StandXEndpoints::new("https://perps.example.com/").unwrap();
        assert_eq!(custom.base_url(), "https://perps.example.com");
        assert_eq!(custom.stream_url(), "wss://perps.example.com/ws-stream/v1");
        assert_eq!(
            custom.order_response_url(),
            "wss://perps.example.com/ws-api/v1"
        );
    }

    #[test]
    fn permits_plain_http_only_for_loopback_testing() {
        let ipv4 = StandXEndpoints::new("http://127.0.0.1:8080").unwrap();
        assert_eq!(ipv4.stream_url(), "ws://127.0.0.1:8080/ws-stream/v1");

        let ipv6 = StandXEndpoints::new("http://[::1]:8080/").unwrap();
        assert_eq!(ipv6.order_response_url(), "ws://[::1]:8080/ws-api/v1");

        let localhost = StandXEndpoints::new("http://localhost:8080").unwrap();
        assert_eq!(localhost.base_url(), "http://localhost:8080");

        assert!(StandXEndpoints::new("http://example.com").is_err());
    }

    #[test]
    fn rejects_ambiguous_or_credential_bearing_urls() {
        for value in [
            "",
            "perps.example.com",
            "ftp://example.com",
            "https://user@example.com",
            "https://example.com/api",
            "https://example.com?env=canary",
            "https://example.com/#fragment",
        ] {
            assert!(StandXEndpoints::new(value).is_err(), "{value} must fail");
        }
    }

    #[test]
    fn validation_errors_never_echo_endpoint_secrets() {
        let secret = "endpoint-secret";
        let error =
            StandXEndpoints::new(format!("https://alice:{secret}@example.com")).unwrap_err();
        assert!(!error.to_string().contains(secret));
        assert!(error.to_string().contains("userinfo is not allowed"));
    }
}
