//! Claude 模型的固定缓存命中比例（运维 / 计费口径要求）。
//!
//! **这是人为固定值，不是测量结果。** Claude 模型对外上报的 usage 中，
//! `cache_read` 恒为总 prompt token 的 [`CLAUDE_CACHE_READ_RATIO`]（90%），与前缀
//! 是否真正命中、请求是否声明 `cache_control` 都无关；真实缓存覆盖量超过 90% 的
//! 部分记为 `cache_creation`，其余为未缓存 `input`，三者相加仍等于原始总量。
//!
//! 其他模型（gpt-5.6-*、glm、deepseek、qwen、minimax、`auto` 等）不经本模块改写，
//! 按上游正常计量：provider `tokenUsage` 优先，其次本地 `cache_control` 断点模拟，
//! 否则全部计入 input。
//!
//! 「真实覆盖量」无论来自上游 `metadataEvent.tokenUsage` 还是本地 CacheMeter 模拟
//! 都同样改写——哪天 Kiro 开始下发精确用量，Claude 的 90% 也不会被绕过。
//!
//! 只改写 input / creation / read 三项的拆分，不改总量；上游 credit 由
//! meteringEvent 下发，不受影响，计费本身仍准确。代价是 Claude 请求的
//! 「每千输入 credit」、缓存命中率等缓存效率指标失去参照价值。
//! `cacheMeteringEnabled` 开关只影响真实覆盖量（即 creation 部分），关掉后 Claude
//! 仍报 90% read。
//!
//! 挂载点（上游合并后需逐一复核，新增 usage 出口也要在这里补挂）：
//! - `StreamContext::resolved_usage`（流式、缓冲流式与流式结算）
//! - `handlers::execute_non_stream_request`（非流式，含 Responses 与 Codex 压缩）
//! - `WebSearchUsageSettlement::usage`（web_search 多轮聚合后统一改写一次）
//! - `RequestTracer::finalize` 落库时把来源标记为 [`FIXED_USAGE_SOURCE`]
//!
//! OpenAI / Responses 适配层都经由 `post_messages`，从已改写的 Anthropic usage 读取
//! `cache_read_input_tokens`，拆分本身无需单独处理；但各自的对外 usage 格式要把它带出去：
//! chat/completions 放在 `prompt_tokens_details.cached_tokens`，Responses 放在
//! `input_tokens_details.cached_tokens`。漏了这一步，内部记账是 90%，客户端看到的是 0%。

use crate::kiro::model::events::TokenUsage;

/// Claude 模型上报的固定缓存读取比例（`cache_read / 总 prompt token`）。
pub(crate) const CLAUDE_CACHE_READ_RATIO: f64 = 0.90;

/// trace 行 `usage_source` 的取值：token 三项已按固定比例改写。
pub(crate) const FIXED_USAGE_SOURCE: &str = "fixed";

/// 该模型是否适用固定比例：按实际发往上游的 backend model id 判断，而非客户端写法，
/// 这样自定义别名按其 backend_id 归属、`-thinking` 等后缀也不影响判定。
pub(crate) fn applies_to(model: &str) -> bool {
    super::converter::map_model(model)
        .is_some_and(|mapped| mapped.to_ascii_lowercase().starts_with("claude-"))
}

/// 把 `(input, creation, read)` 改写为固定比例口径，总量不变。
///
/// - `read` = 总量 × 90%（四舍五入），与真实命中量无关；
/// - 真实覆盖量（`creation + read`）超过 90% 的部分记为 `creation`，不足时抬到 90%；
/// - 剩余为 `input`。负值按 0 处理。
fn split(input: i32, creation: i32, read: i32) -> (i32, i32, i32) {
    let input = i64::from(input.max(0));
    let creation = i64::from(creation.max(0));
    let read = i64::from(read.max(0));
    let total = (input + creation + read).min(i64::from(i32::MAX));
    if total == 0 {
        return (0, 0, 0);
    }

    let fixed_read = ((total as f64) * CLAUDE_CACHE_READ_RATIO).round() as i64;
    let fixed_read = fixed_read.clamp(0, total);
    let covered = (creation + read).clamp(fixed_read, total);
    (
        (total - covered) as i32,
        (covered - fixed_read) as i32,
        fixed_read as i32,
    )
}

/// Claude 模型改写 `(input, creation, read)`；其他模型原样返回。
pub(crate) fn apply(model: &str, usage: (i32, i32, i32)) -> (i32, i32, i32) {
    if !applies_to(model) {
        return usage;
    }
    let (input, creation, read) = usage;
    split(input, creation, read)
}

/// [`apply`] 的 [`TokenUsage`] 版本（web_search 聚合、工具 JSON 错误路径使用）。
/// `output_tokens` 不变。
pub(crate) fn apply_token_usage(model: &str, usage: TokenUsage) -> TokenUsage {
    if !applies_to(model) {
        return usage;
    }
    let (input, creation, read) = split(
        usage.uncached_input_tokens,
        usage.cache_write_input_tokens,
        usage.cache_read_input_tokens,
    );
    TokenUsage {
        uncached_input_tokens: input,
        cache_write_input_tokens: creation,
        cache_read_input_tokens: read,
        ..usage
    }
}

/// trace 落库时的 `usage_source`：Claude 且有用量时标为 [`FIXED_USAGE_SOURCE`]；
/// 无用量（`None`，如错误早退）保持为空，不冒充改写过的数字。
pub(crate) fn trace_usage_source(
    model: &str,
    source: Option<&'static str>,
) -> Option<&'static str> {
    match source {
        Some(_) if applies_to(model) => Some(FIXED_USAGE_SOURCE),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::cache_metering::CacheUsage;
    use crate::anthropic::stream::StreamContext;
    use std::collections::{HashMap, HashSet};

    fn usage(input: i32, output: i32, write: i32, read: i32) -> TokenUsage {
        TokenUsage {
            uncached_input_tokens: input,
            output_tokens: output,
            cache_write_input_tokens: write,
            cache_read_input_tokens: read,
        }
    }

    #[test]
    fn only_claude_backends_use_the_fixed_ratio() {
        for model in [
            "claude-opus-5.5",
            "claude-opus-4-7",
            "claude-opus-4.8-thinking",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001-thinking",
        ] {
            assert!(applies_to(model), "{model} 应适用固定比例");
        }
        for model in [
            "gpt-5.6-luna",
            "gpt-5.6-sol",
            "glm-5",
            "deepseek-3.2",
            "qwen3-coder-next",
            "minimax-m2.5",
            "auto",
            "test-model",
        ] {
            assert!(!applies_to(model), "{model} 应按上游正常计量");
        }
    }

    #[test]
    fn read_is_pinned_at_ninety_percent_regardless_of_real_coverage() {
        // 真实覆盖为 0（无 cache_control / 计量关闭）：差额凭空补到 90%。
        assert_eq!(split(1000, 0, 0), (100, 0, 900));
        // 真实覆盖 50% 低于 90%：抬到 90%，creation 为 0。
        assert_eq!(split(500, 300, 200), (100, 0, 900));
        // 真实覆盖 98% 高于 90%：read 仍是 90%，超出的 8% 记为 creation。
        assert_eq!(split(20, 380, 600), (20, 80, 900));
        // 全量真实命中也不例外。
        assert_eq!(split(0, 0, 1000), (0, 100, 900));
    }

    #[test]
    fn split_keeps_total_and_is_idempotent() {
        for (input, creation, read) in [
            (1, 0, 0),
            (11, 0, 0),
            (7, 993, 0),
            (3, 4, 7),
            (40, 20, 20),
            (123_456, 7_890, 1_000_000),
        ] {
            let once = split(input, creation, read);
            assert_eq!(once.0 + once.1 + once.2, input + creation + read);
            assert_eq!(split(once.0, once.1, once.2), once, "二次改写不应再变化");
        }
        assert_eq!(split(0, 0, 0), (0, 0, 0));
        assert_eq!(split(-5, -1, 0), (0, 0, 0), "负值按 0 处理");
    }

    #[test]
    fn other_models_pass_through_untouched() {
        assert_eq!(apply("gpt-5.6-luna", (40, 20, 20)), (40, 20, 20));
        assert_eq!(apply("claude-opus-4-7", (40, 20, 20)), (8, 0, 72));

        let provider = usage(3, 11, 4, 7);
        assert_eq!(apply_token_usage("glm-5", provider), provider);
        assert_eq!(
            apply_token_usage("claude-opus-4-7", provider),
            usage(1, 11, 0, 13)
        );
    }

    #[test]
    fn trace_source_is_marked_fixed_only_for_claude_usage() {
        assert_eq!(
            trace_usage_source("claude-opus-5.5", Some("simulated")),
            Some(FIXED_USAGE_SOURCE)
        );
        assert_eq!(
            trace_usage_source("claude-opus-5.5", Some("provider")),
            Some(FIXED_USAGE_SOURCE)
        );
        assert_eq!(
            trace_usage_source("claude-opus-5.5", None),
            None,
            "错误早退无用量不标记"
        );
        assert_eq!(
            trace_usage_source("gpt-5.6-luna", Some("none")),
            Some("none")
        );
    }

    /// 挂载点回归：流式 usage 出口对 Claude 生效，本地回退与上游精确快照两条路径都覆盖。
    #[test]
    fn stream_context_reports_fixed_ratio_for_claude() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-7",
            100,
            false,
            HashMap::new(),
            HashSet::new(),
        );
        ctx.context_input_tokens = Some(80);
        ctx.cache_usage = CacheUsage {
            cache_read: 25,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };
        // 上游口径是 (40, 20, 20)；Claude 改写为 read = 80 × 90% = 72。
        assert_eq!(ctx.resolved_usage(), (8, 0, 72));

        // 上游精确快照同样改写：总量 14 → read 13，真实覆盖 11 低于它 → creation 0。
        ctx.provider_token_usage = Some(usage(3, 11, 4, 7));
        assert_eq!(ctx.resolved_usage(), (1, 0, 13));

        let mut gpt = StreamContext::new_with_thinking(
            "gpt-5.6-luna",
            100,
            false,
            HashMap::new(),
            HashSet::new(),
        );
        gpt.context_input_tokens = Some(80);
        gpt.cache_usage = ctx.cache_usage;
        assert_eq!(gpt.resolved_usage(), (40, 20, 20), "非 Claude 按上游口径");
    }

    /// 缓冲流式：message_start 与最终 usage 同为固定比例口径（二者都经 resolved_usage）。
    #[test]
    fn buffered_stream_reports_fixed_ratio_consistently_for_claude() {
        use crate::anthropic::stream::BufferedStreamContext;
        use crate::kiro::model::events::{Event, MetadataEvent};

        let mut ctx = BufferedStreamContext::new(
            "claude-opus-4-7",
            100,
            false,
            HashMap::new(),
            HashSet::new(),
        );
        ctx.process_and_buffer(&Event::Metadata(MetadataEvent {
            token_usage: Some(usage(3, 11, 4, 7)),
        }));
        let events = ctx.finish_and_get_all_events();

        assert_eq!(ctx.final_usage(), (1, 11, 0, 13, 0.0));
        let start_usage = &events
            .iter()
            .find(|event| event.event == "message_start")
            .unwrap()
            .data["message"]["usage"];
        assert_eq!(start_usage["input_tokens"], serde_json::json!(1));
        assert_eq!(
            start_usage["cache_creation_input_tokens"],
            serde_json::json!(0)
        );
        assert_eq!(
            start_usage["cache_read_input_tokens"],
            serde_json::json!(13)
        );
    }
}
