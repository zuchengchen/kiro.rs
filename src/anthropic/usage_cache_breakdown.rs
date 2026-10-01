//! Anthropic usage 里的 `cache_creation` TTL 拆分。
//!
//! 官方 Messages API 在 `cache_creation_input_tokens` 之外还带：
//!
//! ```json
//! "cache_creation": {
//!   "ephemeral_5m_input_tokens": 456,
//!   "ephemeral_1h_input_tokens": 123
//! }
//! ```
//!
//! Sub2API 靠这两个嵌套字段给使用记录打「1h」标记、并分别按 5m / 1h 写入价计费。
//! 缺了它们时，即使聚合 `cache_creation_input_tokens` > 0，1h 列也是 0，
//! 计费回退成全部按 5m 价。
//!
//! 拆分来自 CacheMeter 的断点 TTL（[`CacheUsage::split_creation_ttl`]）：落在 1h 断点
//! 之下的写入记 1h，其余记 5m。没有 1h 断点（默认情况）时全部记 5m。
//!
//! [`CacheUsage::split_creation_ttl`]: super::cache_metering::CacheUsage::split_creation_ttl

use serde_json::{json, Value};

pub(crate) fn cache_creation_object(creation: i32, creation_1h: i32) -> Value {
    let creation = creation.max(0);
    let creation_1h = creation_1h.clamp(0, creation);
    json!({
        "ephemeral_5m_input_tokens": creation - creation_1h,
        "ephemeral_1h_input_tokens": creation_1h,
    })
}

/// `creation_1h` 是 `cache_creation` 中按 1h TTL 写入的部分，其余按 5m。
pub(crate) fn usage_json(
    input_tokens: i32,
    output_tokens: i32,
    cache_creation: i32,
    cache_creation_1h: i32,
    cache_read: i32,
) -> Value {
    let creation = cache_creation.max(0);
    json!({
        "input_tokens": input_tokens.max(0),
        "output_tokens": output_tokens.max(0),
        "cache_creation_input_tokens": creation,
        "cache_read_input_tokens": cache_read.max(0),
        "cache_creation": cache_creation_object(creation, cache_creation_1h),
    })
}

pub(crate) fn attach_cache_creation_object(usage: &mut Value, creation_1h: i32) {
    let creation = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0) as i32;
    usage["cache_creation"] = cache_creation_object(creation, creation_1h);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_json_defaults_creation_to_5m() {
        let usage = usage_json(10, 4, 80, 0, 20);
        assert_eq!(usage["input_tokens"], json!(10));
        assert_eq!(usage["output_tokens"], json!(4));
        assert_eq!(usage["cache_creation_input_tokens"], json!(80));
        assert_eq!(usage["cache_read_input_tokens"], json!(20));
        assert_eq!(
            usage["cache_creation"]["ephemeral_5m_input_tokens"],
            json!(80)
        );
        assert_eq!(
            usage["cache_creation"]["ephemeral_1h_input_tokens"],
            json!(0)
        );
    }

    #[test]
    fn usage_json_splits_mixed_ttl_and_clamps_1h() {
        let usage = usage_json(10, 4, 80, 30, 20);
        assert_eq!(
            usage["cache_creation"]["ephemeral_5m_input_tokens"],
            json!(50)
        );
        assert_eq!(
            usage["cache_creation"]["ephemeral_1h_input_tokens"],
            json!(30)
        );

        let over = usage_json(0, 0, 80, 500, 0);
        assert_eq!(
            over["cache_creation"]["ephemeral_5m_input_tokens"],
            json!(0)
        );
        assert_eq!(
            over["cache_creation"]["ephemeral_1h_input_tokens"],
            json!(80)
        );
    }

    #[test]
    fn attach_overwrites_nested_object_from_aggregate() {
        let mut usage = json!({
            "input_tokens": 1,
            "cache_creation_input_tokens": 9,
            "cache_read_input_tokens": 0
        });
        attach_cache_creation_object(&mut usage, 0);
        assert_eq!(
            usage["cache_creation"]["ephemeral_5m_input_tokens"],
            json!(9)
        );
        assert_eq!(
            usage["cache_creation"]["ephemeral_1h_input_tokens"],
            json!(0)
        );

        attach_cache_creation_object(&mut usage, 9);
        assert_eq!(
            usage["cache_creation"]["ephemeral_5m_input_tokens"],
            json!(0)
        );
        assert_eq!(
            usage["cache_creation"]["ephemeral_1h_input_tokens"],
            json!(9)
        );
    }
}
