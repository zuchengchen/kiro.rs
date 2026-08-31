//! 本机全周期累计积分（credit）计数
//!
//! [`UsageAggregator`](super::usage_stats::UsageAggregator) 的小时/天桶只保留 31 天，
//! `usage_log.*.jsonl` 也按保留期清理，所以两者都答不了「这台机器上的 kiro.rs
//! 一共消耗了多少积分」。这里用一个独立的单调计数器补上：
//!
//! - 每次请求结束时按上游 `meteringEvent` 上报的 credit 累加
//! - 落盘到 `cache_dir/credit_total.json`，重启后继续累加
//! - 首次启用时用现存 JSONL 历史播种（见 [`CreditTotal::seed_if_new`]），
//!   避免老部署升级后显示成 0
//!
//! 只统计实时请求：`rebuild_from_logs` 重放历史时不得走这里，否则每次重启都会把
//! 最近 31 天重复累加一遍。
//!
//! 写盘策略是时间去抖（[`SAVE_DEBOUNCE`]）+ 临时文件 rename：
//! 计数器是单调的，一旦文件被写坏或丢失就永久少算，所以宁可多付一次 rename。
//! 代价是进程被 SIGKILL 时最多丢失最后几秒的积分。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// 落盘文件名（位于 cache_dir 下）
const FILE_NAME: &str = "credit_total.json";

/// 两次落盘之间的最小间隔：把高频请求下的写放大压到每 3 秒一次
const SAVE_DEBOUNCE: Duration = Duration::from_secs(3);

/// 磁盘表示
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreditTotalFile {
    credits: f64,
    calls: u64,
    /// 开始计数的时刻（RFC3339）
    #[serde(default)]
    since: Option<String>,
    /// 最后一次累加的时刻（RFC3339）
    #[serde(default)]
    updated_at: Option<String>,
    /// 按上游凭据 id 分摊的累计量；key 为 id 的十进制字符串（JSON 对象键必须是字符串）
    ///
    /// 老版本文件没有这个字段，缺省为空——此时总量仍然可用，只是账号维度从 0 起算。
    #[serde(default)]
    by_credential: HashMap<String, CredentialCredits>,
}

/// 单个凭据的累计量
///
/// 两套口径并存，因为它们回答不同的问题：
/// - `credits` / `calls`：全周期累计，单调不减，用于「这个号在本机总共用了多少」
/// - `cycle_*`：**当前计费周期内**的累计，会在周期翻转时归零
///
/// 需要后者是因为上游 `getUsageLimits` 的 `currentUsage` 是按计费周期统计的，
/// 拿全周期累计去减它会得到负数或严重偏大的差值。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialCredits {
    pub credits: f64,
    pub calls: u64,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// 本计费周期内本机消耗的积分
    #[serde(default)]
    pub cycle_credits: f64,
    /// 本计费周期内本机转发的请求数
    #[serde(default)]
    pub cycle_calls: u64,
    /// 本周期的重置时刻（Unix 秒），来自余额的 `nextResetAt`；未查过余额时为 None
    #[serde(default)]
    pub cycle_reset_at: Option<f64>,
    /// 本周期的计数是否覆盖了整个周期
    ///
    /// 只有「亲眼看到周期翻转」（跨过 `cycle_reset_at` 后归零）才为 true。首次启用本功能、
    /// 或从历史日志播种时为 false —— 此时 `cycle_credits` 只是下界，据此算出的「其他机器」
    /// 会偏大，前端必须据此降级展示，不能假装精确。
    #[serde(default)]
    pub cycle_from_start: bool,
}

/// 对外快照（进 `/api/admin/stats/overview`）
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditTotalSnapshot {
    pub credits: f64,
    pub calls: u64,
    pub since: Option<String>,
    pub updated_at: Option<String>,
}

/// 共享句柄（Anthropic 路由与 Admin 路由共用同一个实例）
pub type SharedCreditTotal = std::sync::Arc<CreditTotal>;

/// 用量变化时的通知回调（把周期用量推给调度层，用于积分上限判断）
///
/// 用 `Arc` 而不是 `Box`：`notify_usage` 需要先克隆出回调、放掉 `usage_sink` 锁，
/// 再调用它。持锁调用第三方闭包等于把未知代码放进临界区，回调里任何一条回到本类型的
/// 路径都会死锁。
pub type UsageSink = Arc<dyn Fn(HashMap<u64, f64>) + Send + Sync>;

/// 单调累计计数器
pub struct CreditTotal {
    inner: Mutex<State>,
    /// None 表示纯内存模式（无 cache_dir / 测试）
    path: Option<PathBuf>,
    /// 周期用量变化时的订阅者。用回调而不是定时轮询：积分上限必须在超限后的
    /// 下一个请求就生效，轮询间隔内会漏放请求。
    usage_sink: Mutex<Option<UsageSink>>,
}

struct State {
    credits: f64,
    calls: u64,
    since: Option<String>,
    updated_at: Option<String>,
    /// 按凭据 id 分摊。总量不等于各凭据之和：没走到上游的请求记在总量里，
    /// 但没有凭据可归属（credential_id == 0）。
    by_credential: HashMap<u64, CredentialCredits>,
    /// 是否从磁盘读到过有效历史值。false 时允许用 JSONL 历史播种。
    loaded: bool,
    /// 有未落盘的增量
    dirty: bool,
    last_save_at: Option<Instant>,
}

impl State {
    fn snapshot(&self) -> CreditTotalSnapshot {
        CreditTotalSnapshot {
            credits: self.credits,
            calls: self.calls,
            since: self.since.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
}

impl CreditTotal {
    /// 纯内存计数器（不落盘）
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(State {
                credits: 0.0,
                calls: 0,
                since: None,
                updated_at: None,
                by_credential: HashMap::new(),
                loaded: false,
                dirty: false,
                last_save_at: None,
            }),
            path: None,
            usage_sink: Mutex::new(None),
        }
    }

    /// 注册周期用量订阅者，并立即推送一次当前值
    ///
    /// 立即推送很重要：进程刚启动时调度层的快照是空的，若等第一个请求结束才推送，
    /// 已经超限的账号会漏放一个请求。
    pub fn set_usage_sink(&self, sink: UsageSink) {
        let snapshot = self.cycle_usage_map();
        sink(snapshot);
        *self.usage_sink.lock() = Some(sink);
    }

    /// 把当前周期用量推给订阅者
    ///
    /// 必须在释放 `inner` 锁之后调用：`cycle_usage_map` 会再次获取 `inner`，
    /// 且回调本身要拿调度层的锁，持锁调用等于把两把锁嵌在一起。
    fn notify_usage(&self) {
        // 先克隆出回调并立刻放锁：回调内部会调用 cycle_usage_map / cycle_reset_map，
        // 这些又要拿 inner 锁；持 usage_sink 锁调用它没有直接死锁，但把未知代码留在
        // 临界区里，将来任何一条回到 set_usage_sink 的路径都会自锁。
        let Some(sink) = self.usage_sink.lock().clone() else {
            return;
        };
        sink(self.cycle_usage_map());
    }

    /// 从 `dir/credit_total.json` 载入；文件缺失或损坏时从 0 起算并允许播种
    pub fn load(dir: &Path) -> Self {
        // 兜底：空路径归一为 "."，与 UsageRecorder 保持一致，避免写到无目录前缀的路径
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        let path = dir.join(FILE_NAME);
        let mut total = Self::new();
        total.path = Some(path.clone());

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            // 首次运行：文件不存在，保持 loaded = false 等待播种
            Err(_) => return total,
        };
        match serde_json::from_str::<CreditTotalFile>(&content) {
            Ok(file) => {
                let mut state = total.inner.lock();
                // 单调计数器不接受负数 / NaN：坏值按 0 处理，后续累加照常
                state.credits = sane_credits(file.credits);
                state.calls = file.calls;
                state.since = file.since;
                state.updated_at = file.updated_at;
                // 键是十进制 id 字符串；解析不出来的条目直接丢弃（手工编辑过的文件）
                state.by_credential = file
                    .by_credential
                    .into_iter()
                    .filter_map(|(k, mut v)| {
                        let id = k.parse::<u64>().ok()?;
                        v.credits = sane_credits(v.credits);
                        Some((id, v))
                    })
                    .collect();
                state.loaded = true;
                // 先取值再放锁：parking_lot::Mutex 不可重入，在一条 info! 里锁两次会自死锁
                let (credits, calls) = (state.credits, state.calls);
                drop(state);
                tracing::info!(
                    "已载入本机累计积分：{:.6} credit / {} 次调用",
                    credits,
                    calls
                );
            }
            Err(e) => {
                // 解析失败时保持 loaded = false：让 JSONL 历史播种给出一个下界，
                // 好过静默从 0 起算
                tracing::warn!("解析 {} 失败，将按首次运行处理: {}", path.display(), e);
            }
        }
        total
    }

    /// 累加一次请求的积分消耗
    ///
    /// `credits` 非有限值或 ≤ 0 时只计调用次数（错误早退、上游未下发 metering 的请求）。
    /// `credential_id` 为 0 表示请求没走到上游（无凭据可归属），只进总量不进账号维度。
    pub fn add(&self, credential_id: u64, credits: f64) {
        let now = Utc::now().to_rfc3339();
        let delta = sane_credits(credits);
        let mut state = self.inner.lock();
        state.calls += 1;
        if delta > 0.0 {
            state.credits += delta;
        }
        if state.since.is_none() {
            state.since = Some(now.clone());
        }
        state.updated_at = Some(now.clone());
        if credential_id != 0 {
            let per = state.by_credential.entry(credential_id).or_default();
            // 先按当前时刻结算周期，再累加，否则跨过重置点的这一笔会记到上一个周期里
            roll_cycle_if_due(per, now_unix());
            per.calls += 1;
            per.cycle_calls += 1;
            if delta > 0.0 {
                per.credits += delta;
                per.cycle_credits += delta;
            }
            if per.since.is_none() {
                per.since = Some(now.clone());
            }
            per.updated_at = Some(now);
        }
        state.dirty = true;
        // loaded 置真：此后不再接受播种，避免重放历史把实时增量冲掉
        state.loaded = true;
        self.save_locked(&mut state, false);
        drop(state);
        // 出锁后再推送：积分上限要在下一个请求就生效
        self.notify_usage();
    }

    /// 记录某个凭据本周期的重置时刻（来自余额查询的 `nextResetAt`）
    ///
    /// 首次得知重置时刻时同时把周期计数清零并标记 `cycle_from_start`：从这一刻起本机记的
    /// 就是完整周期了。之后每次刷新余额只做「到期则翻转」的判断。
    pub fn note_cycle_reset(&self, credential_id: u64, reset_at: Option<f64>) {
        let Some(reset_at) = reset_at.filter(|v| v.is_finite() && *v > 0.0) else {
            return;
        };
        let mut state = self.inner.lock();
        let per = state.by_credential.entry(credential_id).or_default();
        let changed = match per.cycle_reset_at {
            // 已知周期：只在上游给出更晚的重置点（周期已翻转）时结算
            Some(known) => {
                if reset_at > known {
                    per.cycle_credits = 0.0;
                    per.cycle_calls = 0;
                    per.cycle_reset_at = Some(reset_at);
                    // 翻转是亲眼所见 → 从现在起本机计数覆盖整个周期
                    per.cycle_from_start = true;
                    true
                } else {
                    false
                }
            }
            // 首次得知：无法追溯本周期已过去的部分，所以只记下边界，
            // cycle_from_start 保持 false，让前端把差值标成「至多」
            None => {
                per.cycle_reset_at = Some(reset_at);
                true
            }
        };
        if changed {
            state.dirty = true;
            self.save_locked(&mut state, true);
            drop(state);
            // 周期翻转会把用量归零 → 必须让调度层立刻解除限制
            self.notify_usage();
        }
    }

    /// 各凭据本周期已消耗积分（id → cycle_credits），供调度层判断积分上限
    ///
    /// 与 [`Self::by_credential`] 一样在读取时结算过期周期，保证上限判断不会用旧周期的
    /// 数字把账号错误地挡住。
    pub fn cycle_usage_map(&self) -> HashMap<u64, f64> {
        self.by_credential()
            .into_iter()
            .map(|(id, v)| (id, v.cycle_credits))
            .collect()
    }

    /// 各凭据的周期重置时刻（id → Unix 秒），用于生成积分上限拒绝时的 `Retry-After`
    pub fn cycle_reset_map(&self) -> HashMap<u64, f64> {
        self.by_credential()
            .into_iter()
            .filter_map(|(id, v)| v.cycle_reset_at.map(|r| (id, r)))
            .collect()
    }

    /// 全部凭据的累计量（一次加锁取完，避免 N 个凭据 N 次加锁）
    ///
    /// 读取时也按当前时刻结算周期：长时间没有流量的凭据不会一直显示上个周期的数字。
    ///
    /// 刻意**不**触发 [`Self::notify_usage`]：订阅回调本身要调用本方法取快照，
    /// 在这里推送会无限递归。周期翻转经由 `note_cycle_reset`（余额刷新）或下一次
    /// `add` 推给调度层；两者都没发生时账号处于空闲状态，晚一步解除限制无影响。
    pub fn by_credential(&self) -> HashMap<u64, CredentialCredits> {
        let mut state = self.inner.lock();
        let now = now_unix();
        let mut changed = false;
        for per in state.by_credential.values_mut() {
            changed |= roll_cycle_if_due(per, now);
        }
        if changed {
            state.dirty = true;
            self.save_locked(&mut state, false);
        }
        state.by_credential.clone()
    }

    /// 丢弃某个凭据的累计量
    ///
    /// 凭据被删除时调用。`next_id` 由 `max_existing_id + 1` 推导，删掉最大 id 再重启会
    /// 把它分配给新账号；不清理的话新账号会继承前任的积分。
    pub fn forget_credential(&self, credential_id: u64) {
        let mut state = self.inner.lock();
        if state.by_credential.remove(&credential_id).is_none() {
            return;
        }
        state.dirty = true;
        // 立即落盘：删除是低频操作，不值得等去抖窗口
        self.save_locked(&mut state, true);
        drop(state);
        self.notify_usage();
    }

    /// 首次启用时用现存历史播种
    ///
    /// 仅当磁盘上没有有效计数文件、且本进程还没记到任何实时请求时生效。老部署升级后
    /// 至少能看到保留期内的历史积分，而不是 0。
    pub fn seed_if_new(
        &self,
        credits: f64,
        calls: u64,
        since: Option<String>,
        by_credential: HashMap<u64, CredentialCredits>,
    ) {
        let mut state = self.inner.lock();
        if state.loaded || state.calls > 0 || state.credits > 0.0 {
            return;
        }
        state.credits = sane_credits(credits);
        state.calls = calls;
        state.since = since;
        state.updated_at = Some(Utc::now().to_rfc3339());
        state.by_credential = by_credential
            .into_iter()
            .map(|(id, mut v)| {
                v.credits = sane_credits(v.credits);
                (id, v)
            })
            .collect();
        state.loaded = true;
        state.dirty = true;
        let seeded = state.credits;
        // 立即落盘：下次启动就能直接读到，不再依赖已被清理的 JSONL
        self.save_locked(&mut state, true);
        drop(state);
        tracing::info!(
            "本机累计积分首次初始化：从历史日志播种 {:.6} credit / {} 次调用",
            seeded,
            calls
        );
    }

    /// 当前累计值
    pub fn snapshot(&self) -> CreditTotalSnapshot {
        self.inner.lock().snapshot()
    }

    /// 强制落盘（忽略去抖间隔）
    pub fn flush(&self) {
        let mut state = self.inner.lock();
        self.save_locked(&mut state, true);
    }

    /// 落盘。`force` 为 false 时受 [`SAVE_DEBOUNCE`] 限制。
    ///
    /// 持锁写文件是有意的：计数单调递增，放开锁再写会让并发的旧值覆盖新值。
    /// 文件只有百余字节，且去抖后每 3 秒最多一次。
    fn save_locked(&self, state: &mut State, force: bool) {
        let Some(path) = &self.path else {
            state.dirty = false;
            return;
        };
        if !state.dirty {
            return;
        }
        if !force {
            if let Some(last) = state.last_save_at {
                if last.elapsed() < SAVE_DEBOUNCE {
                    return;
                }
            }
        }
        let file = CreditTotalFile {
            credits: state.credits,
            calls: state.calls,
            since: state.since.clone(),
            updated_at: state.updated_at.clone(),
            by_credential: state
                .by_credential
                .iter()
                .map(|(id, v)| (id.to_string(), v.clone()))
                .collect(),
        };
        let json = match serde_json::to_string_pretty(&file) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("序列化本机累计积分失败: {}", e);
                return;
            }
        };
        // 先写临时文件再 rename：进程在写一半时挂掉也不会把计数器截断成半个 JSON
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            tracing::warn!("写入 {} 失败: {}", tmp.display(), e);
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::warn!("替换 {} 失败: {}", path.display(), e);
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        state.dirty = false;
        state.last_save_at = Some(Instant::now());
    }
}

impl Default for CreditTotal {
    fn default() -> Self {
        Self::new()
    }
}

/// 归一化外部来源的 credit：非有限值 / 负数一律当 0
fn sane_credits(v: f64) -> f64 {
    if v.is_finite() && v > 0.0 { v } else { 0.0 }
}

fn now_unix() -> f64 {
    Utc::now().timestamp() as f64
}

/// 已过重置时刻则把周期计数归零
///
/// 返回是否发生了翻转。上游下一次刷新余额会给出新的 `cycle_reset_at`；在那之前先把
/// 边界置空，避免反复触发。翻转后 `cycle_from_start` 置真：新周期从零开始记，本机
/// 计数就覆盖了整个周期。
fn roll_cycle_if_due(per: &mut CredentialCredits, now: f64) -> bool {
    let Some(reset_at) = per.cycle_reset_at else {
        return false;
    };
    if now < reset_at {
        return false;
    }
    per.cycle_credits = 0.0;
    per.cycle_calls = 0;
    per.cycle_reset_at = None;
    per.cycle_from_start = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一的空目录（本 crate 没有 tempfile 依赖，沿用 token_manager 测试的写法）
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new() -> Self {
            let p =
                std::env::temp_dir().join(format!("kiro-credit-total-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn in_memory_accumulates() {
        let total = CreditTotal::new();
        total.add(7, 0.5);
        total.add(7, 0.25);
        let snap = total.snapshot();
        assert_eq!(snap.calls, 2);
        assert!((snap.credits - 0.75).abs() < 1e-12);
        assert!(snap.since.is_some());
    }

    #[test]
    fn non_finite_and_negative_credits_only_count_calls() {
        let total = CreditTotal::new();
        total.add(7, f64::NAN);
        total.add(7, -1.0);
        total.add(7, 0.0);
        let snap = total.snapshot();
        assert_eq!(snap.calls, 3);
        assert_eq!(snap.credits, 0.0);
        // 账号维度同样只涨次数
        let per = total.by_credential();
        assert_eq!(per[&7].calls, 3);
        assert_eq!(per[&7].credits, 0.0);
    }

    #[test]
    fn survives_reload() {
        let dir = TmpDir::new();
        {
            let total = CreditTotal::load(dir.path());
            total.add(7, 1.5);
            total.add(9, 2.0);
            total.flush();
        }
        let reloaded = CreditTotal::load(dir.path());
        let snap = reloaded.snapshot();
        assert_eq!(snap.calls, 2);
        assert!((snap.credits - 3.5).abs() < 1e-12);

        // 账号维度也必须跨重启存活
        let per = reloaded.by_credential();
        assert!((per[&7].credits - 1.5).abs() < 1e-12);
        assert!((per[&9].credits - 2.0).abs() < 1e-12);

        // 重启后继续累加，不从 0 重来
        reloaded.add(7, 0.5);
        assert!((reloaded.snapshot().credits - 4.0).abs() < 1e-12);
        assert_eq!(reloaded.snapshot().calls, 3);
        assert!((reloaded.by_credential()[&7].credits - 2.0).abs() < 1e-12);
    }

    #[test]
    fn seed_only_applies_to_a_fresh_store() {
        let dir = TmpDir::new();
        let total = CreditTotal::load(dir.path());
        total.seed_if_new(
            10.0,
            4,
            Some("2026-01-01T00:00:00Z".to_string()),
            HashMap::from([(
                7u64,
                CredentialCredits {
                    credits: 6.0,
                    calls: 2,
                    since: Some("2026-01-01T00:00:00Z".to_string()),
                    updated_at: Some("2026-01-02T00:00:00Z".to_string()),
                    ..Default::default()
                },
            )]),
        );
        let snap = total.snapshot();
        assert_eq!(snap.calls, 4);
        assert!((snap.credits - 10.0).abs() < 1e-12);
        assert_eq!(snap.since.as_deref(), Some("2026-01-01T00:00:00Z"));

        // 已有值后再播种应当被忽略（否则重启会覆盖真实累计）
        assert!((total.by_credential()[&7].credits - 6.0).abs() < 1e-12);
        total.seed_if_new(999.0, 999, None, HashMap::new());
        assert!((total.snapshot().credits - 10.0).abs() < 1e-12);

        // 重启后读到磁盘值 → 同样不接受播种
        let reloaded = CreditTotal::load(dir.path());
        reloaded.seed_if_new(999.0, 999, None, HashMap::new());
        assert!((reloaded.snapshot().credits - 10.0).abs() < 1e-12);
        assert_eq!(reloaded.snapshot().calls, 4);
        // 播种的账号分摊值同样跨重启存活
        assert!((reloaded.by_credential()[&7].credits - 6.0).abs() < 1e-12);
    }

    #[test]
    fn credential_zero_counts_only_toward_the_machine_total() {
        let total = CreditTotal::new();
        // 没走到上游的请求（鉴权失败、无可用凭据）没有凭据可归属
        total.add(0, 1.25);
        total.add(4, 0.75);
        let snap = total.snapshot();
        assert_eq!(snap.calls, 2);
        assert!((snap.credits - 2.0).abs() < 1e-12);

        let per = total.by_credential();
        assert_eq!(per.len(), 1, "credential_id 0 不应建立分摊条目");
        assert!(!per.contains_key(&0));
        assert!((per[&4].credits - 0.75).abs() < 1e-12);
        assert_eq!(per[&4].calls, 1);
    }

    #[test]
    fn forget_credential_drops_only_that_account() {
        let dir = TmpDir::new();
        let total = CreditTotal::load(dir.path());
        total.add(7, 1.0);
        total.add(9, 2.0);

        total.forget_credential(7);
        let per = total.by_credential();
        assert!(!per.contains_key(&7), "被删除凭据的分摊值必须清掉");
        assert!((per[&9].credits - 2.0).abs() < 1e-12);
        // 机器总量是历史事实，不因删号回退
        assert!((total.snapshot().credits - 3.0).abs() < 1e-12);
        assert_eq!(total.snapshot().calls, 2);

        // 必须已落盘：否则重启后 id 复用会让新账号继承前任的数字
        let reloaded = CreditTotal::load(dir.path());
        assert!(!reloaded.by_credential().contains_key(&7));
        assert!((reloaded.by_credential()[&9].credits - 2.0).abs() < 1e-12);
    }

    #[test]
    fn cycle_counter_tracks_alongside_the_lifetime_counter() {
        let total = CreditTotal::new();
        let future = now_unix() + 3600.0;
        total.note_cycle_reset(7, Some(future));
        total.add(7, 1.0);
        total.add(7, 0.5);

        let per = total.by_credential();
        let e = &per[&7];
        assert!((e.credits - 1.5).abs() < 1e-12);
        assert!((e.cycle_credits - 1.5).abs() < 1e-12);
        assert_eq!(e.cycle_calls, 2);
        // 首次得知边界无法追溯本周期已过去的部分 → 不敢声称精确
        assert!(!e.cycle_from_start);
    }

    #[test]
    fn later_reset_point_rolls_the_cycle_but_keeps_the_lifetime_total() {
        let total = CreditTotal::new();
        let first = now_unix() + 3600.0;
        total.note_cycle_reset(7, Some(first));
        total.add(7, 2.0);

        // 上游给出更晚的重置点 = 周期已翻转
        total.note_cycle_reset(7, Some(first + 30.0 * 86400.0));
        let per = total.by_credential();
        let e = &per[&7];
        assert!((e.credits - 2.0).abs() < 1e-12, "全周期累计不受周期翻转影响");
        assert_eq!(e.cycle_credits, 0.0, "周期计数必须归零");
        assert_eq!(e.cycle_calls, 0);
        // 亲眼见到翻转 → 之后的周期计数覆盖完整周期
        assert!(e.cycle_from_start);

        total.add(7, 0.25);
        let per = total.by_credential();
        assert!((per[&7].cycle_credits - 0.25).abs() < 1e-12);
        assert!((per[&7].credits - 2.25).abs() < 1e-12);
    }

    #[test]
    fn passing_the_reset_point_rolls_the_cycle_on_read() {
        let total = CreditTotal::new();
        let past = now_unix() - 10.0;
        total.note_cycle_reset(7, Some(past));
        // 先攒一笔，然后让周期过期 —— 没有新流量时也必须在读取时结算，
        // 否则安静的凭据会一直显示上个周期的数字
        {
            let mut state = total.inner.lock();
            let per = state.by_credential.entry(7).or_default();
            per.credits = 3.0;
            per.calls = 1;
            per.cycle_credits = 3.0;
            per.cycle_calls = 1;
            per.cycle_reset_at = Some(past);
        }

        let per = total.by_credential();
        let e = &per[&7];
        assert!((e.credits - 3.0).abs() < 1e-12, "全周期累计必须保留");
        assert_eq!(e.cycle_credits, 0.0, "过期周期不得继续展示旧数字");
        assert!(
            e.cycle_reset_at.is_none(),
            "边界应清空，等下次余额刷新给出新值"
        );
        assert!(e.cycle_from_start);
    }

    #[test]
    fn a_request_after_the_reset_point_lands_in_the_new_cycle() {
        let total = CreditTotal::new();
        total.note_cycle_reset(7, Some(now_unix() - 10.0));
        // add 先结算周期再累加：这一笔属于新周期，不该被自己的结算清掉
        total.add(7, 3.0);

        let per = total.by_credential();
        let e = &per[&7];
        assert!((e.credits - 3.0).abs() < 1e-12);
        assert!(
            (e.cycle_credits - 3.0).abs() < 1e-12,
            "跨过重置点的请求应计入新周期"
        );
        assert_eq!(e.cycle_calls, 1);
        assert!(e.cycle_from_start);
    }

    #[test]
    fn note_cycle_reset_ignores_garbage_and_earlier_values() {
        let total = CreditTotal::new();
        let known = now_unix() + 3600.0;
        total.note_cycle_reset(7, Some(known));
        total.add(7, 1.0);

        // 非法值与更早的重置点都不该清掉周期计数
        total.note_cycle_reset(7, None);
        total.note_cycle_reset(7, Some(f64::NAN));
        total.note_cycle_reset(7, Some(0.0));
        total.note_cycle_reset(7, Some(known - 600.0));

        let per = total.by_credential();
        assert!((per[&7].cycle_credits - 1.0).abs() < 1e-12);
        assert_eq!(per[&7].cycle_reset_at, Some(known));
    }

    #[test]
    fn cycle_state_survives_reload() {
        let dir = TmpDir::new();
        let future = now_unix() + 3600.0;
        {
            let total = CreditTotal::load(dir.path());
            total.note_cycle_reset(7, Some(future));
            total.add(7, 1.25);
            total.flush();
        }
        let reloaded = CreditTotal::load(dir.path());
        let per = reloaded.by_credential();
        assert!((per[&7].cycle_credits - 1.25).abs() < 1e-12);
        assert_eq!(per[&7].cycle_reset_at, Some(future));
    }

    #[test]
    fn legacy_file_without_by_credential_still_loads() {
        let dir = TmpDir::new();
        // v1 格式：只有总量，没有 byCredential
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"{"credits":12.5,"calls":6,"since":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let total = CreditTotal::load(dir.path());
        let snap = total.snapshot();
        assert!((snap.credits - 12.5).abs() < 1e-12, "总量必须保留");
        assert_eq!(snap.calls, 6);
        // 账号维度从 0 起算，但不影响继续累加
        assert!(total.by_credential().is_empty());
        total.add(3, 1.0);
        assert!((total.by_credential()[&3].credits - 1.0).abs() < 1e-12);
        assert!((total.snapshot().credits - 13.5).abs() < 1e-12);
    }

    /// 回归：`load()` 命中已有文件时不得在持锁期间求值日志参数
    ///
    /// `tracing` 宏只在有订阅者关心该级别时才求值参数；`cargo test` 默认不装订阅者，
    /// 所以「在一条 `info!` 里锁两次」的自死锁在普通测试下根本不会执行到。这里显式装一个
    /// 订阅者强制求值，并把 load 放到子线程里限时 join —— 死锁时报失败而不是永久挂起。
    #[test]
    fn load_with_active_log_subscriber_does_not_deadlock() {
        let dir = TmpDir::new();
        {
            let total = CreditTotal::load(dir.path());
            total.add(7, 1.0);
            total.flush();
        }
        let path = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let subscriber = tracing_subscriber::FmtSubscriber::builder()
                .with_max_level(tracing::Level::TRACE)
                .with_test_writer()
                .finish();
            let credits = tracing::subscriber::with_default(subscriber, || {
                CreditTotal::load(&path).snapshot().credits
            });
            let _ = tx.send(credits);
        });
        let credits = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("load() 在有日志订阅者时必须返回（不得自死锁）");
        assert!((credits - 1.0).abs() < 1e-12);
    }

    #[test]
    fn corrupt_file_is_treated_as_fresh() {
        let dir = TmpDir::new();
        std::fs::write(dir.path().join(FILE_NAME), "{ not json").unwrap();
        let total = CreditTotal::load(dir.path());
        assert_eq!(total.snapshot().credits, 0.0);
        // 允许播种给出下界
        total.seed_if_new(7.0, 3, None, HashMap::new());
        assert!((total.snapshot().credits - 7.0).abs() < 1e-12);
    }

    #[test]
    fn debounce_defers_write_but_flush_forces_it() {
        let dir = TmpDir::new();
        let path = dir.path().join(FILE_NAME);
        let total = CreditTotal::load(dir.path());
        total.add(7, 1.0); // 首次 add 没有 last_save_at → 立即落盘
        assert!(path.exists());
        total.add(7, 2.0); // 落在去抖窗口内 → 只留在内存
        let on_disk: CreditTotalFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!((on_disk.credits - 1.0).abs() < 1e-12);
        total.flush();
        let on_disk: CreditTotalFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!((on_disk.credits - 3.0).abs() < 1e-12);
    }
}
