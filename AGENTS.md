# kimi-switch · Agent 快速规则

Kimi Code 多账号管理工具（Rust workspace）：GUI（`kimi-switch.exe`）+ CLI（`kimi-switch-cli.exe`）。

## 目录速记

```text
crates/core/             数据模型、Provider（activate/query_quota）、凭证仓库、额度快照、路径、显示名、自动切换决策
crates/cli/              kimi-switch-cli 命令行
crates/gui/              GUI 命令层（列表/切换/导入/设备码；不含 token DTO）
src-tauri/               Tauri 壳（kimi-switch.exe）
src-ui/                  浅蓝白卡片风前端（总览 / 账号库两个路由，无设置页；fonts/ 无衬线 TTF 随代码提交；api.ts / store / deviceAuth / membership / auto-swap / account-status / labels / library-card / overflow-menu / icons）
scripts/generate-icons.mjs  图标确定性生成器（改图标改这里的常量后重跑，勿手改 icons/ 二进制；icons/ 变更不会触发 tauri-build 重嵌 exe 图标资源，GUI 构建前需先 touch src-tauri/build.rs）
crates/providers/common/ 文件型 OAuth 账号切换共享引擎（FileBlobProvider/FileBlobRuntime）
crates/providers/kimi/   Kimi 适配器（设备码授权、token 刷新协调、额度查询、官方昵称 /me、绑定网页登录）；网络发送统一走 src/net.rs（env 代理不可达回退直连，ADR-0005；有 timeout 时回退只用剩余时间，新增请求勿内联建 client）
```

## 规则

1. **按改动面验证**：Rust 核心、Provider、凭证或共享契约改动运行 `cargo check --workspace` 及受影响的 `cargo test --workspace`；改了 `src-ui/` 再运行对应的 typecheck、测试和构建。跨层、发布或共享契约变化再扩大到完整组合；通过后按任务需要运行 release build。
2. **凭证安全是最高优先级**：token 只能写用户本机私有文件；网络请求只允许发往 Kimi 官方域名（`auth.kimi.com` / `api.kimi.com` / `www.kimi.com`）。`www.kimi.com` 仅限「绑定网页登录」的会员到期读取路径（ADR-0004）。官方昵称只允许在既有导入/授权/额度流程上额外 `GET api.kimi.com/coding/v1/me`，禁止 `www.kimi.com/me`；禁止引入遥测。
3. **手动切换永远可用**：`swap` 不依赖额度查询、`/me` 和网络，原子写入 + 快照回滚的逻辑不得破坏。额度查询成功后不得 await `/me`（后台缓存即可）；`/me` 失败不失败主流程。额度耗尽自动切换默认关闭；子开关 `prefer_lower_tier` / `resident_lower_tier` / `prefer_soonest_reset` / `pace_pre_swap` / `pre_reset_burn` 亦默认关，只改自动切换判定或排序、不影响手动切换。开启「重置前抢用」后烧完自动切回。`candidate_margin` 默认 0；开启回驻后确认期 `resident_confirm_n` 默认 2（已开回驻的存量配置会从立刻回驻变成连续两周期）。`pre_reset_burn` 默认关，三个参数 `pre_reset_burn_min_ratio` 默认 0.7、`pre_reset_burn_near_5h_ms` 1h、`pre_reset_burn_near_7d_ms` 2d。账号 extra `auto_swap_opt_out` 只过滤自动切换候选。凭证待续期：UserInitiated 刷新直查（429 除外）；Scheduled 仅在当前登录文件 mtime 恢复信号时放行（ADR-0009）。判定只用已收集的额度快照、不新打网；执行只走既有 `activate`；不引入常驻 daemon。细节见 CONTEXT.md 与 ADR-0008 / 0009 / 0010–0014。
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

**Issue 操作**（建票/读/评论/标签/关票）前读 `docs/agents/issue-tracker.md`；`gh` 必须带 `-R Baker4real/kimi-switch-private`，禁止打到公开上游 `yuan2627/kimi-switch`。

### Triage labels

**打标签 / triage** 前读 `docs/agents/triage-labels.md`。

### Domain docs

**域词与 ADR**：根目录 `CONTEXT.md` + `docs/adr/`（见 `docs/agents/domain.md`）；**ownership / endpoint / extra 键 / agent 文档查找**走仓库 OKF（`.okf-frontmatter.json`，含 `docs/agents/`）。
