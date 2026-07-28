//! Account API client methods

use crate::auth::{Credentials, StandXSigner};
use crate::client::StandXClient;
use crate::error::{Error, Result};
use crate::models::{Balance, Order, Position};
use chrono::{SecondsFormat, TimeZone, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;

/// API response wrapper for list endpoints
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ApiListResponse<T> {
    code: i32,
    message: String,
    #[serde(rename = "page_size")]
    _page_size: Option<i32>,
    result: Vec<T>,
}

fn funding_history_query(
    symbol: Option<&str>,
    from: Option<i64>,
    last_id: Option<i64>,
    limit: Option<u32>,
) -> Result<Vec<(&'static str, String)>> {
    let mut query = Vec::new();
    if let Some(symbol) = symbol {
        query.push(("symbol", symbol.to_string()));
    }
    if let Some(from) = from {
        let start = Utc
            .timestamp_opt(from, 0)
            .single()
            .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
            .ok_or_else(|| Error::Validation {
                field: "from".to_string(),
                message: format!("invalid Unix timestamp: {from}"),
            })?;
        query.push(("start", start));
    }
    if let Some(last_id) = last_id {
        query.push(("last_id", last_id.to_string()));
    }
    if let Some(limit) = limit {
        query.push(("limit", limit.to_string()));
    }
    Ok(query)
}

fn trade_history_query(
    symbol: &str,
    from: i64,
    to: i64,
    limit: Option<u32>,
) -> Result<Vec<(&'static str, String)>> {
    if from > to {
        return Err(Error::Validation {
            field: "from".to_string(),
            message: "trade history start must not be after end".to_string(),
        });
    }
    let format = |field: &str, value: i64| {
        Utc.timestamp_opt(value, 0)
            .single()
            .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
            .ok_or_else(|| Error::Validation {
                field: field.to_string(),
                message: format!("invalid Unix timestamp: {value}"),
            })
    };
    let mut query = vec![
        ("symbol", symbol.to_string()),
        ("start", format("from", from)?),
        ("end", format("to", to)?),
    ];
    if let Some(limit) = limit {
        query.push(("limit", limit.to_string()));
    }
    Ok(query)
}

/// Account-related API methods
impl StandXClient {
    /// Load credentials and create authenticated headers
    fn auth_headers(&self) -> Result<HeaderMap> {
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

        Ok(headers)
    }

    /// Sign request with Ed25519
    #[allow(dead_code)]
    fn sign_request(&self, payload: &str) -> Result<(StandXSigner, crate::auth::RequestSignature)> {
        let creds = Credentials::load()?;
        let signer = StandXSigner::from_base58(&creds.private_key).map_err(|_| {
            Error::InvalidCredentials {
                message: "Invalid private key format".to_string(),
            }
        })?;

        let signature = signer.sign_request_now(payload);
        Ok((signer, signature))
    }

    /// Get account balances
    pub async fn get_balance(&self) -> Result<Balance> {
        let url = format!("{}/api/query_balance", self.base_url);
        let headers = self.auth_headers()?;

        let response = self.client.get(&url).headers(headers).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_balance".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let data = response.json::<Balance>().await?;
        Ok(data)
    }

    /// Get positions
    pub async fn get_positions(&self, symbol: Option<&str>) -> Result<Vec<Position>> {
        let url = format!("{}/api/query_positions", self.base_url);
        let headers = self.auth_headers()?;

        let mut query = vec![];
        if let Some(s) = symbol {
            query.push(("symbol", s));
        }

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .query(&query)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_positions".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let data = response.json::<Vec<Position>>().await?;
        Ok(data)
    }

    /// Get open orders
    pub async fn get_open_orders(&self, symbol: Option<&str>) -> Result<Vec<Order>> {
        let url = format!("{}/api/query_open_orders", self.base_url);
        let headers = self.auth_headers()?;

        let mut query = vec![];
        if let Some(s) = symbol {
            query.push(("symbol", s));
        }

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .query(&query)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_open_orders".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let wrapper = response.json::<ApiListResponse<Order>>().await?;
        Ok(wrapper.result)
    }

    /// Get one order by its exchange ID.
    ///
    /// This is intentionally separate from order-history polling: an order
    /// that fills between polling cycles can be absent from `/api/query_orders`
    /// while still being retrievable by its stable exchange order ID.
    pub async fn get_order(&self, order_id: u64) -> Result<Order> {
        let url = format!("{}/api/query_order", self.base_url);
        let headers = self.auth_headers()?;

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .query(&[("order_id", order_id)])
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_order".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        Ok(response.json::<Order>().await?)
    }

    /// Get order history
    pub async fn get_order_history(
        &self,
        symbol: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<Order>> {
        let url = format!("{}/api/query_orders", self.base_url);
        let headers = self.auth_headers()?;

        let mut query: Vec<(&str, String)> = vec![];
        // status=filled means filled orders (history)
        query.push(("status", "filled".to_string()));
        if let Some(s) = symbol {
            query.push(("symbol", s.to_string()));
        }
        if let Some(l) = limit {
            query.push(("limit", l.to_string()));
        }

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .query(&query)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_orders".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let wrapper = response.json::<ApiListResponse<Order>>().await?;
        Ok(wrapper.result)
    }

    /// Get user trade history
    pub async fn get_user_trades(
        &self,
        symbol: &str,
        from: i64,
        to: i64,
        limit: Option<u32>,
    ) -> Result<Vec<crate::models::Trade>> {
        let url = format!("{}/api/query_trades", self.base_url);
        let headers = self.auth_headers()?;

        let query = trade_history_query(symbol, from, to, limit)?;

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .query(&query)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_trades".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let wrapper = response
            .json::<ApiListResponse<crate::models::Trade>>()
            .await?;
        Ok(wrapper.result)
    }

    /// Get the authenticated funding payment/receipt history.
    ///
    /// `from` bounds the window (session start, so a long-lived account's older
    /// funding never enters a fresh session's attribution) and `last_id` is the
    /// incremental cursor — rows arrive newest-first, so callers that want
    /// chronological order must sort. There is deliberately no `end`: callers
    /// always want everything up to now.
    ///
    /// Unlike [`Self::get_user_trades`], this endpoint returns a bare JSON
    /// array rather than the `{code, message, result}` envelope.
    pub async fn get_funding_history(
        &self,
        symbol: Option<&str>,
        from: Option<i64>,
        last_id: Option<i64>,
        limit: Option<u32>,
    ) -> Result<Vec<crate::models::FundingHistoryEntry>> {
        let url = format!("{}/api/query_funding_history", self.base_url);
        let headers = self.auth_headers()?;
        let query = funding_history_query(symbol, from, last_id, limit)?;

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .query(&query)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_funding_history".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        Ok(response
            .json::<Vec<crate::models::FundingHistoryEntry>>()
            .await?)
    }

    /// Get position config (includes leverage)
    pub async fn get_position_config(&self, symbol: &str) -> Result<crate::models::PositionConfig> {
        let url = format!("{}/api/query_position_config", self.base_url);
        let headers = self.build_auth_headers(None).await?;

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .query(&[("symbol", symbol)])
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api {
                code: status.as_u16(),
                message: text,
                endpoint: Some("/api/query_position_config".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        let data = response.json::<crate::models::PositionConfig>().await?;
        Ok(data)
    }

    /// Change leverage for a symbol
    pub async fn change_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        let url = format!("{}/api/change_leverage", self.base_url);

        let body = serde_json::json!({
            "symbol": symbol,
            "leverage": leverage,
        });
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
                endpoint: Some("/api/change_leverage".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        Ok(())
    }

    /// Change margin mode for a symbol
    pub async fn change_margin_mode(&self, symbol: &str, mode: &str) -> Result<()> {
        let url = format!("{}/api/change_margin_mode", self.base_url);

        let body = serde_json::json!({
            "symbol": symbol,
            "margin_mode": mode,
        });
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
                endpoint: Some("/api/change_margin_mode".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        Ok(())
    }

    /// Transfer margin for a symbol
    pub async fn transfer_margin(&self, symbol: &str, amount: &str, direction: &str) -> Result<()> {
        let url = format!("{}/api/transfer_margin", self.base_url);

        // Convert amount to negative for withdraw
        let amount_val: f64 = amount.parse().map_err(|_| Error::Api {
            code: 400,
            message: format!("Invalid amount: {}", amount),
            endpoint: None,
            retryable: false,
        })?;

        let final_amount = if direction == "withdraw" {
            -amount_val.abs()
        } else {
            amount_val.abs()
        };

        let body = serde_json::json!({
            "symbol": symbol,
            "amount_in": final_amount.to_string(),
        });
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
                endpoint: Some("/api/transfer_margin".to_string()),
                retryable: status.as_u16() >= 500,
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod trade_history_tests {
    use super::*;

    #[test]
    fn trade_history_query_uses_iso_start_and_end() {
        let query = trade_history_query("XAG-USD", 1_783_696_499, 1_783_696_560, Some(500))
            .expect("valid query");
        assert_eq!(query[0], ("symbol", "XAG-USD".to_string()));
        assert_eq!(query[1], ("start", "2026-07-10T15:14:59Z".to_string()));
        assert_eq!(query[2], ("end", "2026-07-10T15:16:00Z".to_string()));
        assert_eq!(query[3], ("limit", "500".to_string()));
        assert!(query.iter().all(|(key, _)| *key != "from" && *key != "to"));
    }

    /// The funding endpoint's params are all optional, so an empty query must
    /// stay empty rather than sending stray defaults the venue would reject.
    #[test]
    fn funding_history_query_is_all_optional() {
        assert!(funding_history_query(None, None, None, None)
            .expect("valid query")
            .is_empty());

        let query =
            funding_history_query("HYPE-USD".into(), Some(1_783_696_499), Some(42), Some(200))
                .expect("valid query");
        assert_eq!(query[0], ("symbol", "HYPE-USD".to_string()));
        assert_eq!(query[1], ("start", "2026-07-10T15:14:59Z".to_string()));
        assert_eq!(query[2], ("last_id", "42".to_string()));
        assert_eq!(query[3], ("limit", "200".to_string()));
        // The venue takes `start`, not `from`, and has no `end` here.
        assert!(query
            .iter()
            .all(|(key, _)| *key != "from" && *key != "end" && *key != "to"));
    }

    #[test]
    fn funding_history_query_rejects_impossible_timestamp() {
        let error = funding_history_query(None, Some(i64::MAX), None, None).unwrap_err();
        assert!(matches!(error, Error::Validation { field, .. } if field == "from"));
    }

    /// Pins the two verified deviations from the published schema: a bare array
    /// body (no `{code,message,result}` envelope) and a missing `transact_time`.
    #[test]
    fn funding_history_entry_parses_live_shape() {
        let body = r#"[{"asset":"DUSD","created_at":"2026-07-27T03:00:00.254907Z","id":530060896,"qty":"0.000074457","symbol":"HYPE-USD","txn_type":"funding_fee","updated_at":"2026-07-27T03:00:00.254907Z","user":"solana_x"},
                       {"asset":"DUSD","created_at":"2026-07-27T01:00:00.179068Z","id":529761428,"qty":"-0.000119095","symbol":"HYPE-USD","txn_type":"funding_fee","updated_at":"2026-07-27T01:00:00.179068Z","user":"solana_x"}]"#;
        let rows: Vec<crate::models::FundingHistoryEntry> =
            serde_json::from_str(body).expect("live funding shape parses");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, 530060896);
        assert_eq!(rows[0].txn_type, "funding_fee");
        assert_eq!(rows[0].asset, "DUSD");
        assert!(rows[0].transact_time.is_none());
        // Signed cashflow: received then paid.
        assert_eq!(rows[0].qty.parse::<f64>().unwrap(), 0.000074457);
        assert!(rows[1].qty.parse::<f64>().unwrap() < 0.0);
    }

    #[test]
    fn trade_history_query_rejects_reversed_range() {
        let error = trade_history_query("XAG-USD", 20, 10, None).unwrap_err();
        assert!(matches!(error, Error::Validation { field, .. } if field == "from"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    #[tokio::test]
    async fn test_get_balance_unauthorized() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/api/query_balance")
            .with_status(401)
            .with_body("Unauthorized")
            .create();

        let client = StandXClient::with_base_url(server.url()).unwrap();
        // Should fail because no credentials
        let result = client.get_balance().await;
        assert!(result.is_err());
    }
}
