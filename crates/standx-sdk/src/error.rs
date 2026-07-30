use serde::Serialize;
use thiserror::Error;

/// Structured error type for machine parsing
#[derive(Error, Debug, Serialize)]
#[serde(tag = "error_type")]
pub enum Error {
    #[error("HTTP request failed: {code} - {message}")]
    #[serde(rename = "HTTP_ERROR")]
    Http {
        code: u16,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        retryable: Option<bool>,
    },

    #[error("WebSocket error")]
    #[serde(rename = "WEBSOCKET_ERROR")]
    WebSocket { message: String },

    #[error("JSON parse error")]
    #[serde(rename = "JSON_ERROR")]
    Json { message: String },

    #[error("API error: {code} - {message}")]
    #[serde(rename = "API_ERROR")]
    Api {
        code: u16,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        retryable: bool,
    },

    #[error("Authentication required")]
    #[serde(rename = "AUTH_REQUIRED")]
    AuthRequired { message: String, resolution: String },

    #[error("Invalid credentials")]
    #[serde(rename = "INVALID_CREDENTIALS")]
    InvalidCredentials { message: String },

    #[error("Token expired")]
    #[serde(rename = "TOKEN_EXPIRED")]
    TokenExpired { message: String, resolution: String },

    #[error("Invalid symbol: {symbol}")]
    #[serde(rename = "INVALID_SYMBOL")]
    InvalidSymbol {
        symbol: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        available_symbols: Option<Vec<String>>,
    },

    #[error("Rate limit exceeded")]
    #[serde(rename = "RATE_LIMIT")]
    RateLimitExceeded {
        message: String,
        retry_after: Option<u64>,
    },

    #[error("Configuration error: {message}")]
    #[serde(rename = "CONFIG_ERROR")]
    Config { message: String },

    #[error("IO error")]
    #[serde(rename = "IO_ERROR")]
    Io { message: String },

    #[error("Validation error")]
    #[serde(rename = "VALIDATION_ERROR")]
    Validation { field: String, message: String },

    #[error("Dry run")]
    #[serde(rename = "DRY_RUN")]
    DryRun {
        command: String,
        description: String,
    },

    #[error("{0}")]
    #[serde(rename = "UNKNOWN_ERROR")]
    Unknown(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Convert to JSON format for agent consumption
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": self,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        })
    }

    /// Check if error is retryable
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::Http {
                retryable: Some(true),
                ..
            } | Error::Api {
                retryable: true,
                ..
            } | Error::RateLimitExceeded { .. }
                | Error::WebSocket { .. }
        )
    }

    /// Get suggested action for the error
    pub fn suggested_action(&self) -> Option<String> {
        match self {
            Error::AuthRequired { .. } => {
                Some("Run 'standx auth login' to authenticate".to_string())
            }
            Error::TokenExpired { .. } => {
                Some("Re-authenticate with 'standx auth login'".to_string())
            }
            Error::RateLimitExceeded { retry_after, .. } => {
                retry_after.map(|secs| format!("Wait {} seconds before retrying", secs))
            }
            Error::InvalidSymbol { .. } => {
                Some("Run 'standx market symbols' to see available symbols".to_string())
            }
            _ => None,
        }
    }
}

// Conversions from external errors
impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        let code = e.status().map(|s| s.as_u16()).unwrap_or(0);
        let retryable = code == 0 || code >= 500;
        Error::Http {
            code,
            message: e.to_string(),
            retryable: Some(retryable),
        }
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        Error::WebSocket {
            message: e.to_string(),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json {
            message: e.to_string(),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io {
            message: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_api_errors_have_stable_display_and_json_shapes() {
        let http = Error::Http {
            code: 401,
            message: "Unauthorized".to_string(),
            retryable: Some(false),
        };
        assert_eq!(http.to_string(), "HTTP request failed: 401 - Unauthorized");
        let http_json = serde_json::to_value(&http).unwrap();
        assert_eq!(http_json["error_type"], "HTTP_ERROR");
        assert_eq!(http_json["code"], 401);
        assert_eq!(http_json["retryable"], false);

        let api = Error::Api {
            code: 500,
            message: "Internal Server Error".to_string(),
            endpoint: Some("/api/query_positions".to_string()),
            retryable: true,
        };
        assert_eq!(api.to_string(), "API error: 500 - Internal Server Error");
        let api_json = serde_json::to_value(&api).unwrap();
        assert_eq!(api_json["error_type"], "API_ERROR");
        assert_eq!(api_json["endpoint"], "/api/query_positions");
        assert_eq!(api_json["retryable"], true);
    }

    #[test]
    fn retryability_is_derived_from_the_typed_error() {
        let retryable = [
            Error::Http {
                code: 503,
                message: "unavailable".to_string(),
                retryable: Some(true),
            },
            Error::Api {
                code: 500,
                message: "server error".to_string(),
                endpoint: None,
                retryable: true,
            },
            Error::RateLimitExceeded {
                message: "slow down".to_string(),
                retry_after: Some(2),
            },
            Error::WebSocket {
                message: "closed".to_string(),
            },
        ];
        assert!(retryable.iter().all(Error::is_retryable));

        let terminal = Error::Http {
            code: 400,
            message: "bad request".to_string(),
            retryable: Some(false),
        };
        assert!(!terminal.is_retryable());
    }

    #[test]
    fn authentication_error_exposes_a_recovery_action() {
        let error = Error::AuthRequired {
            message: "Token expired".to_string(),
            resolution: "Run 'standx auth login'".to_string(),
        };

        assert_eq!(error.to_string(), "Authentication required");
        assert_eq!(
            error.suggested_action().as_deref(),
            Some("Run 'standx auth login' to authenticate")
        );
    }
}
