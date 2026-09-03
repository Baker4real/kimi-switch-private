# kimi-switch · Agent 快速规则

Kimi Code 多账号管理工具（Rust workspace）：GUI（`kimi-switch.exe`）+ CLI（`kimi-switch-cli.exe`）。

## 目录速记

```text
crates/core/             数据模型、Provider（activate/query_quota）、凭证仓库、额度快照、路径、显示名、自动切换决策
crates/cli/              kimi-switch-cli 命令行
crates/gui/              GUI 命令层（列表/切换/导入/设备码；不含 token DTO）
src-tauri/               Tauri 壳（kimi-switch.exe）
src-ui/                  纸感前端（总览 / 账号库 / 设置；api.ts / store / deviceAuth / membership / auto-swap）
scripts/generate-icons.mjs  图标确定性生成器（改图标改这里的常量后重跑，勿手改 icons/ 二进制）
crates/providers/common/ 文件型 OAuth 账号切换共享引擎（FileBlobProvider/FileBlobRuntime）
crates/providers/kimi/   Kimi 适配器（设备码授权、token 刷新协调、额度查询、官方昵称 /me、绑定网页登录）
```

## 规则

1. **改完必须验证**：Rust 走 `cargo check --workspace && cargo test --workspace`；改了 `src-ui/` 再加 `npx tsc --noEmit && npm test && npm run build`；通过后再 `cargo build --release`。
2. **凭证安全是最高优先级**：token 只能写用户本机私有文件；网络请求只允许发往 Kimi 官方域名（`auth.kimi.com` / `api.kimi.com` / `www.kimi.com`）。`www.kimi.com` 仅限「绑定网页登录」的会员到期读取路径（ADR-0004）。官方昵称只允许在既有导入/授权/额度流程上额外 `GET api.kimi.com/coding/v1/me`，禁止 `www.kimi.com/me`；禁止引入遥测。
3. **手动切换永远可用**：`swap` 不依赖额度查询、`/me` 和网络，原子写入 + 快照回滚的逻辑不得破坏。额度查询成功后不得 await `/me`（后台缓存即可）；`/me` 失败不失败主流程。额度耗尽自动切换默认关闭；判定只用已收集的额度快照、不新打网；执行只走既有 `activate`；不引入常驻 daemon。
4. **token 刷新必须与官方客户端协调**：有官方锁协议的平台只能复用官方锁，不得自创并行刷新机制；仅在无官方锁的平台组合（当前：Windows + TS 客户端），允许经「主动续期」开关（默认关）依赖官方 100ms 重读采纳机制做查询前轮换，前提是原子写回（先账号库后 live，写序见 ADR-0002）。refresh token 是一次性轮换，被拒绝后记录指纹并停止重试。
5. 代码注释、doc comment 用中文；用户可见输出、错误文本用简洁中文（GUI）/英文（CLI 内部标识）。
6. 不做 git 提交、推送、打 tag，除非用户明确要求。

## 常用命令

```bash
cargo check --workspace
cargo test --workspace
npx tsc --noEmit && npm test && npm run build   # src-ui 类型检查 + vitest + 前端产物
cargo build --release -p kimi-switch-cli
cargo build --release -p kimi-switch   # GUI：须带 default feature custom-protocol，禁止连 127.0.0.1:1420
npm install && npm run tauri build     # 等价官方打包入口；npm 侧 @tauri-apps/api、cli 必须与 crate tauri 同 minor（crate exact 钉版，不一致时 CLI 预检直接中止）
node scripts/generate-icons.mjs        # 重生成 src-tauri/icons（零依赖，输出确定性）
```

## Agent skills

### Issue tracker

GitHub Issues 写在 `Baker4real/kimi-switch-private`；`gh` 必须带 `-R`，禁止打到公开上游 `yuan2627/kimi-switch`。See `docs/agents/issue-tracker.md`.

### Triage labels

状态五套 + 分类 `bug` / `enhancement`。See `docs/agents/triage-labels.md`.

### Domain docs

single-context：根目录 `CONTEXT.md` + `docs/adr/`。See `docs/agents/domain.md`.
ownership / endpoint / extra 键 / agent 文档查找：仓库 OKF（`.okf-frontmatter.json`，含 `docs/agents/`）。

