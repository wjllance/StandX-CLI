//! HTTP client for StandX API

pub mod account;
pub mod order;

use crate::auth::{Credentials, StandXSigner};
use crate::endpoints::StandXEndpoints;
use crate::error::{Error, Result};
use crate::models::*;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, ClientBuilder};

use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// StandX API client
#[derive(Debug, Clone)]
pub struct StandXClient {
    client: Client,
    base_url: String,
    session_id: Option<String>,
}

impl StandXClient {
    /// Create a new client
    pub fn new() -> Result<Self> {
        Self::from_endpoints(&StandXEndpoints::default())
    }

    /// Create a new client from a validated StandX endpoint set.
    pub fn from_endpoints(endpoints: &StandXEndpoints) -> Result<Self> {
        let client = ClientBuilder::new()
            .timeout(DEFAULT_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .build()?;

        Ok(Self {
            client,
            base_url: endpoints.base_url().to_string(),
            session_id: None,
        })
    }

    /// Create a new client with custom base URL
    pub fn with_base_url(base_url: String) -> Result<Self> {
        Self::from_endpoints(&StandXEndpoints::new(base_url)?)
    }

    /// Get the base URL
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Attach a stable order-response session ID to signed order requests.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Current order-response session ID, if configured.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Build authenticated headers with optional request signing
    pub async fn build_auth_headers(&self, payload: Option<&str>) -> Result<HeaderMap> {
        let creds = Credentials::load()?;

        if creds.is_expired() {
            return Err(Error::AuthRequired {
                message: "Token expired".to_string(),
                resolution: "Run 'standx auth login' or set STANDX_JWT environment variable"
                    .to_string(),
            });
        }

        let mut headers = HeaderMap::new();

        // Authorization header with JWT
        let auth_value = format!("Bearer {}", creds.token);
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&auth_value).map_err(|e| Error::Api {
                code: 500,
                message: e.to_string(),
                endpoint: None,
                retryable: false,
            })?,
        );

        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        if let Some(session_id) = &self.session_id {
            headers.insert(
                "x-session-id",
                HeaderValue::from_str(session_id).map_err(|e| Error::Validation {
                    field: "session_id".to_string(),
                    message: e.to_string(),
                })?,
            );
        }

        // Add request signature if private key is available
        if !creds.private_key.is_empty() {
            if let Ok(signer) = StandXSigner::from_base58(&creds.private_key) {
                let payload_str = payload.unwrap_or("");
                let signature = signer.sign_request_now(payload_str);

                headers.insert(
                    "x-request-sign-version",
                    HeaderValue::from_str(&signature.version).unwrap(),
                );
                headers.insert(
                    "x-request-id",
                    HeaderValue::from_str(&signature.request_id).unwrap(),
                );
                headers.insert(
                    "x-request-timestamp",
                    HeaderValue::from_str(&signature.timestamp.to_string()).unwrap(),
                );
                headers.insert(
                    "x-request-signature",
                    HeaderValue::from_str(&signature.signature).unwrap(),
                );
            }
        }

        Ok(headers)
    }

    // ==================== Public API ====================

    /// Get all trading symbols information
    pub async fn get_symbol_info(&self) -> Result<Vec<SymbolInfo>> {
        let url = format!("{}/api/query_symbol_info", self.base_url);
        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_symbol_info".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let data = response.json::<Vec<SymbolInfo>>().await?;
        Ok(data)
    }

    /// Get market data for a symbol (includes funding rate)
    pub async fn get_symbol_market(&self, symbol: &str) -> Result<MarketData> {
        let url = format!("{}/api/query_symbol_market", self.base_url);
        let response = self
            .client
            .get(&url)
            .query(&[("symbol", symbol)])
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_symbol_market".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let data = response.json::<MarketData>().await?;
        Ok(data)
    }

    /// Get price data for a symbol
    pub async fn get_symbol_price(&self, symbol: &str) -> Result<PriceData> {
        let url = format!("{}/api/query_symbol_price", self.base_url);
        let response = self
            .client
            .get(&url)
            .query(&[("symbol", symbol)])
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_symbol_price".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let data = response.json::<PriceData>().await?;
        Ok(data)
    }

    /// Get recent trades for a symbol
    pub async fn get_recent_trades(&self, symbol: &str, limit: Option<u32>) -> Result<Vec<Trade>> {
        let url = format!("{}/api/query_recent_trades", self.base_url);
        let mut query = vec![("symbol", symbol.to_string())];

        if let Some(l) = limit {
            query.push(("limit", l.to_string()));
        }

        let response = self.client.get(&url).query(&query).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_recent_trades".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let text = response.text().await.unwrap_or_default();

        // Parse the JSON - API returns array directly, not wrapped
        let trades: Vec<Trade> = serde_json::from_str(&text).map_err(|e| Error::Api {
            code: 0,
            message: format!("error decoding response body: {}", e),
            endpoint: Some("/api/query_recent_trades".to_string()),
            retryable: false,
        })?;

        Ok(trades)
    }

    /// Get order book depth for a symbol
    pub async fn get_depth(&self, symbol: &str, limit: Option<u32>) -> Result<OrderBook> {
        let url = format!("{}/api/query_depth_book", self.base_url);
        let mut query = vec![("symbol", symbol.to_string())];

        if let Some(l) = limit {
            query.push(("limit", l.to_string()));
        }

        let response = self.client.get(&url).query(&query).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_depth_book".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let mut data = response.json::<OrderBook>().await?;
        // Sort bids descending by price
        data.bids.sort_by(|a, b| {
            let price_a: f64 = a[0].parse().unwrap_or(0.0);
            let price_b: f64 = b[0].parse().unwrap_or(0.0);
            price_b
                .partial_cmp(&price_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Sort asks ascending by price
        data.asks.sort_by(|a, b| {
            let price_a: f64 = a[0].parse().unwrap_or(0.0);
            let price_b: f64 = b[0].parse().unwrap_or(0.0);
            price_a
                .partial_cmp(&price_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(data)
    }

    /// Get kline data for a symbol
    pub async fn get_kline(
        &self,
        symbol: &str,
        resolution: &str,
        from: i64,
        to: i64,
    ) -> Result<Vec<Kline>> {
        let url = format!("{}/api/kline/history", self.base_url);
        let query: Vec<(&str, String)> = vec![
            ("symbol", symbol.to_string()),
            ("resolution", resolution.to_string()),
            ("from", from.to_string()),
            ("to", to.to_string()),
        ];

        let response = self.client.get(&url).query(&query).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/kline/history".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let response_wrapper = response.json::<crate::models::KlineResponse>().await?;

        if response_wrapper.s != "ok" {
            return Err(Error::Api {
                code: 500,
                message: format!("Kline API returned status: {}", response_wrapper.s),
                endpoint: Some("/api/kline/history".to_string()),
                retryable: false,
            });
        }

        let data = response_wrapper.to_klines();
        Ok(data)
    }

    /// Get funding rate history for a symbol
    pub async fn get_funding_rate(
        &self,
        symbol: &str,
        start_time: i64,
        end_time: i64,
    ) -> Result<Vec<FundingRate>> {
        let url = format!("{}/api/query_funding_rates", self.base_url);
        let query: Vec<(&str, String)> = vec![
            ("symbol", symbol.to_string()),
            ("start_time", (start_time * 1000).to_string()),
            ("end_time", (end_time * 1000).to_string()),
        ];

        let response = self.client.get(&url).query(&query).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_funding_rates".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let data = response.json::<Vec<FundingRate>>().await?;
        Ok(data)
    }

    /// Get block trades from the block trade API
    pub async fn get_block_trades(
        &self,
        symbol: Option<&str>,
        limit: u32,
        is_completed: Option<bool>,
    ) -> Result<Vec<crate::models::BlockTrade>> {
        let url = format!("{}/v1/block", self.base_url);

        let mut body = serde_json::json!({ "limit": limit });
        if let Some(s) = symbol {
            body["symbol"] = serde_json::json!(s);
        }
        if let Some(completed) = is_completed {
            body["isCompleted"] = serde_json::json!(completed);
        }

        let body_str = body.to_string();
        let headers = self.build_auth_headers(Some(&body_str)).await?;

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .body(body_str)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/v1/block".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let result: serde_json::Value = response.json().await?;

        // API returns { "data": [...] } wrapper
        let trades = result["data"]
            .as_array()
            .ok_or_else(|| Error::Api {
                code: 0,
                message: "Expected 'data' array in block trade response".to_string(),
                endpoint: Some("/v1/block".to_string()),
                retryable: false,
            })?
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<serde_json::Result<Vec<crate::models::BlockTrade>>>()
            .map_err(|e| Error::Api {
                code: 0,
                message: format!("error decoding block trade: {}", e),
                endpoint: Some("/v1/block".to_string()),
                retryable: false,
            })?;

        Ok(trades)
    }

    /// Health check - returns true if API is available
    pub async fn health_check(&self) -> Result<bool> {
        // Use query_symbol_info as health check since /api/health doesn't exist
        let url = format!("{}/api/query_symbol_info", self.base_url);
        let response = self.client.get(&url).send().await?;
        Ok(response.status().is_success())
    }
}

impl Default for StandXClient {
    fn default() -> Self {
        Self::new().expect("Failed to create default client")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    #[test]
    fn session_id_builder_is_stable() {
        let client = StandXClient::with_base_url("https://example.invalid".to_string())
            .unwrap()
            .with_session_id("maker-session");
        assert_eq!(client.session_id(), Some("maker-session"));
    }

    #[tokio::test]
    async fn test_get_symbol_info() {
        let mut server = Server::new_async().await;
        let _m = server.mock("GET", "/api/query_symbol_info")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[{"symbol":"BTC-USD","base_asset":"BTC","quote_asset":"DUSD","base_decimals":9,"price_tick_decimals":2,"qty_tick_decimals":4,"min_order_qty":"0.0001","def_leverage":"10","max_leverage":"40","maker_fee":"0.0001","taker_fee":"0.0004","status":"trading"}]"#)
            .create();

        let client = StandXClient::with_base_url(server.url()).unwrap();
        let symbols = client.get_symbol_info().await.unwrap();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].symbol, "BTC-USD");
    }

    #[tokio::test]
    async fn test_get_symbol_market() {
        let mut server = Server::new_async().await;
        let _m = server.mock("GET", "/api/query_symbol_market")
            .match_query(mockito::Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"symbol":"BTC-USD","mark_price":"68000.00","index_price":"68001.50","last_price":"68000.00","volume_24h":"1234567.89","high_price_24h":"69000.00","low_price_24h":"67000.00","funding_rate":"0.0001","next_funding_time":"2026-02-24T16:00:00Z"}"#)
            .create();

        let client = StandXClient::with_base_url(server.url()).unwrap();
        let market = client.get_symbol_market("BTC-USD").await.unwrap();

        assert_eq!(market.symbol, "BTC-USD");
        assert_eq!(market.mark_price, "68000.00");
    }

    #[tokio::test]
    async fn test_health_check() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/api/query_symbol_info")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[]"#)
            .create();

        let client = StandXClient::with_base_url(server.url()).unwrap();
        let healthy = client.health_check().await.unwrap();

        assert!(healthy);
    }

    #[tokio::test]
    async fn test_api_error() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/api/query_symbol_info")
            .with_status(400)
            .with_body("Invalid request")
            .create();

        let client = StandXClient::with_base_url(server.url()).unwrap();
        let result = client.get_symbol_info().await;

        assert!(matches!(result, Err(Error::Api { code: 400, .. })));
    }

    #[tokio::test]
    async fn test_api_error_401_unauthorized() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/api/query_symbol_info")
            .with_status(401)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":"Unauthorized"}"#)
            .create();

        let client = StandXClient::with_base_url(server.url()).unwrap();
        let result = client.get_symbol_info().await;

        assert!(matches!(result, Err(Error::Api { code: 401, .. })));
    }

    #[tokio::test]
    async fn test_api_error_500_server_error() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/api/query_symbol_info")
            .with_status(500)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":"Internal Server Error"}"#)
            .create();

        let client = StandXClient::with_base_url(server.url()).unwrap();
        let result = client.get_symbol_info().await;

        assert!(matches!(result, Err(Error::Api { code: 500, .. })));
    }
}
