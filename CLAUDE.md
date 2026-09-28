# kiro.rs (zuchengchen fork)

## Fork 关系：三层，别搞混

```
hank9999/kiro.rs        原始项目。版本号是日期式（v2026.3.1）。
        ↓               落后我们约 36k 行，永远不要直接 merge 它。
ZyphrZero/kiro.rs       我们真正的上游。0.x 语义版本（v0.9.0）。
        ↓               = git remote `upstream`
zuchengchen/kiro.rs     本仓库。= git remote `origin`
```

我们的 `0.x` 版本号继承自 ZyphrZero，不是 hank9999。历史上曾把 hank9999
误认为上游，导致对「上游是哪个版本」判断错误——认准 `upstream` remote 即可。

## 分支

| 分支 | 用途 | 跟踪 |
|---|---|---|
| `master` | **上游纯镜像**，不放任何定制 | `upstream/master` |
| `main-czc` | **定制主分支**，生产部署来源 | `origin/main-czc` |

`git diff master main-czc -- src/` 就是我们全部的定制（相对 v0.9.0 约 +4,200 / −400 行，27 个文件）。

## 同步上游

```bash
git fetch upstream
git log --oneline main-czc..upstream/master     # 先看有什么
git merge-tree --write-tree --name-only main-czc upstream/master   # 空跑：冲突文件列表，不动工作区

git checkout master && git merge --ff-only upstream/master && git push origin master

git checkout main-czc && git merge upstream/master
# 解冲突 → cargo test → cargo build --release → 前端 tsc + vite build
git push origin main-czc
```

**永远 merge，不要 rebase。** 提交已推送且被 `deployment-*.json` 按 commit hash
引用；rebase 会重写历史、使部署记录失效，也让 git 无法识别哪些上游提交已合过。

### 常见冲突点

定制集中在上游的活跃区，这几个文件几乎每次都冲突：

- `src/kiro/token_manager.rs` — 选号策略 + 取号路径（我们的限流内部等待在这里）
- `src/kiro/provider.rs` — 重试循环、429 换桶
- `src/anthropic/responses.rs` — 流式分流
- `src/anthropic/cache_metering.rs` — 上游反复重写计量；我们的固定比例已移到
  `fixed_cache_ratio.rs`，该文件与上游保持一致，冲突时直接取上游版本

**没报冲突不等于合对了。** v0.9.0 合并时：上游删掉的函数仍被我们调用（编译失败）、
断言旧语义的测试被静默合入（测试失败）、上游新加的 `bind_session` 没覆盖我们的换桶
成功路径（静默缺失）。解完冲突标记后跑全量测试，并逐个复核上游新增的调用点是否也要
走我们的定制。

解冲突时必须守住的三条不变量（第 2 条的流式 web_search 入口见下方定制清单后的说明）：

1. **不能持 `parking_lot` guard 跨 `.await`**。guard 是 `!Send`，会让 handler
   future 变成 `!Send`（编译不过），强行绕过则阻塞 OS 线程、卡住所有需要
   `entries` 锁的路径（含写回冷却状态的 `report_*`），全池死锁。
2. **错误响应必须保留 `Retry-After`**。丢了客户端会立即重试，一次冷却放大成
   持续 429 风暴（曾实测 8 天 19,454 次伪 429，单次冷却连带拒绝 635 个请求）。
3. **`AcquireWaitBudget` 必须由最外层调用方创建并跨重试共享**。每次取号各自
   新建预算会把单请求累计等待放大到 `轮数 × 预算`（WebSearch 6 轮 × 4 次重试）。
   上游新增的取号入口（如 v0.9.0 的 `acquire_context_routed`）要改成接收调用方的
   预算，不能照搬上游签名。一次客户端请求会多次调用上游的路径，要在最外层建一份预算，
   经 `call_api*_with_budget` / `call_mcp` 往下传。目前有 web_search 多轮循环
   （含 MCP 搜索）和 Codex 压缩的溢出重试；以后新增这类路径也照此处理。

## 版本号：`0.9.0.1` = 上游基线 + 定制迭代号

第四段是本仓库的定制迭代号，前三段永远是我们所基于的上游基线。这样上游发到
`0.9.1` 也不会和我们的编号撞车。

**Cargo 不接受四段版本号**（`0.9.0.1` 直接报 `unexpected character '.' after
patch version number`），所以三个版本文件里写的是 semver build metadata 形式：

| 文件 | 值 |
|---|---|
| `Cargo.toml` | `0.9.0+3` |
| `Cargo.lock`（kiro-rs 自身条目） | `0.9.0+3` |
| `admin-ui/package.json` | `0.9.0+3` |

`display_version()`（`src/admin/service.rs`）在对外暴露时把 `+1` 还原成 `.1`，
Admin UI 显示 `v0.9.0.1`。`parse_semver_core()` 返回 `[u32; 4]`，两种形式都解析
成同一个 `[0,9,0,1]` —— 显示形式会回流进 `compare_semver`（`current_version`
已是显示形式），两者必须一致，否则第四段被 `splitn` 吞掉。

更新提示的语义因此是对的：我们 `[0,9,0,1]` > 上游 `0.9.0` = `[0,9,0,0]`，不提示；
上游发 `0.9.1`/`0.9.8`/`0.10.0` 时前三段更大，正常提示。上游历史 tag 全是纯三段
（`v0.7.0` … `v0.9.0`），从未带 `+`，所以第四段解析为 0 不会误判。

**升级定制迭代号时三个文件一起改**，只改 `Cargo.toml` 会让 `Cargo.lock` 和
`package.json` 落后。合并上游时这三行都会冲突：保留上游的三段基线，把 `+N` 接
回去；基线变了（如上游到 0.9.0）则迭代号归 1。

image tag 和 `deployment-*.json` 沿用同一个编号（`kiro-rs:0.9.0.1`）。历史上
`0.8.1`–`0.8.8` 那批部署记录是旧的两段式本地编号，留着不动，它们是历史追溯点。

## Tag 约定

- 只用 `deploy/<version>`，对应 `/home/czc/kiro-rs/deployment-*.json` 的部署点。
- **不要 `git push --tags`。** 上游 tag 会污染 `origin`。已设
  `remote.upstream.tagOpt=--no-tags` 阻止拉取，推送时显式指定 tag 名。

## 定制清单（相对上游 v0.9.0）

| 提交 | 内容 |
|---|---|
| `6924c26` | 端点分桶 + 429 同账号换桶 failover |
| `0ace7b9` | 额度感知选号（现降级为同 priority 内的 tie-break；会话粘性命中时跳过） |
| `aedc64c` | 全池冷却内部等待（`acquireWaitBudgetMs`）+ `agentMode` |
| `a84e02e` | Admin UI 区分「同凭据换桶」与「转其他凭据」救回 |
| `bd53626` | 按账号周期积分上限参与调度（粘性选号同样受限） |
| v0.9.0 合并 | Claude 固定 90% 缓存命中（`src/anthropic/fixed_cache_ratio.rs`），其他模型走上游计量；取代 `a90235e` / `6c26708` 的全模型固定比例 |
| `0338d8b` | 凭据 ID 跨重启单调（`src/kiro/credential_id_watermark.rs`）：删号 + 重启不再把旧 ID 分给新账号 |
| `9065d67` | API Key / PKCE 用系统熵源生成（`src/common/secure_random.rs`），不再用 `fastrand` |

2026-09-28 全项目审查的其余修复（`8085197..HEAD`）都是对已有代码的缺陷修正，
没有新功能，逐条见各自的提交说明。其中会影响合并判断的两处上游代码改动：

- 流式 web_search agentic loop（`websearch_loop.rs::run_web_search_loop`）在首次
  上游尝试成功前不提交 HTTP 200，靠 `RequestTracer::subscribe_first_success`。
  上游若重写这个入口，要保住「首轮失败返回真实 429 + Retry-After」。
- `Config` 带 `#[serde(flatten)] unknown_fields`，保存时原样写回不认识的字段；
  上游给 `Config` 加字段不会冲突，但别删掉它（回滚安全靠它）。

写定制时的两个习惯，能显著减少下次冲突：

- **尽量隔离成新文件**，在上游函数里只留一个调用点。
- **一个提交只做一件事**。`aedc64c` 混了三件事，上游若只与其中一件冲突，
  没法单独处理。

## 构建与验证

`admin-ui/dist` 被 gitignore，但 RustEmbed 编译期需要它，所以裸 `cargo build`
在干净检出上会失败。先构建前端：

```bash
cd admin-ui && pnpm install --no-frozen-lockfile && ./node_modules/.bin/vite build
cd .. && cargo test          # 当前基线 849 通过
```

CI（`.github/workflows/test.yaml`）在 main-czc 的 push / PR 上跑 `bun install
--frozen-lockfile && bun run build`（含 `tsc -b`）、`cargo test --locked` 和
`cargo audit`。`build.yaml` 也跟 main-czc；`docker-build.yaml` 故意仍挂在废弃的
`dev-czc` 上——它会往 Docker Hub 推镜像，而生产镜像是本地从 `git archive` 构建的。
截至 2026-09-28 这个仓库在 GitHub 上一次 workflow run 都没有（Actions API
`total_count: 0`），推送后若仍无记录，检查仓库设置里 Actions 是否被禁用（fork
默认禁用）。

仓库跟踪的是 `admin-ui/bun.lock`；上面的 `pnpm install` 会生成一个未跟踪的
`admin-ui/pnpm-lock.yaml`，构建完删掉，别提交。

注意 `cargo fmt` 会格式化整个 crate，忽略文件参数——它会顺带重排大量无关文件，
提交前用 `git checkout --` 撤回那些噪音，保持 diff 干净。

## 生产部署

部署目录 `/home/czc/kiro-rs`（docker compose + 固定镜像 tag），流程和回滚约定见
该目录的 `README.local.md`。要点：候选端口验证 → 归档线上二进制到 `rollback/`
（保留最近两个 + SHA-256）→ 切换 → 验证 → 写 `deployment-<version>.json`。

**部署目录不是 git 仓库。** 在 `/home/czc/kiro-rs` 里跑 `git rev-parse HEAD` 会
静默失败（`fatal: not a git repository`），把空字符串写进 `deployment-*.json` 的
`sourceCommit` —— 而部署记录正是靠这个字段定位回滚版本。这个坑已经踩过两次。
先在源码目录取值再切过去：

```bash
COMMIT=$(git rev-parse HEAD)      # 在 /home/czc/projects/workging/kiro.rs
cd /home/czc/kiro-rs && ...       # 之后才用 $COMMIT
```

写完务必回读校验，别只看命令成功：

```bash
python3 -c "import json;d=json.load(open('deployment-<version>.json'));assert d['sourceCommit'];print(d['sourceCommit'])"
```

同类陷阱：验证「配置项生效」时不能只测默认值——那无法区分「配置真的被读取」和
「配置被忽略但默认值恰好正确」。要先显式设一个反常值确认行为改变（如把
`upstreamTimeoutSecs` 设成 3 秒看请求是否被切断），再恢复默认确认恢复正常。

Admin UI 的「可更新」提示查的是硬编码的 `ZyphrZero/kiro.rs` releases
（`src/admin/binary_update.rs`、`src/admin/service.rs`）。**不要点更新**：它会用
上游预编译二进制覆盖掉本地定制。容器化部署下自更新本身也不适用（重启即回滚到
镜像内版本，状态不可复现）。当前靠版本号追平上游来消除提示，上游发新版会再次出现。
