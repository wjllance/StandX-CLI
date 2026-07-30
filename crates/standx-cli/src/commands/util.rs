use anyhow::Result;
use standx_sdk::error::Error as StandxError;
use std::future::Future;
use std::time::Duration;
use tokio::signal;
use tokio::sync::watch;

/// Parse time string to timestamp
/// Supports:
/// - Unix timestamp (e.g., "1704067200")
/// - ISO date (e.g., "2024-01-01")
/// - Relative time (e.g., "1h", "1d", "7d", "30m")
pub fn parse_time_string(time_str: &str, default_now: bool) -> anyhow::Result<i64> {
    // Try parsing as unix timestamp first
    if let Ok(timestamp) = time_str.parse::<i64>() {
        return Ok(timestamp);
    }

    // Try parsing as ISO date (YYYY-MM-DD)
    if let Ok(date) = chrono::NaiveDate::parse_from_str(time_str, "%Y-%m-%d") {
        let datetime = date.and_hms_opt(0, 0, 0).unwrap();
        return Ok(datetime.and_utc().timestamp());
    }

    // Try parsing as relative time
    let time_str = time_str.to_lowercase();
    let now = chrono::Utc::now().timestamp();

    if let Some(unit) = time_str.chars().last() {
        let value_end = time_str.len() - unit.len_utf8();
        let value = &time_str[..value_end];
        if !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && matches!(unit, 's' | 'm' | 'h' | 'd' | 'w')
        {
            let value: i64 = value.parse()?;
            let seconds = match unit {
                's' => value,
                'm' => value * 60,
                'h' => value * 3600,
                'd' => value * 86400,
                'w' => value * 604800,
                _ => unreachable!(),
            };

            if default_now {
                // For "to" time, we use now + offset (future) or just now
                return Ok(now + seconds);
            } else {
                // For "from" time, we use now - offset (past)
                return Ok(now - seconds);
            }
        }
    }

    Err(anyhow::anyhow!(
        "Invalid time format: {}. Use unix timestamp, YYYY-MM-DD, or relative like 1h, 1d, 7d",
        time_str
    ))
}

pub(super) fn is_auth_error(error: &StandxError) -> bool {
    matches!(
        error,
        StandxError::AuthRequired { .. }
            | StandxError::TokenExpired { .. }
            | StandxError::InvalidCredentials { .. }
            | StandxError::Api { code: 401, .. }
    )
}

pub(super) async fn run_watch_loop<F, Fut>(
    watch: Option<u64>,
    mut render_once: F,
    error_prefix: &str,
    mut update_rx: Option<watch::Receiver<u64>>,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    if let Some(interval_secs) = watch {
        loop {
            let render_result = tokio::select! {
                _ = signal::ctrl_c() => {
                    println!("\n👋 Stopping watch mode");
                    break;
                }
                result = render_once() => result,
            };

            match render_result {
                Ok(rendered) => {
                    // Clear only after new frame is ready, reducing flicker.
                    print!("\x1B[2J\x1B[1H");
                    print!("{}", rendered);
                }
                Err(e) => {
                    eprintln!("⚠️  {}: {}", error_prefix, e);
                }
            }

            tokio::select! {
                _ = signal::ctrl_c() => {
                    println!("\n👋 Stopping watch mode");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => {}
                ws_updated = async {
                    if let Some(rx) = update_rx.as_mut() {
                        rx.changed().await.is_ok()
                    } else {
                        std::future::pending::<bool>().await
                    }
                } => {
                    // If sender is dropped, disable event-triggered refresh and keep interval refresh.
                    if !ws_updated {
                        update_rx = None;
                    }
                }
            }
        }
        Ok(())
    } else {
        let rendered = render_once().await?;
        print!("{}", rendered);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_relative_time_hours() {
        let now = chrono::Utc::now().timestamp();
        let result = parse_time_string("1h", false).unwrap();
        assert!(result < now);
        assert!(result >= now - 3600);
    }

    #[test]
    fn test_parse_relative_time_days() {
        let now = chrono::Utc::now().timestamp();
        let result = parse_time_string("1d", false).unwrap();
        assert!(result < now);
        assert!(result >= now - 86400);
    }

    #[test]
    fn test_parse_iso_date() {
        let result = parse_time_string("2024-01-01", true).unwrap();
        let expected = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_parse_unix_timestamp() {
        let result = parse_time_string("1704067200", true).unwrap();
        assert_eq!(result, 1704067200);
    }

    #[test]
    fn test_parse_invalid_time() {
        assert!(parse_time_string("invalid", true).is_err());
        assert!(parse_time_string("", true).is_err());
        assert!(parse_time_string("1天", true).is_err());
    }

    #[test]
    fn test_parse_time_edge_cases() {
        let now = chrono::Utc::now().timestamp();

        // 测试 0 秒（边界值）
        let result = parse_time_string("0s", false).unwrap();
        assert!(result <= now && result >= now - 10); // 允许 10 秒误差

        // 测试大数字天数
        let result = parse_time_string("999d", false).unwrap();
        assert!(result < now);
        assert!(result < now - 86300000); // 999 天大约是这么多秒

        // 测试分钟
        let result = parse_time_string("30m", false).unwrap();
        assert!(result < now);
        assert!(result >= now - 1800);
    }

    #[test]
    fn relative_time_supports_seconds_weeks_and_uppercase_units() {
        let now = chrono::Utc::now().timestamp();

        let seconds_ago = parse_time_string("60s", false).unwrap();
        assert!((now - 61..=now - 59).contains(&seconds_ago));

        let week_ago = parse_time_string("1w", false).unwrap();
        assert!((now - 604_801..=now - 604_799).contains(&week_ago));

        let lowercase = parse_time_string("1d", false).unwrap();
        let uppercase = parse_time_string("1D", false).unwrap();
        assert!((lowercase - uppercase).abs() <= 1);
    }
}
