# kiro.rs (zuchengchen fork)

## Fork 关系：三层，别搞混

```
hank9999/kiro.rs        原始项目。版本号是日期式（v2026.3.1）。
        ↓               落后我们约 36k 行，永远不要直接 merge 它。
ZyphrZero/kiro.rs       我们真正的上游。0.x 语义版本（v0.8.0）。
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

`git diff master main-czc -- src/` 就是我们全部的定制（约 1,900 行 / 12 个文件）。

## 同步上游

```bash
git fetch upstream
git log --oneline main-czc..upstream/master     # 先看有什么

git checkout master && git merge --ff-only upstream/master && git push origin master

git checkout main-czc && git merge upstream/master
# 解冲突 → cargo test → cargo build --release → 前端 tsc + vite build
git push origin main-czc
```

**永远 merge，不要 rebase。** 提交已推送且被 `deployment-*.json` 按 commit hash
引用；rebase 会重写历史、使部署记录失效，也让 git 无法识别哪些上游提交已合过。

### 常见冲突点

定制集中在上游的活跃区，这三个文件几乎每次都冲突：

- `src/kiro/token_manager.rs` — 选号策略 + 取号路径（我们的限流内部等待在这里）
- `src/kiro/provider.rs` — 重试循环、429 换桶
- `src/anthropic/responses.rs` — 流式分流

解冲突时必须守住的三条不变量：

1. **不能持 `parking_lot` guard 跨 `.await`**。guard 是 `!Send`，会让 handler
   future 变成 `!Send`（编译不过），强行绕过则阻塞 OS 线程、卡住所有需要
   `entries` 锁的路径（含写回冷却状态的 `report_*`），全池死锁。
2. **错误响应必须保留 `Retry-After`**。丢了客户端会立即重试，一次冷却放大成
   持续 429 风暴（曾实测 8 天 19,454 次伪 429，单次冷却连带拒绝 635 个请求）。
3. **`AcquireWaitBudget` 必须由最外层调用方创建并跨重试共享**。每次取号各自
   新建预算会把单请求累计等待放大到 `轮数 × 预算`（WebSearch 6 轮 × 4 次重试）。

## 版本号：`0.8.0.8` = 上游基线 + 定制迭代号

第四段是本仓库的定制迭代号，前三段永远是我们所基于的上游基线。这样上游发到
`0.8.8` 也不会和我们的编号撞车。

**Cargo 不接受四段版本号**（`0.8.0.8` 直接报 `unexpected character '.' after
patch version number`），所以三个版本文件里写的是 semver build metadata 形式：

| 文件 | 值 |
|---|---|
| `Cargo.toml` | `0.8.0+8` |
| `Cargo.lock`（kiro-rs 自身条目） | `0.8.0+8` |
| `admin-ui/package.json` | `0.8.0+8` |

`display_version()`（`src/admin/service.rs`）在对外暴露时把 `+8` 还原成 `.8`，
Admin UI 显示 `v0.8.0.8`。`parse_semver_core()` 返回 `[u32; 4]`，两种形式都解析
成同一个 `[0,8,0,8]` —— 显示形式会回流进 `compare_semver`（`current_version`
已是显示形式），两者必须一致，否则第四段被 `splitn` 吞掉。

更新提示的语义因此是对的：我们 `[0,8,0,8]` > 上游 `0.8.0` = `[0,8,0,0]`，不提示；
上游发 `0.8.1`/`0.8.8`/`0.9.0` 时前三段更大，正常提示。上游历史 tag 全是纯三段
（`v0.7.0` … `v0.8.0`），从未带 `+`，所以第四段解析为 0 不会误判。

**升级定制迭代号时三个文件一起改**，只改 `Cargo.toml` 会让 `Cargo.lock` 和
`package.json` 落后。合并上游时这三行都会冲突：保留上游的三段基线，把 `+N` 接
回去；基线变了（如上游到 0.9.0）则迭代号归 1。

image tag 和 `deployment-*.json` 沿用同一个编号（`kiro-rs:0.8.0.8`）。历史上
`0.8.1`–`0.8.8` 那批部署记录是旧的两段式本地编号，留着不动，它们是历史追溯点。

## Tag 约定

- 只用 `deploy/<version>`，对应 `/home/czc/kiro-rs/deployment-*.json` 的部署点。
- **不要 `git push --tags`。** 上游 tag 会污染 `origin`。已设
  `remote.upstream.tagOpt=--no-tags` 阻止拉取，推送时显式指定 tag 名。

## 定制清单（相对上游 v0.8.0）

| 提交 | 内容 |
|---|---|
| `6924c26` | 端点分桶 + 429 同账号换桶 failover |
| `0ace7b9` | 额度感知选号（现降级为同 priority 内的 tie-break） |
| `aedc64c` | 全池冷却内部等待（`acquireWaitBudgetMs`）+ `agentMode` |
| `a84e02e` | Admin UI 区分「同凭据换桶」与「转其他凭据」救回 |

写定制时的两个习惯，能显著减少下次冲突：

- **尽量隔离成新文件**，在上游函数里只留一个调用点。
- **一个提交只做一件事**。`aedc64c` 混了三件事，上游若只与其中一件冲突，
  没法单独处理。

## 构建与验证

`admin-ui/dist` 被 gitignore，但 RustEmbed 编译期需要它，所以裸 `cargo build`
在干净检出上会失败。先构建前端：

```bash
cd admin-ui && pnpm install --no-frozen-lockfile && ./node_modules/.bin/vite build
cd .. && cargo test          # 当前基线 712 通过
```

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
