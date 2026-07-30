//! Telemetry module for collecting usage statistics
//!
//! This module provides local telemetry collection for standx-cli.
//! Data is stored locally and never sent to remote servers.
//!
//! To disable telemetry, set environment variable: STANDX_TELEMETRY=0

use serde::{Deserialize, Serialize};
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

/// Telemetry configuration
const TELEMETRY_ENV_VAR: &str = "STANDX_TELEMETRY";
const TELEMETRY_DISABLED: &str = "0";

/// Telemetry event types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    /// Command started execution
    CommandStarted,
    /// Command completed execution
    CommandCompleted,
    /// Error occurred
    Error,
    /// API call made
    ApiCall,
}

/// Telemetry event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryEvent {
    /// ISO 8601 timestamp
    pub timestamp: String,
    /// Event type
    pub event_type: EventType,
    /// CLI version
    pub version: String,
    /// Event properties
    pub properties: serde_json::Value,
}

/// Telemetry collector
pub struct Telemetry {
    enabled: bool,
    log_path: PathBuf,
    session_id: String,
    command_start: Option<Instant>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl Telemetry {
    /// Create new telemetry instance
    pub fn new() -> Self {
        let enabled = std::env::var(TELEMETRY_ENV_VAR).unwrap_or_default() != TELEMETRY_DISABLED;
        let log_path = Self::get_log_path();
        let session_id = uuid::Uuid::new_v4().to_string();

        // Ensure directory exists
        if let Some(parent) = log_path.parent() {
            let _ = create_dir_all(parent);
        }

        let telemetry = Self {
            enabled,
            log_path,
            session_id: session_id.clone(),
            command_start: None,
        };

        // Track session start
        telemetry.track_event(
            EventType::CommandStarted,
            serde_json::json!({
                "session_id": &session_id,
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            }),
        );

        telemetry
    }

    /// Get telemetry log file path
    fn get_log_path() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("standx")
            .join("telemetry.log")
    }

    /// Check if telemetry is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Track a generic event
    pub fn track_event(&self, event_type: EventType, properties: serde_json::Value) {
        if !self.enabled {
            return;
        }

        let event = TelemetryEvent {
            timestamp: chrono::Utc::now().to_rfc3339(),
            event_type,
            version: env!("CARGO_PKG_VERSION").to_string(),
            properties,
        };

        if let Ok(json) = serde_json::to_string(&event) {
            let _ = self.append_to_log(&json);
        }
    }

    /// Track command execution start
    pub fn track_command_start(&mut self, command: &str, args: &[String]) {
        self.command_start = Some(Instant::now());
        let sanitized_args = sanitize_args(args);

        self.track_event(
            EventType::CommandStarted,
            serde_json::json!({
                "session_id": &self.session_id,
                "command": command,
                "args": sanitized_args,
                "arg_count": args.len(),
            }),
        );
    }

    /// Track command execution completion
    pub fn track_command_complete(&self, command: &str, success: bool, error: Option<&str>) {
        let duration_ms = self
            .command_start
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);

        self.track_event(
            EventType::CommandCompleted,
            serde_json::json!({
                "session_id": &self.session_id,
                "command": command,
                "success": success,
                "duration_ms": duration_ms,
                "error": error.map(|e| e.to_string()),
            }),
        );
    }

    /// Track API call
    pub fn track_api_call(&self, endpoint: &str, method: &str, status_code: u16, duration_ms: u64) {
        self.track_event(
            EventType::ApiCall,
            serde_json::json!({
                "session_id": &self.session_id,
                "endpoint": endpoint,
                "method": method,
                "status_code": status_code,
                "duration_ms": duration_ms,
            }),
        );
    }

    /// Track error
    pub fn track_error(&self, error_type: &str, message: &str, command: Option<&str>) {
        self.track_event(
            EventType::Error,
            serde_json::json!({
                "session_id": &self.session_id,
                "error_type": error_type,
                "message": message,
                "command": command,
            }),
        );
    }

    /// Append line to log file
    fn append_to_log(&self, line: &str) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;

        writeln!(file, "{}", line)?;
        Ok(())
    }

    /// Get telemetry log file path
    pub fn log_path(&self) -> &PathBuf {
        &self.log_path
    }

    /// Read all telemetry events (for debugging/analysis)
    pub fn read_events(&self) -> Vec<TelemetryEvent> {
        if !self.log_path.exists() {
            return vec![];
        }

        std::fs::read_to_string(&self.log_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

/// Redact secrets from CLI argv while preserving the flag names needed for
/// aggregate usage analysis. Handles both `--flag value` and `--flag=value`.
fn sanitize_args(args: &[String]) -> Vec<String> {
    const SENSITIVE_FLAGS: &[&str] = &[
        "--private-key",
        "--token",
        "--alert-webhook",
        "--endpoint",
        "-p",
    ];

    let mut sanitized = Vec::with_capacity(args.len());
    let mut redact_next = false;

    for arg in args {
        if redact_next {
            sanitized.push("***".to_string());
            redact_next = false;
            continue;
        }

        if SENSITIVE_FLAGS.contains(&arg.as_str()) {
            sanitized.push(arg.clone());
            redact_next = true;
            continue;
        }

        if let Some(flag) = SENSITIVE_FLAGS
            .iter()
            .find(|flag| arg.starts_with(&format!("{}=", flag)))
        {
            sanitized.push(format!("{}=***", flag));
            continue;
        }

        sanitized.push(arg.clone());
    }

    sanitized
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        // Ensure any pending events are written
        if self.enabled && self.command_start.is_some() {
            self.track_command_complete("unknown", false, Some("interrupted"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_telemetry_disabled() {
        // Serialize with every other env-touching test across the crate; a bare
        // set_var here otherwise races the process-global environ against reads
        // like Credentials::load() elsewhere. See crate::TEST_ENV_LOCK.
        let _env = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Save original value
        let original = env::var(TELEMETRY_ENV_VAR).ok();

        env::set_var(TELEMETRY_ENV_VAR, TELEMETRY_DISABLED);
        let telemetry = Telemetry::new();
        assert!(!telemetry.is_enabled());

        // Restore original
        match original {
            Some(v) => env::set_var(TELEMETRY_ENV_VAR, v),
            None => env::remove_var(TELEMETRY_ENV_VAR),
        }
    }

    #[test]
    fn test_telemetry_enabled_by_default() {
        let _env = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        env::remove_var(TELEMETRY_ENV_VAR);
        let _telemetry = Telemetry::new();
        // Note: This might fail if user has env var set
        // In real tests, use a temp directory
    }

    #[test]
    fn sanitize_args_redacts_separate_sensitive_values() {
        let args = vec![
            "standx".to_string(),
            "maker".to_string(),
            "run".to_string(),
            "BTC-USD".to_string(),
            "--alert-webhook".to_string(),
            "https://api.telegram.org/bot-secret/sendMessage".to_string(),
            "--token".to_string(),
            "jwt-secret".to_string(),
            "--endpoint".to_string(),
            "https://alice:endpoint-secret@example.com".to_string(),
        ];

        let sanitized = sanitize_args(&args);
        assert_eq!(sanitized[4], "--alert-webhook");
        assert_eq!(sanitized[5], "***");
        assert_eq!(sanitized[6], "--token");
        assert_eq!(sanitized[7], "***");
        assert_eq!(sanitized[8], "--endpoint");
        assert_eq!(sanitized[9], "***");
        assert!(!sanitized.join(" ").contains("secret"));
    }

    #[test]
    fn sanitize_args_redacts_equals_form_and_preserves_other_args() {
        let args = vec![
            "standx".to_string(),
            "auth".to_string(),
            "login".to_string(),
            "--private-key=private-secret".to_string(),
            "--alert-webhook=https://hooks.example/secret".to_string(),
            "--endpoint=https://alice:endpoint-secret@example.com".to_string(),
            "--output=json".to_string(),
        ];

        assert_eq!(
            sanitize_args(&args),
            vec![
                "standx",
                "auth",
                "login",
                "--private-key=***",
                "--alert-webhook=***",
                "--endpoint=***",
                "--output=json",
            ]
        );
    }
}
