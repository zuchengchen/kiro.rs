# 说人话：为什么 Sub2API 看不到「1 小时缓存创建」，以及这次怎么补上的

日期：2026-09-29  
范围：Sub2API 使用记录 ↔ kiro.rs Claude usage

---

## 先给结论

Sub2API 这边 **5 分钟 / 1 小时缓存创建的配置是齐全的**，后台表格、计费、价格表都认这两个档。

真正缺的是上游 kiro.rs 回 usage 的时候，**只给了「一共写了多少缓存」**，**没给「这是 5 分钟写的还是 1 小时写的」**。Sub2API 拿不到 1 小时那一栏，使用记录里的「1h」标记就永远出不来。

这次在 kiro.rs 把官方那份嵌套字段补上了。本地缓存本来就是按 1 小时记的，所以写入量全部记进 **1 小时缓存创建**。

---

## 1. 你做了什么？

查了两头：

- Sub2API：使用记录怎么读、怎么展示、怎么计费
- kiro.rs：Claude 调用结束时，usage JSON 到底长什么样

然后改了 kiro.rs：每次对外报 usage，都带上官方字段：

```json
"cache_creation": {
  "ephemeral_5m_input_tokens": 0,
  "ephemeral_1h_input_tokens": 1234
}
```

`1234` 就是原来的 `cache_creation_input_tokens`（缓存写入总量）。因为我们本地模拟的 TTL 是 1 小时，所以写进 1h，5m 为 0。

Sub2API **不用改代码**。它早就在等这个字段。

---

## 2. 为什么要这么做？

使用者在 Sub2API 后台看模型调用记录，能看到「写了缓存」，但看不到「这是 1 小时档」。

这不是 Sub2API 开关没开。它的表格是这样认的：

- 有缓存写入总量 → 显示写入数字
- **只有** `cache_creation_1h_tokens > 0` 才打橙色 **1h** 标记

这个 1h 数字，来自上游 usage 里的 `cache_creation.ephemeral_1h_input_tokens`。

kiro.rs 以前只回：

- `cache_creation_input_tokens`（总数）
- `cache_read_input_tokens`（命中）

没有嵌套的 5m / 1h。Sub2API 解析结果就是：总量有、1h 永远是 0。

---

## 3. 做了意味着什么？

对使用者：

- 走 kiro.rs 的 Claude 调用，只要有缓存**写入**，Sub2API 使用记录会出现 **1h** 标记
- 计费也会按 **1 小时写入价**（官方大约是输入价的 2 倍），不再因为缺明细而整笔按 5 分钟价（约 1.25 倍）兜底

对运维：

- Sub2API 里「Anthropic 缓存 TTL 注入」开关 **继续关着就对了**。那个开关是给 OAuth 账号改请求体的，而且会把 usage **计费改回 5 分钟**，和「要看到 1 小时」是反着的
- 渠道上可以不填自定义 1h 价，会用官方价格表里的 `cache_creation_input_token_cost_above_1hr`

这仍然是 **上报口径**，不降低 Kiro 真实 credit。

---

## 4. 之前是怎么样的？现在是怎么样的？

查了线上最近两天 Claude 使用记录（Sub2API 库）：

| | 之前（改 kiro.rs 之前） | 现在（字段补上并部署之后） |
|---|---|---|
| 有缓存写入的请求 | 很多（两万多笔） | 一样会有写入 |
| 其中带「5 分钟明细」 | 极少（约 38 笔，来自真 Anthropic，不是 kiro） | kiro 流量 5m 为 0 |
| 其中带「1 小时明细」 | **0 笔** | kiro 有写入的请求会记 1h |
| 后台「1h」角标 | 出不来 | 有写入就能出来 |
| 缺明细时的计费 | 整笔写入按 5 分钟价 | 按 1 小时价 |

Sub2API 自己的开关：

- `enable_anthropic_cache_ttl_1h_injection` = **false**（正确，别开）
- `rewrite_message_cache_control` = **false**

渠道自定义 5m/1h 写入价可以空着，走官方目录价。

---

## 5. 动了什么地方？

只动 **kiro.rs**，不动 Sub2API。

新增：

- `src/anthropic/usage_cache_breakdown.rs`  
  统一拼 usage JSON，带上 `cache_creation` 嵌套对象

接到这些出口，避免漏一个：

- 流式 `message_start` / `message_delta`
- 缓冲流式改 usage
- 非流式 `/v1/messages`
- web_search 的 JSON 和 SSE

测试：`cargo test` 852 通过。

---

## 6. 之前为什么没有？

两件事叠在一起：

1. **Anthropic 官方把「写入总量」和「5 分钟 / 1 小时拆分」分成两套字段。**  
   总量给所有客户端看；拆分给要分档计费的网关看。kiro.rs 一直只实现了总量。

2. **我们后来把本地模拟 TTL 改成了 1 小时**（不再用 5 分钟）。  
   模拟层知道这是 1 小时，但对外 JSON 没把这件事说出去。Sub2API 只能看见「写了缓存」，看不见「写的是 1 小时档」。

所以不是 Sub2API 漏了配置，是上游没把 1 小时这张标签递过去。

---

## 上线后怎么确认

在 Sub2API 后台打开一条 **有缓存写入** 的 Claude 记录：

1. 写入数字旁边应有橙色 **1h**
2. `cache_creation_1h_tokens` 应等于 `cache_creation_tokens`
3. `cache_creation_5m_tokens` 应为 0

走真 Anthropic OAuth 的账号，仍按官方返回的 5m/1h 拆分，不受这次 kiro.rs 改动影响。
