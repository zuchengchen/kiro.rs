//! 用上游 meteringEvent.credits 修正 CacheMeter 的缓存拆分。
//!
//! CacheMeter 在请求开始时按断点估算 creation/read，新断点要到首个上游 chunk
//! 才对其他请求可见。同一 session 并发发出的同前缀请求因此被本地记成整段
//! cache_creation，而 Kiro 上游已经按命中扣费（828K prompt：未命中约 10.3
//! credits，命中约 2.9）。流结束时拿到真实 credits 后，若它明确落在「命中」
//! 一侧，就把 creation 改记为 read。
//!
//! 只做向下修正；本地判命中、上游未命中的反向误差不在这里调高。

/// 某模型的 Kiro credit 单价（每 token）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct CreditRates {
    pub read: f64,
    pub uncached: f64,
    pub output: f64,
}

/// claude-opus-5.5：2026-09-23..09-30 traces.db 13,169 条 simulated 样本截尾最小二乘，
/// 中位相对误差 4.1%。重新校准：`tools/credit_rate_fit.py`。
const OPUS_5_5: CreditRates = CreditRates {
    read: 3.370e-6,
    uncached: 12.069e-6,
    output: 527.0e-6,
};

/// 只收录读取与未缓存单价能明显区分的模型。claude-sonnet-5 实测二者几乎相同
/// （4.09 vs 4.16 /M），无法从 credits 判断命中，故不收录。
pub(crate) fn rates_for(model: &str) -> Option<CreditRates> {
    let mapped = super::converter::map_model(model)
        .unwrap_or_else(|| model.to_string())
        .to_ascii_lowercase();
    match mapped.as_str() {
        "claude-opus-5.5" | "claude-opus-5-5" => Some(OPUS_5_5),
        _ => None,
    }
}

/// 低于此量的 creation 不值得修正，且信号太弱。
const MIN_CREATION_TOKENS: i32 = 20_000;
/// 命中与未命中两种假设的 credit 差额至少占未命中预测的比例；输出占大头时跳过。
/// 与 `tools/credit_rate_fit.py` 保持同步。
const MIN_GAP_RATIO: f64 = 0.5;
/// 实际 credits 在「未命中 → 命中」区间内的位置 t，达到此值才判为命中。
/// 与 `tools/credit_rate_fit.py` 保持同步。
const HIT_THRESHOLD: f64 = 0.75;
/// t 明显超过 1 说明单价已漂移，不修正。与 `tools/credit_rate_fit.py` 保持同步。
const MAX_T: f64 = 1.25;

/// 纯函数：`split` 为 `(uncached_input, cache_creation, cache_read)`。
/// 判为上游命中时返回整段 creation 转入 read 后的拆分，否则 `None`。
pub(crate) fn reconcile_with_rates(
    rates: CreditRates,
    split: (i32, i32, i32),
    output_tokens: i32,
    credits: f64,
) -> Option<(i32, i32, i32)> {
    let (input, creation, read) = split;
    if !credits.is_finite() || credits <= 0.0 || creation < MIN_CREATION_TOKENS {
        return None;
    }
    let (input_f, creation_f, read_f) = (input.max(0) as f64, creation as f64, read.max(0) as f64);
    let output_f = output_tokens.max(0) as f64;

    let as_miss =
        rates.uncached * (input_f + creation_f) + rates.read * read_f + rates.output * output_f;
    let gap = (rates.uncached - rates.read) * creation_f;
    if gap <= 0.0 || gap < MIN_GAP_RATIO * as_miss {
        return None;
    }
    let t = (as_miss - credits) / gap;
    if !(HIT_THRESHOLD..=MAX_T).contains(&t) {
        return None;
    }
    Some((input, 0, read.saturating_add(creation)))
}

/// 按模型查单价后修正；模型未收录时返回 `None`。
pub(crate) fn reconcile_split(
    model: &str,
    split: (i32, i32, i32),
    output_tokens: i32,
    credits: f64,
) -> Option<(i32, i32, i32)> {
    reconcile_with_rates(rates_for(model)?, split, output_tokens, credits)
}

/// `KIRO_RS_CREDIT_CACHE_RECONCILE`：`1`/`on`/`true`/`yes`/`enabled` 开启，默认关闭。
/// 必须等 Sub2API 支持 `usage_final` 后再开，否则 delta 的 creation=0 会被忽略导致重复计费。
pub(crate) fn enabled_from_env() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KIRO_RS_CREDIT_CACHE_RECONCILE")
            .map(|raw| {
                matches!(
                    raw.trim().to_ascii_lowercase().as_str(),
                    "1" | "on" | "true" | "yes" | "enabled"
                )
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // 真实样本：traces.db 0ddb52e6（与 6976af5f 并发，上游命中）
    #[test]
    fn concurrent_request_billed_as_hit_is_moved_to_read() {
        assert_eq!(
            reconcile_with_rates(OPUS_5_5, (0, 828_840, 0), 387, 2.913),
            Some((0, 0, 828_840))
        );
    }

    // 6976af5f：真正的冷启动，上游按未命中扣费
    #[test]
    fn real_cold_write_is_unchanged() {
        assert_eq!(
            reconcile_with_rates(OPUS_5_5, (0, 828_767, 0), 335, 10.313),
            None
        );
    }

    // c8c99903：并发但上游也未命中（首字前对方仍在 prefill）
    #[test]
    fn concurrent_request_that_upstream_also_missed_is_unchanged() {
        assert_eq!(
            reconcile_with_rates(OPUS_5_5, (0, 820_795, 7_538), 129, 10.219),
            None
        );
    }

    // 039591d1：输出 2.3 万 token 占大头，信号不可靠
    #[test]
    fn output_dominated_request_is_unchanged() {
        assert_eq!(
            reconcile_with_rates(OPUS_5_5, (32_711, 610_471, 0), 23_325, 17.142),
            None
        );
    }

    #[test]
    fn missing_credits_small_writes_and_implausible_ratio_are_unchanged() {
        assert_eq!(
            reconcile_with_rates(OPUS_5_5, (0, 828_840, 0), 387, 0.0),
            None
        );
        assert_eq!(
            reconcile_with_rates(OPUS_5_5, (0, 828_840, 0), 387, f64::NAN),
            None
        );
        assert_eq!(
            reconcile_with_rates(OPUS_5_5, (0, 19_999, 0), 10, 0.01),
            None
        );
        // credits 远低于「全部命中」预测（t > 1.25）：单价漂移，不修正
        assert_eq!(
            reconcile_with_rates(OPUS_5_5, (0, 828_840, 0), 387, 0.01),
            None
        );
    }

    #[test]
    fn reconciled_split_conserves_prompt_total() {
        let (i, c, r) = reconcile_with_rates(OPUS_5_5, (1_234, 700_000, 5_000), 100, 2.5).unwrap();
        assert_eq!(i + c + r, 1_234 + 700_000 + 5_000);
        assert_eq!((i, c), (1_234, 0));
    }

    #[test]
    fn only_calibrated_models_are_reconciled() {
        assert!(rates_for("claude-opus-5.5").is_some());
        assert!(rates_for("claude-opus-5-5").is_some());
        assert!(rates_for("claude-sonnet-5").is_none());
        assert!(rates_for("claude-opus-5-thinking").is_none());
        assert_eq!(
            reconcile_split("claude-sonnet-5", (0, 422_055, 0), 8, 1.0),
            None
        );
    }
}
