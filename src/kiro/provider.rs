//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::admin::trace_db::{TraceAttempt, TraceSink, outcome, truncate_snippet};
use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::error::UpstreamRateLimitError;
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::TlsBackend;
use parking_lot::Mutex;

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 总重试次数硬上限（避免无限重试）
///
/// 注：上游 429 多为账号级速率配额（SERVICE_REQUEST_RATE_EXCEEDED），高峰期
/// 多账号同时触顶时，过多重试会在账号间连环撞墙、放大限流。故上限取较小值，
/// 配合 429 专用长退避（见 retry_delay_throttle），被限时尽早返回而非耗尽配额。
const MAX_TOTAL_RETRIES: usize = 4;

/// HTTP Client 缓存容量上限（不含常驻的全局代理 client）。
/// 代理池条目较多时，避免每个不同代理都常驻一个 reqwest::Client 导致内存无界增长。
const CLIENT_CACHE_CAP: usize = 64;

/// The fallback must resolve before the outer stream's zero-byte guard fires.
const FALLBACK_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(10);

/// Failed profile discovery is retried after a bounded negative-cache window.
const PROFILE_RESOLUTION_NEGATIVE_TTL: Duration = Duration::from_secs(5 * 60);

/// 带容量上限的 HTTP Client 缓存。
///
/// - key 为 effective proxy 配置（None = 直连/全局回退）
/// - 受保护 key（全局代理对应的 effective 配置）永不被淘汰
/// - 超出容量时按插入顺序淘汰最旧的「非受保护」条目
struct ClientCache {
    map: HashMap<Option<ProxyConfig>, Client>,
    /// 插入顺序（仅记录可淘汰的非受保护 key）
    order: std::collections::VecDeque<Option<ProxyConfig>>,
    /// 受保护、不参与淘汰的 key（全局代理）
    protected: Option<ProxyConfig>,
    cap: usize,
}

impl ClientCache {
    fn new(protected: Option<ProxyConfig>, initial: Client, cap: usize) -> Self {
        let mut map = HashMap::new();
        map.insert(protected.clone(), initial);
        Self {
            map,
            order: std::collections::VecDeque::new(),
            protected,
            cap,
        }
    }

    fn get(&self, key: &Option<ProxyConfig>) -> Option<Client> {
        self.map.get(key).cloned()
    }

    /// 插入新条目，必要时淘汰最旧的非受保护条目
    fn insert(&mut self, key: Option<ProxyConfig>, client: Client) {
        if key == self.protected || self.map.contains_key(&key) {
            self.map.insert(key, client);
            return;
        }
        while self.order.len() >= self.cap {
            if let Some(evict) = self.order.pop_front() {
                self.map.remove(&evict);
            } else {
                break;
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, client);
    }
}

/// API 调用结果，附带本次实际命中的上游凭据 ID（用于用量统计）
pub struct KiroCallResult {
    pub response: reqwest::Response,
    pub credential_id: u64,
}

/// A successful MCP HTTP response whose trace attempt is finalized after body validation.
struct McpCallResult {
    response: reqwest::Response,
    credential_id: u64,
    endpoint: &'static str,
    attempt: usize,
    started_at: Instant,
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client。
    /// 带容量上限淘汰（全局代理 client 常驻），避免代理数量增长导致内存无界增长。
    client_cache: Mutex<ClientCache>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 上游请求总超时（秒）。启动时从配置读定，供 client_for 建缓存 Client 使用。
    upstream_timeout_secs: u64,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
    /// Failed/empty profileArn discovery attempts, with a five-minute TTL.
    ///
    /// 避免对「无 Enterprise profile」的账号（如纯 BuilderID）在每次请求都重复调用
    /// `ListAvailableProfiles`。命中真实 ARN 的账号会把 ARN 持久化进凭据，之后
    /// 通过 `streaming_profile_arn()` 直接命中，不再进入解析路径。
    profile_resolution_negative_cache: Mutex<HashMap<u64, Instant>>,
}

impl KiroProvider {
    /// 返回共享凭据管理器，供模型发现等只读控制面逻辑复用。
    pub fn token_manager(&self) -> &Arc<MultiTokenManager> {
        &self.token_manager
    }

    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        let tls_backend = token_manager.config().tls_backend;
        // 启动时读定，避免在 client_for 热路径上反复取配置锁。
        let upstream_timeout_secs = token_manager.config().upstream_timeout_secs;
        // 预热：构建全局代理对应的 Client（作为受保护的常驻条目）
        let initial_client = build_client(proxy.as_ref(), upstream_timeout_secs, tls_backend)
            .expect("创建 HTTP 客户端失败");
        let client_cache = ClientCache::new(proxy.clone(), initial_client, CLIENT_CACHE_CAP);

        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(client_cache),
            tls_backend,
            upstream_timeout_secs,
            endpoints,
            default_endpoint,
            profile_resolution_negative_cache: Mutex::new(HashMap::new()),
        }
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client);
        }
        let client = build_client(effective.as_ref(), self.upstream_timeout_secs, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        let name = credentials
            .endpoint
            .as_deref()
            .unwrap_or(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// 在发起请求前，确保 Enterprise / IdC 账号的真实 profileArn 已解析并写入 `ctx`。
    ///
    /// 流式端点强制要求 profileArn；Enterprise / IdC 账号必须先把 BuilderID
    /// 占位符解析为真实 ARN，纯 BuilderID 账号则回退占位符。
    /// 仅对「profileArn 缺失或为占位符」的账号触发一次上游
    /// `ListAvailableProfiles` 查询（进程内去重）：
    /// - 命中真实 ARN → 写回 `ctx.credentials.profile_arn` 并由 token_manager 持久化；
    ///   之后该凭据的 `streaming_profile_arn()` 直接命中，不再进入此路径。
    /// - 无 Enterprise profile（纯 BuilderID 等）→ 保持占位符回退逻辑，并标记已尝试，
    ///   避免每次请求重复查询。
    async fn ensure_profile_arn(
        &self,
        ctx: &mut crate::kiro::token_manager::CallContext,
    ) -> anyhow::Result<()> {
        use crate::kiro::model::credentials::is_placeholder_profile_arn;

        let needs = match ctx.credentials.profile_arn.as_deref() {
            None => true,
            Some(arn) => is_placeholder_profile_arn(arn),
        };
        if !needs {
            return Ok(());
        }
        if self
            .profile_resolution_negative_cache
            .lock()
            .get(&ctx.id)
            .is_some_and(|attempted_at| attempted_at.elapsed() < PROFILE_RESOLUTION_NEGATIVE_TTL)
        {
            return Ok(());
        }
        match self
            .token_manager
            .resolve_profile_arn_for(ctx.id, &ctx.token)
            .await
        {
            Ok(Some(arn)) => {
                ctx.credentials.profile_arn = Some(arn);
                self.profile_resolution_negative_cache
                    .lock()
                    .remove(&ctx.id);
            }
            Ok(None) => {
                self.profile_resolution_negative_cache
                    .lock()
                    .insert(ctx.id, Instant::now());
            }
            Err(e) => {
                if is_rate_limit_error(&e) {
                    return Err(e);
                }
                self.profile_resolution_negative_cache
                    .lock()
                    .insert(ctx.id, Instant::now());
                tracing::warn!(
                    "凭据 #{} 解析真实 profileArn 失败（按原 profileArn 继续）: {}",
                    ctx.id,
                    e
                );
            }
        }
        Ok(())
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）。
    /// `sink` 可选，用于逐跳上报链路追踪。
    pub async fn call_api(
        &self,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        self.call_api_with_retry(request_body, false, sink, group)
            .await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(
        &self,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        self.call_api_with_retry(request_body, true, sink, group)
            .await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(
        &self,
        request_body: &str,
        group: Option<&str>,
    ) -> anyhow::Result<reqwest::Response> {
        let result = self.call_mcp_with_retry(request_body, None, group).await?;
        self.token_manager
            .report_success_for_request(result.credential_id, None);
        Ok(result.response)
    }

    /// 发送 MCP API 请求，并在响应正文通过调用方校验后提交最终 trace attempt。
    pub(crate) async fn call_mcp_with_trace<T>(
        &self,
        request_body: &str,
        sink: &dyn TraceSink,
        group: Option<&str>,
        validate: fn(&str) -> anyhow::Result<T>,
        is_benign_error: fn(&anyhow::Error) -> bool,
    ) -> anyhow::Result<T> {
        let result = self
            .call_mcp_with_retry(request_body, Some(sink), group)
            .await?;
        let status = result.response.status().as_u16();
        let body = match result.response.text().await {
            Ok(body) => body,
            Err(e) => {
                Self::emit_attempt(
                    Some(sink),
                    result.attempt,
                    result.credential_id,
                    result.endpoint,
                    Some(status),
                    outcome::NETWORK_ERROR,
                    Some(&e.to_string()),
                    result.started_at,
                );
                return Err(e.into());
            }
        };

        let validation = validate(&body);
        let validation_outcome = Self::mcp_validation_outcome(&validation, is_benign_error);
        let error = validation
            .as_ref()
            .err()
            .filter(|_| validation_outcome != outcome::SUCCESS)
            .map(|e| format!("{}: {}", e, body));
        Self::emit_attempt(
            Some(sink),
            result.attempt,
            result.credential_id,
            result.endpoint,
            Some(status),
            validation_outcome,
            error.as_deref(),
            result.started_at,
        );
        if validation_outcome == outcome::SUCCESS {
            self.token_manager
                .report_success_for_request(result.credential_id, None);
        }
        validation
    }

    fn mcp_validation_outcome<T>(
        validation: &anyhow::Result<T>,
        is_benign_error: fn(&anyhow::Error) -> bool,
    ) -> &'static str {
        match validation {
            Ok(_) => outcome::SUCCESS,
            Err(error) if is_benign_error(error) => outcome::SUCCESS,
            Err(_) => outcome::UNKNOWN,
        }
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(
        &self,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<McpCallResult> {
        let total_credentials = self.token_manager.total_count_in_group(group).max(1);
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        // 同 call_api_with_retry：等待预算在重试间共享，避免累计放大。
        let mut wait_budget = self.token_manager.new_acquire_wait_budget();

        for attempt in 0..max_retries {
            let attempt_start = Instant::now();
            // MCP 调用不涉及模型选择，但必须遵守客户端 Key 的凭据分组隔离。
            let mut ctx = match self
                .token_manager
                .acquire_context_with_budget(None, group, &mut wait_budget)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    if is_rate_limit_error(&e) {
                        Self::emit_attempt(
                            sink,
                            attempt,
                            0,
                            "",
                            None,
                            outcome::TRANSIENT,
                            Some(&e.to_string()),
                            attempt_start,
                        );
                        return Err(e);
                    }
                    // Preserve the prior upstream 429 as the terminal trace attempt. A
                    // concurrent selection failure has no credential to attribute.
                    if let Some(rate_limit) = take_rate_limit_error(&mut last_error) {
                        return Err(rate_limit);
                    }
                    Self::emit_attempt(
                        sink,
                        attempt,
                        0,
                        "",
                        None,
                        outcome::UNKNOWN,
                        Some(&e.to_string()),
                        attempt_start,
                    );
                    last_error = Some(e);
                    continue;
                }
            };

            // Pure MCP routes (including Web Search) require the same Enterprise / IdC
            // profileArn resolution as regular model calls.
            if let Err(e) = self.ensure_profile_arn(&mut ctx).await {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    "",
                    None,
                    outcome::TRANSIENT,
                    Some(&e.to_string()),
                    attempt_start,
                );
                return Err(e);
            }

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    Self::emit_attempt(
                        sink,
                        attempt,
                        ctx.id,
                        "",
                        None,
                        outcome::UNKNOWN,
                        Some(&e.to_string()),
                        attempt_start,
                    );
                    last_error = Some(e);
                    // endpoint 解析失败：记为失败，换下一张凭据
                    self.token_manager
                        .report_failure_for_request(ctx.id, None, group);
                    continue;
                }
            };
            let endpoint_name = endpoint.name();

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            let client = match self.client_for(&ctx.credentials) {
                Ok(client) => client,
                Err(e) => {
                    Self::emit_attempt(
                        sink,
                        attempt,
                        ctx.id,
                        endpoint_name,
                        None,
                        outcome::NETWORK_ERROR,
                        Some(&e.to_string()),
                        attempt_start,
                    );
                    return Err(e);
                }
            };
            let base = client
                .post(&url)
                .body(body)
                .header("content-type", endpoint.content_type())
                .header("Connection", "close");
            let request = endpoint.decorate_mcp(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    Self::emit_attempt(
                        sink,
                        attempt,
                        ctx.id,
                        endpoint_name,
                        None,
                        outcome::NETWORK_ERROR,
                        Some(&e.to_string()),
                        attempt_start,
                    );
                    // 凭据专属代理故障时，跳过该凭据换下一个。
                    let has_own_proxy = ctx
                        .credentials
                        .proxy_url
                        .as_deref()
                        .is_some_and(|u| !u.trim().is_empty());
                    if has_own_proxy {
                        tracing::warn!("凭据 #{} 有专属代理且 MCP 请求失败，跳过该凭据", ctx.id);
                        self.token_manager
                            .report_failure_for_request(ctx.id, None, group);
                    }
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();
            let rate_limit_error = (status.as_u16() == 429)
                .then(|| UpstreamRateLimitError::from_headers(response.headers()));

            // 成功响应
            if status.is_success() {
                return Ok(McpCallResult {
                    response,
                    credential_id: ctx.id,
                    endpoint: endpoint_name,
                    attempt,
                    started_at: attempt_start,
                });
            }

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 402 额度用尽
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::QUOTA_EXHAUSTED,
                    Some(&body),
                    attempt_start,
                );
                let has_available = self
                    .token_manager
                    .report_quota_exhausted_for_request(ctx.id, None, group);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // 403 + 明确封禁文案：账号被封禁，立即禁用且不参与自愈（受配置开关控制）
                if status.as_u16() == 403
                    && self.token_manager.get_suspended_detection_enabled()
                    && endpoint.is_account_suspended(&body)
                {
                    Self::emit_attempt(
                        sink,
                        attempt,
                        ctx.id,
                        endpoint_name,
                        Some(status.as_u16()),
                        outcome::ACCOUNT_SUSPENDED,
                        Some(&body),
                        attempt_start,
                    );
                    let has_available = self
                        .token_manager
                        .report_suspended_for_request(ctx.id, None, group);
                    if !has_available {
                        anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                    }
                    last_error = Some(anyhow::anyhow!(
                        "MCP 请求失败（账号封禁）: {} {}",
                        status,
                        body
                    ));
                    continue;
                }

                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::AUTH_FAILED,
                    Some(&body),
                    attempt_start,
                );

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if Self::handle_force_refresh_result(
                        self.token_manager.force_refresh_token_for(ctx.id).await,
                    )? {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self
                    .token_manager
                    .report_failure_for_request(ctx.id, None, group);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::TRANSIENT,
                    Some(&body),
                    attempt_start,
                );
                last_error = if let Some(rate_limit) = rate_limit_error {
                    if !rate_limit.should_retry_locally() {
                        return Err(rate_limit.into());
                    }
                    Some(rate_limit.into())
                } else {
                    Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body))
                };
                if attempt + 1 < max_retries {
                    // 429 限流用更长退避；408/5xx 仍用通用快速退避
                    let delay = if status.as_u16() == 429 {
                        Self::retry_delay_throttle(attempt)
                    } else {
                        Self::retry_delay(attempt)
                    };
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            Self::emit_attempt(
                sink,
                attempt,
                ctx.id,
                endpoint_name,
                Some(status.as_u16()),
                outcome::UNKNOWN,
                Some(&body),
                attempt_start,
            );
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    async fn prepare_endpoint_api_body(
        &self,
        endpoint: &dyn KiroEndpoint,
        credential_id: u64,
        request_body: &str,
        rctx: &RequestContext<'_>,
    ) -> String {
        let body = if endpoint.requires_codewhisperer_model_id() {
            if let Some(requested) = Self::extract_model_from_request(request_body) {
                let resolved = self
                    .token_manager
                    .resolve_codewhisperer_model_id_for(credential_id, &requested)
                    .await;
                crate::kiro::endpoint::apply_payload_model_id(request_body, &resolved)
            } else {
                request_body.to_string()
            }
        } else {
            request_body.to_string()
        };
        endpoint.transform_api_body(&body, rctx)
    }

    /// Build and send one data-plane request. The returned response still owns
    /// its streaming body; callers must keep body parsing outside timeout/error
    /// scopes so a mid-stream error cannot be relabeled as a quota failure.
    async fn execute_endpoint_api_request(
        &self,
        endpoint: &dyn KiroEndpoint,
        credential_id: u64,
        credentials: &KiroCredentials,
        request_body: &str,
        rctx: &RequestContext<'_>,
    ) -> anyhow::Result<reqwest::Response> {
        let url = endpoint.api_url(rctx);
        let body = self
            .prepare_endpoint_api_body(endpoint, credential_id, request_body, rctx)
            .await;

        tracing::debug!("使用端点 [{}] POST {}", endpoint.display_name(), url);
        tracing::debug!("实际发送请求体: {}", body);

        let base = self
            .client_for(credentials)?
            .post(&url)
            .body(body)
            .header("content-type", endpoint.content_type())
            .header("Connection", "close");
        let request = endpoint
            .decorate_api(base, rctx)
            .build()
            .map_err(|e| anyhow::anyhow!("构建请求失败: {}", e))?;

        if tracing::enabled!(tracing::Level::DEBUG) {
            for (key, value) in request.headers() {
                tracing::debug!("  header {}: {}", key, value.to_str().unwrap_or("<binary>"));
            }
        }

        self.client_for(credentials)?
            .execute(request)
            .await
            .map_err(Into::into)
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(凭据数量 × 每凭据重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        // 重试预算按当前请求所属分组的账号数计算，避免小分组按全局账号数获得过多无效重试
        let total_credentials = self.token_manager.total_count_in_group(group).max(1);
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 尝试从请求体中提取模型信息
        let model = Self::extract_model_from_request(request_body);
        // 全池冷却的内部等待预算按「一次调用」计量并在各次重试间共享，
        // 否则每轮重试各自新建预算会把累计等待放大到 重试轮数 × 预算。
        let mut wait_budget = self.token_manager.new_acquire_wait_budget();

        for attempt in 0..max_retries {
            let attempt_start = Instant::now();
            // 获取调用上下文（绑定 index、credentials、token）
            let mut ctx = match self
                .token_manager
                .acquire_context_with_budget(model.as_deref(), group, &mut wait_budget)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    if is_rate_limit_error(&e) {
                        Self::emit_attempt(
                            sink, attempt, 0, "", None, outcome::TRANSIENT,
                            Some(&e.to_string()), attempt_start,
                        );
                        return Err(e);
                    }
                    if let Some(rate_limit) = take_rate_limit_error(&mut last_error) {
                        return Err(rate_limit);
                    }
                    Self::emit_attempt(
                        sink, attempt, 0, "", None, outcome::UNKNOWN,
                        Some(&e.to_string()), attempt_start,
                    );
                    last_error = Some(e);
                    continue;
                }
            };

            // 确保 Enterprise / IdC 账号的真实 profileArn 已解析（流式端点强制要求）
            if let Err(e) = self.ensure_profile_arn(&mut ctx).await {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    "",
                    None,
                    outcome::TRANSIENT,
                    Some(&e.to_string()),
                    attempt_start,
                );
                return Err(e);
            }

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let mut endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    Self::emit_attempt(
                        sink,
                        attempt,
                        ctx.id,
                        "",
                        None,
                        outcome::UNKNOWN,
                        Some(&e.to_string()),
                        attempt_start,
                    );
                    last_error = Some(e);
                    self.token_manager
                        .report_failure_for_request(ctx.id, model.as_deref(), group);
                    continue;
                }
            };
            let mut endpoint_name = endpoint.name();

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let response = match self
                .execute_endpoint_api_request(
                    endpoint.as_ref(),
                    ctx.id,
                    &ctx.credentials,
                    request_body,
                    &rctx,
                )
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "API 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    Self::emit_attempt(
                        sink,
                        attempt,
                        ctx.id,
                        endpoint_name,
                        None,
                        outcome::NETWORK_ERROR,
                        Some(&e.to_string()),
                        attempt_start,
                    );
                    // 网络错误通常是上游/链路瞬态问题，默认不"禁用凭据"或"切换凭据"
                    // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）。
                    //
                    // 例外：凭据有专属代理时，故障多半出在该代理上，重试同一凭据无意义，
                    // 计入失败以换下一个。直连或仅全局代理时切换凭据不解决问题，保持重试。
                    let has_own_proxy = ctx
                        .credentials
                        .proxy_url
                        .as_deref()
                        .is_some_and(|u| !u.trim().is_empty());
                    if has_own_proxy {
                        tracing::warn!("凭据 #{} 有专属代理且网络请求失败，跳过该凭据", ctx.id);
                        self.token_manager
                            .report_failure_for_request(ctx.id, model.as_deref(), group);
                    }

                    last_error = Some(e);
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let mut status = response.status();
            let mut rate_limit_error = (status.as_u16() == 429)
                .then(|| UpstreamRateLimitError::from_headers(response.headers()));

            // 成功响应
            if status.is_success() {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::SUCCESS,
                    None,
                    attempt_start,
                );
                self.token_manager
                    .report_success_for_request(ctx.id, model.as_deref());
                return Ok(KiroCallResult {
                    response,
                    credential_id: ctx.id,
                });
            }

            // Successful streaming bodies are returned untouched above. Keep
            // the primary response optional so a 429 can be dropped
            // immediately instead of waiting for its body before failover.
            let mut primary_response = Some(response);
            let mut error_body: Option<String> = None;
            let mut fallback_exhausted = false;

            // runtime.kiro.dev and *.amazonaws.com use independent limit
            // buckets. A 429 gets exactly one same-account retry on the paired
            // bucket before account cooldown/credential failover sees it.
            if status.as_u16() == 429
                && let Some(fallback_name) = endpoint.fallback_name()
                && let Some(fallback_endpoint) = self.endpoints.get(fallback_name).cloned()
            {
                // Dropping reqwest::Response cancels/releases its body. The
                // fallback first-byte budget therefore starts immediately
                // after the primary response headers arrive.
                drop(primary_response.take());
                error_body = Some(format!("Quota exhausted on {}", endpoint.display_name()));
                tracing::warn!(
                    "[Kiro] Endpoint {} 429, retry on fallback {} (same account)",
                    endpoint.display_name(),
                    fallback_endpoint.display_name()
                );

                let fallback_result = tokio::time::timeout(
                    FALLBACK_FIRST_BYTE_TIMEOUT,
                    self.execute_endpoint_api_request(
                        fallback_endpoint.as_ref(),
                        ctx.id,
                        &ctx.credentials,
                        request_body,
                        &rctx,
                    ),
                )
                .await;

                match fallback_result {
                    Ok(Ok(fallback_response)) => {
                        let fallback_status = fallback_response.status();
                        if fallback_status.is_success() {
                            tracing::info!(
                                "[Kiro] Fallback endpoint {} succeeded after {} 429",
                                fallback_endpoint.display_name(),
                                endpoint.display_name()
                            );
                            Self::emit_attempt(
                                sink,
                                attempt,
                                ctx.id,
                                fallback_endpoint.name(),
                                Some(fallback_status.as_u16()),
                                outcome::SUCCESS,
                                None,
                                attempt_start,
                            );
                            self.token_manager
                                .report_success_for_request(ctx.id, model.as_deref());
                            return Ok(KiroCallResult {
                                response: fallback_response,
                                credential_id: ctx.id,
                            });
                        }

                        let fallback_rate_limit = (fallback_status.as_u16() == 429).then(|| {
                            UpstreamRateLimitError::from_headers(fallback_response.headers())
                        });
                        let fallback_body = fallback_response.text().await.unwrap_or_default();

                        if matches!(fallback_status.as_u16(), 401 | 403) {
                            // Auth/suspension evidence from the actual request
                            // must not be hidden behind the primary 429.
                            endpoint = fallback_endpoint;
                            endpoint_name = endpoint.name();
                            status = fallback_status;
                            rate_limit_error = fallback_rate_limit;
                            error_body = Some(fallback_body);
                        } else {
                            fallback_exhausted = true;
                            tracing::warn!(
                                "[Kiro] Fallback endpoint {} also failed ({}), keep quota semantics",
                                fallback_endpoint.display_name(),
                                fallback_status.as_u16()
                            );
                        }
                    }
                    Ok(Err(error)) => {
                        fallback_exhausted = true;
                        tracing::warn!("[Kiro] Fallback endpoint request error: {}", error);
                    }
                    Err(_) => {
                        fallback_exhausted = true;
                        tracing::warn!(
                            "[Kiro] Fallback endpoint request error: first-byte timeout ({}ms)",
                            FALLBACK_FIRST_BYTE_TIMEOUT.as_millis()
                        );
                    }
                }
            }

            let body = match error_body {
                Some(body) => body,
                None => match primary_response.take() {
                    Some(response) => response.text().await.unwrap_or_default(),
                    None => format!("Quota exhausted on {}", endpoint.display_name()),
                },
            };

            // 402 Payment Required 且额度用尽：禁用凭据并故障转移
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                tracing::warn!(
                    "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::QUOTA_EXHAUSTED,
                    Some(&body),
                    attempt_start,
                );

                let has_available = self.token_manager.report_quota_exhausted_for_request(
                    ctx.id,
                    model.as_deref(),
                    group,
                );
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 400 Bad Request - 请求问题，重试/切换凭据无意义
            if status.as_u16() == 400 {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(400),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
            if matches!(status.as_u16(), 401 | 403) {
                // 403 + 明确封禁文案：账号被封禁，立即禁用且不参与自愈（受配置开关控制）
                if status.as_u16() == 403
                    && self.token_manager.get_suspended_detection_enabled()
                    && endpoint.is_account_suspended(&body)
                {
                    tracing::error!(
                        "API 请求失败（账号被封禁，禁用凭据 #{} 并切换，尝试 {}/{}）: {} {}",
                        ctx.id,
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    Self::emit_attempt(
                        sink,
                        attempt,
                        ctx.id,
                        endpoint_name,
                        Some(403),
                        outcome::ACCOUNT_SUSPENDED,
                        Some(&body),
                        attempt_start,
                    );

                    let has_available = self.token_manager.report_suspended_for_request(
                        ctx.id,
                        model.as_deref(),
                        group,
                    );
                    if !has_available {
                        anyhow::bail!(
                            "{} API 请求失败（所有凭据已用尽）: {} {}",
                            api_type,
                            status,
                            body
                        );
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（账号封禁）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    continue;
                }

                tracing::warn!(
                    "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::AUTH_FAILED,
                    Some(&body),
                    attempt_start,
                );

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if Self::handle_force_refresh_result(
                        self.token_manager.force_refresh_token_for(ctx.id).await,
                    )? {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available =
                    self.token_manager
                        .report_failure_for_request(ctx.id, model.as_deref(), group);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 429 + suspicious activity = 账号级临时风控
            // 仅当前凭据被针对，故障转移到其它凭据可立即恢复（受配置开关控制）。
            if status.as_u16() == 429
                && self.token_manager.get_account_throttle_failover()
                && (fallback_exhausted || endpoint.is_account_throttled(&body))
            {
                let cooldown_secs = self
                    .token_manager
                    .get_account_throttle_cooldown_secs()
                    .max(1);
                let cooldown = std::time::Duration::from_secs(cooldown_secs);
                tracing::warn!(
                    "API 请求失败（账号级风控，凭据 #{} 冷却 {}s 并切换，尝试 {}/{}）: {}",
                    ctx.id,
                    cooldown_secs,
                    attempt + 1,
                    max_retries,
                    body
                );

                let remaining = self.token_manager.report_account_throttled_for_request(
                    ctx.id,
                    cooldown,
                    model.as_deref(),
                    group,
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(429),
                    outcome::ACCOUNT_THROTTLED,
                    Some(&body),
                    attempt_start,
                );
                // 账号级风控通常不返回 Retry-After；此时使用本地实际冷却时间，
                // 让下游网关在同一时段内也停止调度该虚拟账号。
                let (rate_limit_error, must_wait_for_upstream) =
                    account_rate_limit_with_fallback(rate_limit_error, cooldown_secs);

                // 上游给出明确等待时间时必须立即交给客户端遵守，不能在同一请求中
                // 提前换号重试。无有效 Retry-After 时仍允许按既有策略故障转移。
                if must_wait_for_upstream && !fallback_exhausted {
                    return Err(rate_limit_error.into());
                }

                if remaining == 0 {
                    return Err(rate_limit_error.into());
                }
                last_error = Some(rate_limit_error.into());
                continue;
            }

            // 客户端请求格式错误（messages 数组违反协议）：根因在调用方，重试无意义
            // 上游常以 5xx 返回，必须在下方"瞬态错误重试"分支之前拦截，否则会被当作
            // 上游故障重试 max_retries 次，把一个坏请求放大成多次 503（503 风暴）。
            // 直接终止：不重试、不切换凭据、不计入凭据失败。
            if endpoint.is_client_validation_error(&body) {
                tracing::warn!(
                    "API 请求失败（客户端请求格式错误，不重试）: {} {}",
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 524 / gateway timeout：上游边缘层超时，继续在本次请求内重试通常只会
            // 放大客户端等待时间和 Claude 端 Retrying 轮数；快速返回，让客户端下一次调用
            // 重新建连。
            if status.as_u16() == 524 || endpoint.is_gateway_timeout(&body) {
                tracing::warn!("API 请求失败（上游网关超时，不重试）: {} {}", status, body);
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::TRANSIENT,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 429/408/5xx - 瞬态上游错误：重试但不禁用或切换凭据
            // （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::TRANSIENT,
                    Some(&body),
                    attempt_start,
                );
                last_error = if let Some(rate_limit) = rate_limit_error {
                    if !rate_limit.should_retry_locally() {
                        return Err(rate_limit.into());
                    }
                    Some(rate_limit.into())
                } else {
                    Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ))
                };

                // 给当前凭据打一个短冷却，让下一轮取号换到别的凭据。
                //
                // 不这样做的话，瞬态分支只重试、不改动任何调度状态，下一轮
                // select_next_credential 的输入完全没变、必然重新选中同一个凭据：
                // 实测一次请求 4 跳全撞在同一个号的同一个桶上，而池里另一个号有
                // 充足额度却一次没被用到。换号后端点也随之重选，等于同时换了桶。
                //
                // 冷却时长取本次退避时长：保证这一跳之后该凭据被跳过，且不会在
                // 请求结束后继续影响调度。
                let backoff = if status.as_u16() == 429 {
                    Self::retry_delay_throttle(attempt)
                } else {
                    Self::retry_delay(attempt)
                };

                // 只有确实存在别的可用凭据时才打冷却。否则打了冷却会让下一轮取号
                // 失败（全池不可用 → 立即 429），把本可同号重试的请求直接判死：
                // 实测单凭据池下这样做会让「未联系上游就被本地拒绝」的比例从 0%
                // 涨到 48%。内部等待预算（默认 3s）也吸收不了 429 退避的累计时长
                // （1+2+4=7s），指望它兜住是错的。
                let can_switch = self.token_manager.has_other_available_for_request(
                    ctx.id,
                    model.as_deref(),
                    group,
                );
                if can_switch {
                    self.token_manager.report_transient_throttle_for_request(
                        ctx.id,
                        backoff,
                        model.as_deref(),
                        group,
                    );
                }

                if attempt + 1 < max_retries {
                    // 能换号就立即重试：退避是为了给「同一个」凭据的配额留恢复时间，
                    // 换到别的凭据没有等待的理由。无处可换时照常退避。
                    if !can_switch {
                        sleep(backoff).await;
                    }
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
            if status.is_client_error() {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 兜底：当作可重试的瞬态错误处理（不切换凭据）
            tracing::warn!(
                "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                attempt + 1,
                max_retries,
                status,
                body
            );
            Self::emit_attempt(
                sink,
                attempt,
                ctx.id,
                endpoint_name,
                Some(status.as_u16()),
                outcome::UNKNOWN,
                Some(&body),
                attempt_start,
            );
            last_error = Some(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            ));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 所有重试都失败
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }

    /// 向 trace sink 上报一跳结果（sink 为 None 时无开销）
    #[allow(clippy::too_many_arguments)]
    fn emit_attempt(
        sink: Option<&dyn TraceSink>,
        attempt: usize,
        credential_id: u64,
        endpoint: &str,
        http_status: Option<u16>,
        outcome: &str,
        error_body: Option<&str>,
        started: Instant,
    ) {
        let Some(sink) = sink else { return };
        sink.on_attempt(TraceAttempt {
            attempt: attempt as u32,
            credential_id,
            endpoint: endpoint.to_string(),
            http_status,
            outcome: outcome.to_string(),
            error_snippet: error_body.and_then(truncate_snippet),
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }

    /// 从请求体中提取模型信息
    ///
    /// 尝试解析 JSON 请求体，提取 conversationState.currentMessage.userInputMessage.modelId
    fn extract_model_from_request(request_body: &str) -> Option<String> {
        use serde_json::Value;

        let json: Value = serde_json::from_str(request_body).ok()?;

        json.get("conversationState")?
            .get("currentMessage")?
            .get("userInputMessage")?
            .get("modelId")?
            .as_str()
            .map(|s| s.to_string())
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 429 限流专用退避：比通用退避更长。
    ///
    /// 上游 429（SERVICE_REQUEST_RATE_EXCEEDED）是账号级速率配额耗尽，需要更长
    /// 时间恢复；用通用的 ≤2s 快速退避只会让请求在配额恢复前反复撞墙、持续触顶。
    /// 这里 base 1s、封顶 8s，给账号配额留出恢复窗口。
    fn retry_delay_throttle(attempt: usize) -> Duration {
        const BASE_MS: u64 = 1_000;
        const MAX_MS: u64 = 8_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 返回是否刷新成功；类型化刷新 429 原样传播，其他刷新失败交回调用方按认证失败处理。
    fn handle_force_refresh_result(result: anyhow::Result<()>) -> anyhow::Result<bool> {
        match result {
            Ok(()) => Ok(true),
            Err(error) if is_rate_limit_error(&error) => Err(error),
            Err(_) => Ok(false),
        }
    }
}

fn is_rate_limit_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<UpstreamRateLimitError>().is_some()
}

fn take_rate_limit_error(last_error: &mut Option<anyhow::Error>) -> Option<anyhow::Error> {
    if last_error
        .as_ref()
        .is_some_and(|error| error.downcast_ref::<UpstreamRateLimitError>().is_some())
    {
        last_error.take()
    } else {
        None
    }
}

/// 为账号风控 429 补齐本地冷却时间，并区分上游是否明确要求等待。
fn account_rate_limit_with_fallback(
    rate_limit: Option<UpstreamRateLimitError>,
    cooldown_secs: u64,
) -> (UpstreamRateLimitError, bool) {
    let must_wait_for_upstream = rate_limit
        .as_ref()
        .is_some_and(|error| !error.should_retry_locally());
    let error = match rate_limit {
        Some(error) if error.retry_after().is_some() => error,
        _ => UpstreamRateLimitError::new(Some(cooldown_secs.to_string())),
    };
    (error, must_wait_for_upstream)
}

#[cfg(test)]
mod rate_limit_tests {
    use super::*;
    use axum::{Router, http::StatusCode, routing::post};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockEndpoint {
        name: &'static str,
        url: String,
        fallback: Option<&'static str>,
    }

    impl KiroEndpoint for MockEndpoint {
        fn name(&self) -> &'static str {
            self.name
        }

        fn fallback_name(&self) -> Option<&'static str> {
            self.fallback
        }

        fn api_url(&self, _ctx: &RequestContext<'_>) -> String {
            self.url.clone()
        }

        fn mcp_url(&self, _ctx: &RequestContext<'_>) -> String {
            self.url.clone()
        }

        fn decorate_api(
            &self,
            req: reqwest::RequestBuilder,
            ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req.header("Authorization", format!("Bearer {}", ctx.token))
        }

        fn decorate_mcp(
            &self,
            req: reqwest::RequestBuilder,
            ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            self.decorate_api(req, ctx)
        }

        fn transform_api_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
            body.to_string()
        }
    }

    async fn mock_upstreams(
        fallback_status: StatusCode,
        fallback_body: &'static str,
    ) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let primary_count = Arc::new(AtomicUsize::new(0));
        let fallback_count = Arc::new(AtomicUsize::new(0));
        let primary_counter = Arc::clone(&primary_count);
        let fallback_counter = Arc::clone(&fallback_count);
        let app = Router::new()
            .route(
                "/primary",
                post(move || {
                    let counter = Arc::clone(&primary_counter);
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            r#"{"reason":"USER_REQUEST_RATE_EXCEEDED"}"#,
                        )
                    }
                }),
            )
            .route(
                "/fallback",
                post(move || {
                    let counter = Arc::clone(&fallback_counter);
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        (fallback_status, fallback_body)
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{}", address), primary_count, fallback_count)
    }

    fn provider_for_mock_upstreams(base_url: &str) -> KiroProvider {
        let credentials = KiroCredentials {
            id: Some(1),
            auth_method: Some("api_key".to_string()),
            kiro_api_key: Some("ksk_test".to_string()),
            profile_arn: Some(
                "arn:aws:codewhisperer:us-east-1:123456789012:profile/test".to_string(),
            ),
            endpoint: Some("primary".to_string()),
            ..KiroCredentials::default()
        };
        let mut config = crate::model::config::Config::default();
        config.account_throttle_cooldown_secs = 300;
        let token_manager =
            Arc::new(MultiTokenManager::new(config, vec![credentials], None, None, false).unwrap());
        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "primary".to_string(),
            Arc::new(MockEndpoint {
                name: "primary",
                url: format!("{}/primary", base_url),
                fallback: Some("fallback"),
            }),
        );
        endpoints.insert(
            "fallback".to_string(),
            Arc::new(MockEndpoint {
                name: "fallback",
                url: format!("{}/fallback", base_url),
                fallback: None,
            }),
        );
        KiroProvider::with_proxy(token_manager, None, endpoints, "primary".to_string())
    }

    const TEST_REQUEST_BODY: &str = r#"{
        "conversationState": {
            "currentMessage": {"userInputMessage": {"modelId": "claude-sonnet-4.6"}}
        }
    }"#;

    #[tokio::test]
    async fn primary_429_uses_same_account_fallback_without_outer_retry() {
        let (base_url, primary_count, fallback_count) =
            mock_upstreams(StatusCode::OK, "fallback-ok").await;
        let provider = provider_for_mock_upstreams(&base_url);

        let result = provider
            .call_api_stream(TEST_REQUEST_BODY, None, None)
            .await
            .unwrap();
        assert_eq!(result.credential_id, 1);
        assert_eq!(result.response.text().await.unwrap(), "fallback-ok");
        assert_eq!(primary_count.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn both_buckets_429_cool_down_after_one_fallback_request() {
        let (base_url, primary_count, fallback_count) = mock_upstreams(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"reason":"SERVICE_REQUEST_RATE_EXCEEDED"}"#,
        )
        .await;
        let provider = provider_for_mock_upstreams(&base_url);

        let error = match provider
            .call_api_stream(TEST_REQUEST_BODY, None, None)
            .await
        {
            Ok(_) => panic!("both rate-limit buckets should propagate a typed 429"),
            Err(error) => error,
        };
        let rate_limit = error
            .downcast_ref::<UpstreamRateLimitError>()
            .expect("the outer layer must retain rate-limit semantics");
        assert_eq!(rate_limit.retry_after(), Some("300"));
        assert_eq!(primary_count.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mcp_validation_treats_caller_accepted_errors_as_success() {
        fn is_no_results(error: &anyhow::Error) -> bool {
            error.to_string().contains("Tool returned no results")
        }

        let valid: anyhow::Result<()> = Ok(());
        let no_results: anyhow::Result<()> = Err(anyhow::anyhow!("Tool returned no results"));
        let malformed: anyhow::Result<()> = Err(anyhow::anyhow!("invalid JSON"));

        assert_eq!(
            KiroProvider::mcp_validation_outcome(&valid, is_no_results),
            outcome::SUCCESS
        );
        assert_eq!(
            KiroProvider::mcp_validation_outcome(&no_results, is_no_results),
            outcome::SUCCESS
        );
        assert_eq!(
            KiroProvider::mcp_validation_outcome(&malformed, is_no_results),
            outcome::UNKNOWN
        );
    }

    #[test]
    fn preserves_typed_rate_limit_when_later_credential_selection_fails() {
        let mut last_error = Some(anyhow::Error::new(UpstreamRateLimitError::new(Some(
            "45".to_string(),
        ))));

        // A concurrent request may cool the final credential before the next acquisition.
        // The earlier upstream 429 must win over the later generic selection failure.
        let returned = take_rate_limit_error(&mut last_error)
            .unwrap_or_else(|| anyhow::anyhow!("所有凭据均已禁用"));

        let rate_limit = returned
            .downcast_ref::<UpstreamRateLimitError>()
            .expect("应保留最初的类型化 429");
        assert_eq!(rate_limit.retry_after(), Some("45"));
        assert!(last_error.is_none());
    }

    #[test]
    fn does_not_relabel_generic_error_as_rate_limit() {
        let mut last_error = Some(anyhow::anyhow!("所有凭据均已禁用"));
        assert!(take_rate_limit_error(&mut last_error).is_none());
        assert!(last_error.is_some());
    }

    #[test]
    fn account_rate_limit_uses_cooldown_when_retry_after_is_missing() {
        let (error, must_wait) =
            account_rate_limit_with_fallback(Some(UpstreamRateLimitError::new(None)), 300);

        assert_eq!(error.retry_after(), Some("300"));
        assert!(!must_wait, "无上游等待值时仍可按账号冷却策略故障转移");
    }

    #[test]
    fn account_rate_limit_honors_explicit_upstream_retry_after() {
        let (error, must_wait) = account_rate_limit_with_fallback(
            Some(UpstreamRateLimitError::new(Some("90".to_string()))),
            300,
        );

        assert_eq!(error.retry_after(), Some("90"));
        assert!(must_wait, "上游明确要求等待时不得在内部提前重试");
    }

    #[test]
    fn force_refresh_rate_limit_is_propagated_instead_of_counted_as_auth_failure() {
        let error = anyhow::Error::new(UpstreamRateLimitError::new(Some("60".to_string())));
        let returned = KiroProvider::handle_force_refresh_result(Err(error))
            .expect_err("强制刷新 429 应立即传播");

        let rate_limit = returned
            .downcast_ref::<UpstreamRateLimitError>()
            .expect("应保留类型化 429");
        assert_eq!(rate_limit.retry_after(), Some("60"));
    }

    #[test]
    fn generic_force_refresh_failure_remains_an_auth_failure() {
        let outcome = KiroProvider::handle_force_refresh_result(Err(anyhow::anyhow!(
            "invalid refresh token",
        )))
        .unwrap();
        assert!(!outcome);
    }

    #[test]
    fn current_acquire_rate_limit_is_detected_before_outer_retry() {
        let error = anyhow::Error::new(UpstreamRateLimitError::new(Some("30".to_string())));
        assert!(is_rate_limit_error(&error));
    }
}
