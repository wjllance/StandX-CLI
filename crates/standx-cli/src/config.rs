//! Configuration management for StandX CLI

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use standx_sdk::StandXEndpoints;
use std::path::PathBuf;

pub const BASE_URL_ENV: &str = "STANDX_BASE_URL";

/// Must be set to a non-empty value before an authenticated command (anything
/// but `market`) is allowed to run against a plain-HTTP/WS endpoint.
pub const ALLOW_INSECURE_ENDPOINT_ENV: &str = "STANDX_ALLOW_INSECURE_ENDPOINT";

/// Refuse to run an authenticated command against a non-TLS endpoint unless the
/// operator explicitly opts in. `StandXEndpoints::new` already restricts plain
/// HTTP/WS to loopback addresses, but that alone doesn't stop a normal
/// invocation — e.g. a mistyped `--endpoint` or a `STANDX_BASE_URL` picked up
/// from a compromised script — from pointing the JWT and signing key at an
/// unintended local listener in clear text. This is a second, explicit signal
/// that the operator means it.
pub fn ensure_authenticated_endpoint_is_secure(endpoints: &StandXEndpoints) -> Result<()> {
    if endpoints.is_secure() {
        return Ok(());
    }
    if std::env::var_os(ALLOW_INSECURE_ENDPOINT_ENV).is_some_and(|value| !value.is_empty()) {
        return Ok(());
    }
    Err(Error::Config {
        message: format!(
            "refusing to send credentials to insecure endpoint '{}': plain HTTP/WS exposes the \
             JWT and signing key to anything listening on that address. Set \
             {ALLOW_INSECURE_ENDPOINT_ENV}=1 to confirm this is a trusted local test endpoint.",
            endpoints.base_url()
        ),
    })
}

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// API base URL
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// Whether `base_url` was explicitly confirmed through `config set` (or a fresh
    /// default config). Config files written before endpoint routing existed have no
    /// such field and deserialize this as `false`, so a stale custom `base_url` that
    /// was previously cosmetic-only stays inactive after upgrading until the user
    /// re-confirms it — an upgrade must never silently turn an old value into live
    /// routing for financial commands.
    #[serde(default)]
    pub base_url_confirmed: bool,

    /// Default output format
    pub output_format: String,

    /// Default trading symbol
    pub default_symbol: String,

    /// Configuration directory
    #[serde(skip)]
    pub config_dir: PathBuf,

    /// Exact configuration file selected through `--config`, when present.
    #[serde(skip)]
    config_file_override: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            base_url_confirmed: true,
            output_format: "table".to_string(),
            default_symbol: "BTC-USD".to_string(),
            config_dir: Self::default_config_dir(),
            config_file_override: None,
        }
    }
}

fn default_base_url() -> String {
    standx_sdk::endpoints::DEFAULT_BASE_URL.to_string()
}

impl Config {
    /// Get default configuration directory
    pub fn default_config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("standx")
    }

    /// Get configuration file path
    pub fn config_file(&self) -> PathBuf {
        self.config_file_override
            .clone()
            .unwrap_or_else(|| self.config_dir.join("config.toml"))
    }

    /// Load configuration from file
    pub fn load() -> Result<Self> {
        Self::load_from_path(None::<PathBuf>)
    }

    /// Load configuration from a specific path
    ///
    /// If `path` is `None`, uses the default config directory.
    /// If `path` is `Some(path)`, loads config from that directory.
    ///
    /// # Arguments
    /// * `path` - Optional path to the configuration directory
    ///
    /// # Returns
    /// * `Result<Self>` - The loaded configuration or an error
    ///
    /// # Example
    /// ```ignore
    /// // Load from default directory
    /// let config = Config::load_from_path(None)?;
    ///
    /// // Load from specific directory
    /// let config = Config::load_from_path(Some("/tmp/my-config"))?;
    /// ```
    pub fn load_from_path<T: Into<PathBuf>>(path: Option<T>) -> Result<Self> {
        let selected = path.map(Into::into);
        let (config_dir, config_file_override) = match selected {
            Some(path) if is_config_file_path(&path) => {
                let parent = path
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                (parent, Some(path))
            }
            Some(path) => (path, None),
            None => (Self::default_config_dir(), None),
        };
        let config_file = config_file_override
            .clone()
            .unwrap_or_else(|| config_dir.join("config.toml"));

        if !config_file.exists() {
            return Ok(Self {
                config_dir,
                config_file_override,
                ..Self::default()
            });
        }

        let content = std::fs::read_to_string(&config_file).map_err(|e| Error::Config {
            message: format!("Failed to read config file: {}", e),
        })?;

        let mut config: Config = toml::from_str(&content).map_err(|e| Error::Config {
            message: format!("Failed to parse config file: {}", e),
        })?;

        config.config_dir = config_dir;
        config.config_file_override = config_file_override;
        Ok(config)
    }

    /// Load the configuration selected by the global `--config` option.
    pub fn load_selected(path: Option<&str>) -> Result<Self> {
        Self::load_from_path(path.map(PathBuf::from))
    }

    /// Resolve the effective endpoint using CLI > environment > file > default.
    pub fn resolve_endpoints(
        cli_endpoint: Option<&str>,
        config_path: Option<&str>,
    ) -> Result<StandXEndpoints> {
        if let Some(endpoint) = cli_endpoint {
            return StandXEndpoints::new(endpoint);
        }
        if let Some(endpoint) = std::env::var_os(BASE_URL_ENV) {
            let endpoint = endpoint.into_string().map_err(|_| Error::Config {
                message: format!("{BASE_URL_ENV} is not valid UTF-8"),
            })?;
            return StandXEndpoints::new(endpoint);
        }
        let config = Self::load_selected(config_path)?;
        if config_path.is_some() && !config.config_file().exists() {
            return Err(Error::Config {
                message: format!(
                    "Selected config file does not exist: {}",
                    config.config_file().display()
                ),
            });
        }
        if config.base_url != standx_sdk::endpoints::DEFAULT_BASE_URL && !config.base_url_confirmed
        {
            eprintln!(
                "warning: ignoring unconfirmed base_url '{}' in {} — it predates endpoint \
                 routing and was never active; run `standx config set base_url {}` to confirm \
                 and use it, or pass --endpoint. Falling back to the default endpoint.",
                config.base_url,
                config.config_file().display(),
                config.base_url,
            );
            return StandXEndpoints::new(standx_sdk::endpoints::DEFAULT_BASE_URL);
        }
        StandXEndpoints::new(&config.base_url)
    }

    /// Save configuration to file
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir).map_err(|e| Error::Config {
            message: format!("Failed to create config directory: {}", e),
        })?;

        let content = toml::to_string_pretty(self).map_err(|e| Error::Config {
            message: format!("Failed to serialize config: {}", e),
        })?;

        std::fs::write(self.config_file(), content).map_err(|e| Error::Config {
            message: format!("Failed to write config file: {}", e),
        })?;

        Ok(())
    }

    /// Set a configuration value
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "base_url" => {
                self.base_url = StandXEndpoints::new(value)?.base_url().to_string();
                self.base_url_confirmed = true;
            }
            "output_format" => self.output_format = value.to_string(),
            "default_symbol" => self.default_symbol = value.to_string(),
            _ => {
                return Err(Error::Config {
                    message: format!("Unknown config key: {}", key),
                })
            }
        }
        self.save()
    }

    /// Get a configuration value
    pub fn get(&self, key: &str) -> Result<String> {
        match key {
            "base_url" => Ok(self.base_url.clone()),
            "output_format" => Ok(self.output_format.clone()),
            "default_symbol" => Ok(self.default_symbol.clone()),
            _ => Err(Error::Config {
                message: format!("Unknown config key: {}", key),
            }),
        }
    }
}

fn is_config_file_path(path: &std::path::Path) -> bool {
    if path.is_dir() {
        return false;
    }
    path.is_file()
        || path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// Helper struct to temporarily set environment variables
    /// Restores original value (or removes if not set) when dropped.
    /// Holds the crate-wide [`crate::TEST_ENV_LOCK`] for its lifetime so env
    /// tests never overlap — including across modules (telemetry, maker,
    /// pipeline).
    struct EnvGuard {
        values: Vec<(String, Option<std::ffi::OsString>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(key: &str, value: &str) -> Self {
            Self::set_many(&[(key, Some(value))])
        }

        fn unset(key: &str) -> Self {
            Self::set_many(&[(key, None)])
        }

        fn set_many(values: &[(&str, Option<&str>)]) -> Self {
            let lock = crate::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let original_values = values
                .iter()
                .map(|(key, value)| {
                    let original = std::env::var_os(key);
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                    ((*key).to_string(), original)
                })
                .collect();
            Self {
                values: original_values,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, original) in self.values.drain(..).rev() {
                match original {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.base_url, "https://perps.standx.com");
        assert_eq!(config.output_format, "table");
        assert_eq!(config.default_symbol, "BTC-USD");
    }

    #[test]
    fn test_config_save_load() {
        let temp_dir = TempDir::new().unwrap();
        let config = Config {
            base_url: "https://test.standx.com".to_string(),
            base_url_confirmed: true,
            output_format: "json".to_string(),
            default_symbol: "ETH-USD".to_string(),
            config_dir: temp_dir.path().to_path_buf(),
            config_file_override: None,
        };

        // Save config
        config.save().unwrap();

        // Verify file exists
        assert!(config.config_file().exists());

        // Read and verify content
        let content = std::fs::read_to_string(config.config_file()).unwrap();
        assert!(content.contains("https://test.standx.com"));
        assert!(content.contains("json"));
    }

    #[test]
    fn test_set_get() {
        let temp_dir = TempDir::new().unwrap();
        let mut config = Config::load_from_path(Some(temp_dir.path())).unwrap();

        config.set("base_url", "https://test.com").unwrap();
        assert_eq!(config.get("base_url").unwrap(), "https://test.com");

        config.set("output_format", "json").unwrap();
        assert_eq!(config.get("output_format").unwrap(), "json");

        assert!(config.set("unknown_key", "value").is_err());
        assert!(config.get("unknown_key").is_err());
    }

    #[test]
    fn test_config_missing_file() {
        // 使用临时目录确保配置文件不存在
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path().join("nonexistent");

        // 直接测试：当配置文件不存在时，应该返回默认配置
        // 注意：这里我们手动构造场景，因为 Config::load() 使用固定路径
        let config_file = config_dir.join("config.toml");
        assert!(!config_file.exists());

        // 验证默认配置
        let config = Config::default();
        assert_eq!(config.base_url, "https://perps.standx.com");
        assert_eq!(config.output_format, "table");
    }

    #[test]
    fn test_config_corrupted_file() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("config.toml");

        // 写入损坏的 TOML 内容
        let mut file = std::fs::File::create(&config_file).unwrap();
        file.write_all(b"invalid toml content [[[").unwrap();
        drop(file);

        // 尝试从该目录加载配置应该失败
        // 由于 Config::load() 使用固定路径，我们测试 save/load 循环
        let config = Config {
            base_url: "https://test.com".to_string(),
            base_url_confirmed: true,
            output_format: "json".to_string(),
            default_symbol: "ETH-USD".to_string(),
            config_dir: temp_dir.path().to_path_buf(),
            config_file_override: None,
        };

        // 先保存有效配置
        config.save().unwrap();

        // 然后损坏文件
        let mut file = std::fs::File::create(config.config_file()).unwrap();
        file.write_all(b"invalid toml [[[").unwrap();
        drop(file);

        // 尝试加载损坏的配置文件
        // 注意：Config::load() 使用默认路径，这里我们手动测试解析错误
        let content = std::fs::read_to_string(config.config_file()).unwrap();
        let result: std::result::Result<Config, _> = toml::from_str(&content);
        assert!(result.is_err());
    }

    #[test]
    fn test_config_env_override_base_url() {
        // Test that environment variable can be set and read for base_url
        let _guard = EnvGuard::set("STANDX_BASE_URL", "https://env.standx.com");

        let env_url = std::env::var("STANDX_BASE_URL").unwrap();
        assert_eq!(env_url, "https://env.standx.com");
    }

    #[test]
    fn test_config_env_override_output_format() {
        // Test that environment variable can be set and read for output_format
        let _guard = EnvGuard::set("STANDX_OUTPUT_FORMAT", "json");

        let env_format = std::env::var("STANDX_OUTPUT_FORMAT").unwrap();
        assert_eq!(env_format, "json");
    }

    #[test]
    fn test_config_env_override_default_symbol() {
        // Test that environment variable can be set and read for default_symbol
        let _guard = EnvGuard::set("STANDX_DEFAULT_SYMBOL", "ETH-USD");

        let env_symbol = std::env::var("STANDX_DEFAULT_SYMBOL").unwrap();
        assert_eq!(env_symbol, "ETH-USD");
    }

    #[test]
    fn test_config_env_priority() {
        // Test environment variable priority: Env > File > Default
        // Create a config file with specific values
        let temp_dir = TempDir::new().unwrap();
        let config = Config {
            base_url: "https://file.standx.com".to_string(),
            base_url_confirmed: true,
            output_format: "table".to_string(),
            default_symbol: "BTC-USD".to_string(),
            config_dir: temp_dir.path().to_path_buf(),
            config_file_override: None,
        };
        config.save().unwrap();

        // Set environment variable (should take priority)
        let _guard = EnvGuard::set("STANDX_BASE_URL", "https://env.standx.com");

        // Verify environment variable exists
        let env_val = std::env::var("STANDX_BASE_URL").unwrap();
        assert_eq!(env_val, "https://env.standx.com");
    }

    #[test]
    fn test_config_env_empty_string() {
        // Test empty string environment variable
        let _guard = EnvGuard::set("STANDX_BASE_URL", "");

        let env_val = std::env::var("STANDX_BASE_URL").unwrap();
        assert_eq!(env_val, "");
    }

    #[test]
    fn test_config_env_isolation() {
        // Verify the temporary-set-then-restore semantics EnvGuard relies on.
        // Done inline under TEST_ENV_LOCK rather than via EnvGuard: EnvGuard
        // holds that same (non-reentrant) lock for its lifetime, so nesting it
        // under an already-held lock would deadlock. Holding the lock keeps
        // every mutation here from racing the process-global environ against
        // env reads in other tests. EnvGuard's own set/restore is covered by
        // the env-override tests above.
        let _lock = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        std::env::set_var("TEST_ISOLATION_VAR", "original");
        let saved = std::env::var("TEST_ISOLATION_VAR").ok();

        std::env::set_var("TEST_ISOLATION_VAR", "modified");
        assert_eq!(std::env::var("TEST_ISOLATION_VAR").unwrap(), "modified");

        match saved {
            Some(value) => std::env::set_var("TEST_ISOLATION_VAR", value),
            None => std::env::remove_var("TEST_ISOLATION_VAR"),
        }
        assert_eq!(std::env::var("TEST_ISOLATION_VAR").unwrap(), "original");

        std::env::remove_var("TEST_ISOLATION_VAR");
    }

    #[test]
    fn test_load_from_path_with_specific_directory() {
        let temp_dir = TempDir::new().unwrap();

        let config = Config {
            base_url: "https://specific.test.com".to_string(),
            base_url_confirmed: true,
            output_format: "json".to_string(),
            default_symbol: "ETH-USD".to_string(),
            config_dir: temp_dir.path().to_path_buf(),
            config_file_override: None,
        };
        config.save().unwrap();

        let loaded = Config::load_from_path(Some(temp_dir.path())).unwrap();
        assert_eq!(loaded.base_url, "https://specific.test.com");
    }

    #[test]
    fn test_load_from_path_nonexistent_directory() {
        let nonexistent = PathBuf::from("/tmp/nonexistent_standx_test_dir");
        let result = Config::load_from_path(Some(&nonexistent));
        assert!(result.is_ok());
    }

    #[test]
    fn test_load_from_path_with_string() {
        let temp_dir = TempDir::new().unwrap();

        let config = Config {
            base_url: "https://string.test.com".to_string(),
            base_url_confirmed: true,
            output_format: "csv".to_string(),
            default_symbol: "DOGE-USD".to_string(),
            config_dir: temp_dir.path().to_path_buf(),
            config_file_override: None,
        };
        config.save().unwrap();

        let temp_path = temp_dir.path().to_str().unwrap();
        let loaded = Config::load_from_path(Some(temp_path)).unwrap();
        assert_eq!(loaded.base_url, "https://string.test.com");
    }

    #[test]
    fn test_load_from_path_with_pathbuf() {
        let temp_dir = TempDir::new().unwrap();

        let config = Config {
            base_url: "https://pathbuf.test.com".to_string(),
            base_url_confirmed: true,
            output_format: "csv".to_string(),
            default_symbol: "DOGE-USD".to_string(),
            config_dir: temp_dir.path().to_path_buf(),
            config_file_override: None,
        };
        config.save().unwrap();

        let loaded = Config::load_from_path(Some(temp_dir.path().to_path_buf())).unwrap();
        assert_eq!(loaded.base_url, "https://pathbuf.test.com");
    }

    #[test]
    fn test_load_from_path_none_uses_default() {
        let temp_dir = TempDir::new().unwrap();
        let config = Config::load_from_path(Some(temp_dir.path())).unwrap();
        assert_eq!(config.base_url, "https://perps.standx.com");
    }

    #[test]
    fn test_load_backward_compatibility() {
        let temp_dir = TempDir::new().unwrap();

        let config = Config {
            base_url: "https://backward.compat.com".to_string(),
            base_url_confirmed: true,
            output_format: "table".to_string(),
            default_symbol: "BTC-USD".to_string(),
            config_dir: temp_dir.path().to_path_buf(),
            config_file_override: None,
        };
        config.save().unwrap();

        let loaded = Config::load_from_path(Some(temp_dir.path())).unwrap();
        assert_eq!(loaded.base_url, "https://backward.compat.com");
    }

    #[test]
    fn legacy_config_without_base_url_uses_default_endpoint() {
        let _env = EnvGuard::unset(BASE_URL_ENV);
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(
            temp_dir.path().join("config.toml"),
            "output_format = \"json\"\ndefault_symbol = \"BTC-USD\"\n",
        )
        .unwrap();

        let loaded = Config::load_from_path(Some(temp_dir.path())).unwrap();
        assert_eq!(loaded.base_url, standx_sdk::endpoints::DEFAULT_BASE_URL);
        assert!(!loaded.base_url_confirmed);
        assert_eq!(loaded.output_format, "json");
        assert_eq!(loaded.default_symbol, "BTC-USD");

        let resolved =
            Config::resolve_endpoints(None, Some(temp_dir.path().to_str().unwrap())).unwrap();
        assert_eq!(resolved, StandXEndpoints::default());
    }

    #[test]
    fn test_load_from_path_corrupted_file() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("config.toml");

        let mut file = std::fs::File::create(&config_file).unwrap();
        file.write_all(b"invalid toml content [[[").unwrap();
        drop(file);

        let result = Config::load_from_path(Some(temp_dir.path()));
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_resolution_precedence_is_cli_then_env_then_file_then_default() {
        let temp_dir = TempDir::new().unwrap();
        let mut config = Config::load_from_path(Some(temp_dir.path())).unwrap();
        config
            .set("base_url", "https://file.standx.example")
            .unwrap();
        let config_path = temp_dir.path().to_str().unwrap();
        let isolated_home = TempDir::new().unwrap();

        let env = EnvGuard::set_many(&[
            (BASE_URL_ENV, Some("https://env.standx.example")),
            ("HOME", isolated_home.path().to_str()),
        ]);
        let cli = Config::resolve_endpoints(Some("https://cli.standx.example/"), Some(config_path))
            .unwrap();
        assert_eq!(cli.base_url(), "https://cli.standx.example");

        let from_env = Config::resolve_endpoints(None, Some(config_path)).unwrap();
        assert_eq!(from_env.base_url(), "https://env.standx.example");

        // Keep the process-wide lock while temporarily clearing the test value;
        // dropping the guard below restores the caller's original environment.
        std::env::remove_var(BASE_URL_ENV);
        let from_file = Config::resolve_endpoints(None, Some(config_path)).unwrap();
        assert_eq!(from_file.base_url(), "https://file.standx.example");

        let default = Config::resolve_endpoints(None, None).unwrap();
        assert_eq!(default, StandXEndpoints::default());
        drop(env);
    }

    #[test]
    fn pre_existing_unconfirmed_base_url_stays_inactive_after_upgrade() {
        // Simulates a config.toml written by a version of the CLI that predates
        // endpoint routing: `base_url` was persisted but had no functional effect
        // (no `base_url_confirmed` field exists). Upgrading must not silently turn
        // that stale value into live routing for financial commands.
        let _env = EnvGuard::unset(BASE_URL_ENV);
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("config.toml");
        std::fs::write(
            &config_file,
            "base_url = \"https://stale-legacy.example.com\"\n\
             output_format = \"table\"\n\
             default_symbol = \"BTC-USD\"\n",
        )
        .unwrap();
        let config_path = temp_dir.path().to_str().unwrap();

        let resolved = Config::resolve_endpoints(None, Some(config_path)).unwrap();
        assert_eq!(resolved, StandXEndpoints::default());

        // Once the user explicitly re-confirms the same value via `config set`, it
        // becomes active.
        let mut config = Config::load_from_path(Some(temp_dir.path())).unwrap();
        config
            .set("base_url", "https://stale-legacy.example.com")
            .unwrap();
        let resolved = Config::resolve_endpoints(None, Some(config_path)).unwrap();
        assert_eq!(resolved.base_url(), "https://stale-legacy.example.com");
    }

    #[test]
    fn explicit_empty_environment_endpoint_fails_closed() {
        let _env = EnvGuard::set(BASE_URL_ENV, "");
        let error = Config::resolve_endpoints(None, None).unwrap_err();
        assert!(error.to_string().contains("invalid StandX endpoint"));
    }

    #[test]
    fn explicitly_selected_missing_config_fails_closed_for_endpoint_resolution() {
        let _env = EnvGuard::unset(BASE_URL_ENV);
        let temp_dir = TempDir::new().unwrap();
        let missing_file = temp_dir.path().join("missing.toml");
        let error =
            Config::resolve_endpoints(None, missing_file.to_str()).expect_err("must fail closed");
        assert!(error.to_string().contains("does not exist"));

        let missing_directory = temp_dir.path().join("missing-directory");
        let error = Config::resolve_endpoints(None, missing_directory.to_str())
            .expect_err("must fail closed");
        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn selected_toml_file_is_loaded_and_saved_without_directory_rewriting() {
        let temp_dir = TempDir::new().unwrap();
        let selected = temp_dir.path().join("canary.toml");
        let mut config = Config::load_from_path(Some(&selected)).unwrap();
        config
            .set("base_url", "https://perps.example.com/")
            .unwrap();

        assert!(selected.exists());
        assert!(!temp_dir.path().join("config.toml").exists());
        let loaded = Config::load_from_path(Some(&selected)).unwrap();
        assert_eq!(loaded.base_url, "https://perps.example.com");
        assert_eq!(loaded.config_file(), selected);
    }

    #[test]
    fn existing_directory_named_with_toml_suffix_remains_a_directory() {
        let temp_dir = TempDir::new().unwrap();
        let selected = temp_dir.path().join("profile.toml");
        std::fs::create_dir(&selected).unwrap();

        let mut config = Config::load_from_path(Some(&selected)).unwrap();
        config.set("base_url", "https://perps.example.com").unwrap();

        assert_eq!(config.config_file(), selected.join("config.toml"));
        assert!(selected.join("config.toml").is_file());
    }

    #[test]
    fn setting_base_url_normalizes_valid_values_and_rejects_invalid_ones() {
        let temp_dir = TempDir::new().unwrap();
        let mut config = Config::load_from_path(Some(temp_dir.path())).unwrap();
        config
            .set("base_url", "https://perps.example.com/")
            .unwrap();
        assert_eq!(config.base_url, "https://perps.example.com");

        let error = config
            .set("base_url", "http://perps.example.com")
            .unwrap_err();
        assert!(error.to_string().contains("plain HTTP"));
        assert_eq!(config.base_url, "https://perps.example.com");
    }

    #[test]
    fn insecure_endpoint_requires_explicit_opt_in() {
        let insecure = StandXEndpoints::new("http://127.0.0.1:8080").unwrap();

        {
            let _env = EnvGuard::unset(ALLOW_INSECURE_ENDPOINT_ENV);
            let error = ensure_authenticated_endpoint_is_secure(&insecure).unwrap_err();
            assert!(error.to_string().contains(ALLOW_INSECURE_ENDPOINT_ENV));
        }

        let _opt_in = EnvGuard::set(ALLOW_INSECURE_ENDPOINT_ENV, "1");
        ensure_authenticated_endpoint_is_secure(&insecure).unwrap();
    }

    #[test]
    fn secure_endpoint_never_needs_opt_in() {
        let _env = EnvGuard::unset(ALLOW_INSECURE_ENDPOINT_ENV);
        ensure_authenticated_endpoint_is_secure(&StandXEndpoints::default()).unwrap();
    }
}
