# 项目实现记录

## 凭据元数据

- 凭据元数据使用可扩展对象 `metadata`，固定字段 `type` 只接受 `normal` 或 `boom`。
- 固定字段 `saleStatus` 表示账号在售状态，只接受 `not_for_sale`、`for_sale`、`sold`，
  旧凭据默认 `not_for_sale`；它和 `type` 都只做运营标记，不参与调度，批量编辑也应支持。
- 内置可选字段 `salePrice` 是非负数字，单位固定为 CNY；未设置时不展示，卡片按人民币
  格式显示，单个编辑和批量编辑都应支持设置或清除。
- 旧凭据或新增请求未携带 `metadata` 时，`metadata.type` 默认为 `normal`。
- 未识别的 metadata 扩展键必须在读取、Admin API 编辑和持久化过程中保留。
- `metadata.type` 当前仅用于运营标记，不参与优先级或负载均衡调度。
- metadata 字段定义使用标准 JSON Schema，保存于 `config.json` 的
  `credentialMetadataSchema`；设置页负责维护 key、值类型、默认值和枚举 value。
- 新增和编辑表单按 schema 动态渲染，后端按同一 schema 校验已登记字段，避免前后端规则漂移。
- 凭据卡片用两列表格、紧凑列表用单行摘要展示全部 metadata：优先使用字段 title
  和枚举 title，schema 外扩展字段回退显示原始 key，空值不占用卡片空间。
- Schema 字段可用 `x-css` 配置卡片值样式；前后端都必须拒绝外链、脚本表达式和
  会让内容脱离卡片边界的布局属性，避免自定义样式成为数据外传或界面劫持入口。

## Anthropic Prompt Cache 本地计量

- 本项目拿不到 Kiro 上游的真实 KV 张量缓存，只能在 provider 未返回精确 usage 时，
  按请求前缀模拟 Anthropic 的 `cache_creation_input_tokens` 与
  `cache_read_input_tokens`；该模拟不会降低上游推理成本。
- 只有顶层自动缓存或显式 block `cache_control` 才写入条目；自动断点落在最后一个
  合格 block，显式断点最多 4 个，读取从各断点回溯最多 20 个位置。生产计量入口在
  客户端完全未声明断点时补顶层 ephemeral（不写 ttl，走智能路由），与官方 automatic
  caching 同形。
- 连续 `tool_use` 和连续 `tool_result` 块分别只占一个回溯位置；断点只匹配此前真实
  写入的前缀，不为未声明的中间 block 建条目。
- TTL：显式 `ttl=5m` 记 300 秒、`ttl=1h` 记 3600 秒，原样尊重。未写 ttl 默认 5m；
  只有两条规则升 1h：R1 断点处累计前缀 ≥500K estimate token（生产 traces 回放的成本
  拐点，约 8% 轮次）；R2 请求无 session、按 Key 共享，断点在 tools/system 段且前缀
  ≥8192。升级取满足规则的最深断点，连同它之前未写 ttl 的断点一起升，遇显式 5m 停止，
  保证 1h 永远在 5m 之前。客户端显式写成 1h 在 5m（或未写）之后时整次不模拟（官方 400）。
  命中后按自身 TTL 滑动续期。
- usage 的 `cache_creation.ephemeral_{5m,1h}_input_tokens` 按 1h 断点覆盖到的前缀位置
  拆分（`CacheUsage::split_creation_ttl`）；上游精确 usage 与 credits 修正后的拆分同样
  适用。websearch 路径没有 CacheMeter 断点，写入全部记 5m。
- 缓存键覆盖完整结构化 block 与模型配置，并遵守 tools -> system -> messages 的
  失效层级；`tool_choice` 和图片存在性只使 message 前缀失效。
- 本地条目按客户端 Key 与 session 组合隔离；共享系统 Key 缺少 session 时禁用模拟，
  避免跨用户虚假命中。
- 代表性三轮夹具的热缓存读取率约 88.21%，仅用于验证目标工作负载；不得随机或
  硬编码制造 75%～90% 命中率。
- 按模型执行 512～4096 token 的最小缓存阈值；新断点在上游响应开始后才可被其他
  请求命中。JSON 解析会规范化对象键序，无法复现原始线序变化导致的失效。
- 字符串 message content 按单个 text block 计算签名。Claude Code 只给打断点的尾消息
  （含 `role:"system"` 中段系统消息）发数组 + `cache_control`，不再是尾部时退回字符串；
  不归一会让长会话每轮只命中 tools+system，其余整段误报为 cache_creation。
- 同一 session 并发的同前缀请求：后一个开始时前一个还没出首字，本地判为整段
  cache_creation，但 Kiro 上游已按命中扣费。`credit_cache_reconcile.rs` 在流结束后用
  `meteringEvent.credits` 判定（只覆盖 claude-opus-5.5，只向下修正 creation → read），
  trace 记 `usage_source = reconciled`。开关 `KIRO_RS_CREDIT_CACHE_RECONCILE` 默认关，
  必须等 Sub2API 支持 `message_delta.usage.usage_final` 后再开，否则 delta 的 0 被忽略、
  start 的 creation 残留，会重复计费。单价漂移用 `tools/credit_rate_fit.py` 重新拟合。
