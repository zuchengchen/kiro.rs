//! Shared typed errors for Kiro upstream calls.

/// The upstream still returned HTTP 429 after any applicable failover/retry.
#[derive(Debug, Clone, thiserror::Error)]
#[error("upstream rate limited")]
pub struct UpstreamRateLimitError {
    retry_after: Option<String>,
}

impl UpstreamRateLimitError {
    pub(crate) fn new(retry_after: Option<String>) -> Self {
        Self {
            retry_after: retry_after.and_then(normalize_retry_after),
        }
    }

    pub(crate) fn from_headers(headers: &http::HeaderMap) -> Self {
        let retry_after = headers
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Self::new(retry_after)
    }

    pub fn retry_after(&self) -> Option<&str> {
        self.retry_after.as_deref()
    }

    /// Without an explicit upstream delay, a short local retry is still useful.
    pub(crate) fn should_retry_locally(&self) -> bool {
        self.retry_after.is_none()
    }
}

/// 所有候选账号都已用满管理员设置的周期积分上限。
///
/// 与上游限流区分开：这是本地策略拒绝，不是 Kiro 侧的 429。但对客户端而言表现应当一致
/// —— 429 + `Retry-After`，否则客户端会立即重试，把一次上限拒绝放大成持续请求风暴。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct CreditLimitReachedError {
    message: String,
    /// 距离最近一个账号的计费周期重置还有多少秒
    retry_after_secs: Option<u64>,
}

impl CreditLimitReachedError {
    pub(crate) fn new(message: String, retry_after_secs: Option<u64>) -> Self {
        Self {
            message,
            retry_after_secs,
        }
    }

    /// `Retry-After` 头的取值（秒）
    ///
    /// 未知重置时刻时回退到 1 小时：既不至于让客户端疯狂重试，也不会因为等太久而
    /// 错过周期已经翻转的时机（周期翻转后账号会立即恢复可用）。
    pub fn retry_after_secs(&self) -> u64 {
        const FALLBACK_RETRY_AFTER_SECS: u64 = 3600;
        // 上限也压到 1 小时：计费周期可能还剩好几天，直接把 Retry-After 设成
        // 那么长会让客户端在周期翻转后仍然长时间不回来。
        self.retry_after_secs
            .map(|s| s.clamp(1, FALLBACK_RETRY_AFTER_SECS))
            .unwrap_or(FALLBACK_RETRY_AFTER_SECS)
    }
}

fn normalize_retry_after(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if value.parse::<u64>().is_ok() || httpdate::parse_http_date(value).is_ok() {
        Some(value.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_delta_seconds_and_http_date() {
        let seconds = UpstreamRateLimitError::new(Some(" 1800 ".to_string()));
        assert_eq!(seconds.retry_after(), Some("1800"));
        assert!(!seconds.should_retry_locally());

        let date = "Sun, 12 Jul 2026 02:30:00 GMT";
        let http_date = UpstreamRateLimitError::new(Some(date.to_string()));
        assert_eq!(http_date.retry_after(), Some(date));
        assert!(!http_date.should_retry_locally());
    }

    #[test]
    fn rejects_invalid_retry_after() {
        let error = UpstreamRateLimitError::new(Some("not-a-retry-delay".to_string()));
        assert_eq!(error.retry_after(), None);
        assert!(error.should_retry_locally());
    }
}
