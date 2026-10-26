//! amazonq-cli 兜底桶的模型策略（本仓库定制，独立文件以降低上游合并冲突）。
//!
//! 实测（2026-10-26，0.9.2.4）：IDE 额度用尽（runtime / codewhisperer 402
//! `MONTHLY_REQUEST_COUNT`）的 FREE / STUDENT 账号，amazonq-cli 仍能服务
//! `claude-haiku-4.5`；同一批账号对 `claude-sonnet-4.6`、`claude-opus-5`、5.5 系列
//! 回 400 `Improperly formed request.`。所以：
//!
//! - IDE 用尽但 CLI 未尽的账号对 CLI 可服务的模型仍可作为最后一档兜底；
//! - CLI 是否接受某模型按「静态已知拒绝 + 运行时学习到的拒绝」判定，
//!   学习结果带 TTL，上游放开模型后会自动重新尝试。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::kiro::endpoint::cli::AMAZON_Q_CLI_ENDPOINT_NAME;

/// 运行时学习到的 CLI 模型拒绝保留多久。到期后会再试一次，最多浪费一跳。
pub const LEARNED_REJECTION_TTL: Duration = Duration::from_secs(6 * 3600);

fn normalize(model: &str) -> String {
    model.to_ascii_lowercase().replace('_', "-")
}

/// amazonq-cli 已知不接受的实验模型 ID，hop 过去只会 400 Improperly formed request。
pub fn cli_rejects_model_statically(model: Option<&str>) -> bool {
    let Some(model) = model else {
        return false;
    };
    let normalized = normalize(model);
    normalized.contains("haiku-5.5")
        || normalized.contains("haiku-5-5")
        || normalized.contains("opus-5.5")
        || normalized.contains("opus-5-5")
}

/// IDE 桶已 402 额度尽之后，amazonq-cli hop 回的 400 属于同一额度事件
/// （CLI 不接受该模型，`Improperly formed request`），应换号而不是当客户端坏请求。
/// 仅限 amazonq-cli：其它桶的 400 和超长内容 400 仍是真实请求错误。
pub fn is_cli_400_after_quota(endpoint_name: &str, status: u16, body: &str) -> bool {
    endpoint_name == AMAZON_Q_CLI_ENDPOINT_NAME
        && status == 400
        && !body.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD")
}

/// 运行时学习到的「amazonq-cli 不接受该模型」集合（模型级，跨账号共享）。
#[derive(Default)]
pub struct CliModelRejections {
    rejected_at: Mutex<HashMap<String, Instant>>,
}

impl CliModelRejections {
    pub fn record(&self, model: &str) {
        self.rejected_at.lock().insert(normalize(model), Instant::now());
    }

    pub fn is_rejected(&self, model: &str, now: Instant) -> bool {
        let mut map = self.rejected_at.lock();
        let key = normalize(model);
        match map.get(&key) {
            Some(at) if now.duration_since(*at) < LEARNED_REJECTION_TTL => true,
            Some(_) => {
                map.remove(&key);
                false
            }
            None => false,
        }
    }

    /// 该模型是否值得 hop 到 amazonq-cli。`None`（tool / 无模型请求）不走 CLI 兜底。
    pub fn cli_may_serve(&self, model: Option<&str>) -> bool {
        match model {
            Some(model) => {
                !cli_rejects_model_statically(Some(model))
                    && !self.is_rejected(model, Instant::now())
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_rejection_covers_5_5_spellings() {
        for model in [
            "claude-opus-5.5",
            "claude-opus-5-5",
            "claude_haiku_5_5",
            "CLAUDE-HAIKU-5.5",
        ] {
            assert!(cli_rejects_model_statically(Some(model)), "{model}");
        }
        assert!(!cli_rejects_model_statically(Some("claude-haiku-4.5")));
        assert!(!cli_rejects_model_statically(None));
    }

    #[test]
    fn learned_rejection_blocks_until_ttl() {
        let rejections = CliModelRejections::default();
        assert!(rejections.cli_may_serve(Some("claude-sonnet-4.6")));
        rejections.record("Claude-Sonnet-4.6");
        assert!(!rejections.cli_may_serve(Some("claude-sonnet-4.6")));
        assert!(rejections.cli_may_serve(Some("claude-haiku-4.5")));
        let later = Instant::now() + LEARNED_REJECTION_TTL + Duration::from_secs(1);
        assert!(!rejections.is_rejected("claude-sonnet-4.6", later));
        assert!(rejections.cli_may_serve(Some("claude-sonnet-4.6")));
    }

    #[test]
    fn no_model_never_uses_cli_fallback() {
        assert!(!CliModelRejections::default().cli_may_serve(None));
    }

    #[test]
    fn cli_400_after_quota_is_limited_to_amazonq_cli() {
        let improper = r#"{"message":"Improperly formed request."}"#;
        assert!(is_cli_400_after_quota("amazonq-cli", 400, improper));
        assert!(!is_cli_400_after_quota("runtime", 400, improper));
        assert!(!is_cli_400_after_quota("codewhisperer", 400, improper));
        assert!(!is_cli_400_after_quota(
            "amazonq-cli",
            400,
            r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#
        ));
        assert!(!is_cli_400_after_quota("amazonq-cli", 402, improper));
    }
}
