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
edge-extension/          Edge 免粘贴绑定扩展（MV3 三文件私有分发包：仅带挑战 fragment 的控制台页读网页凭证→一次性 loopback 回调，再凭 nonce 只读回查状态并在挑战页显示结果，ADR-0018；协议字面量与权限面由 src-ui/edge-extension-contract.test.ts 静态钉住，改协议两侧测试同改；真实 Edge 交接不再作为关闭条件，且尚未真机核对）
```

## 规则

1. **按改动面验证**：Rust 核心、Provider、凭证或共享契约改动运行 `cargo check --workspace` 及受影响的 `cargo test --workspace`；改了 `src-ui/` 再运行对应的 typecheck、测试和构建。跨层、发布或共享契约变化再跑全量检查 `npm run check`；通过后按任务需要运行 release build。
2. **凭证安全是最高优先级**：token 只能写用户本机私有文件；网络请求只允许发往 Kimi 官方域名（`auth.kimi.com` / `api.kimi.com` / `www.kimi.com`）。`www.kimi.com` 仅限会员到期读取（绑定 / 用户点刷新 / 已绑定账号的导入或 login 补读，ADR-0004）；静默列出不打该接口。官方昵称只允许在既有导入/授权/额度流程上额外 `GET api.kimi.com/coding/v1/me`，禁止 `www.kimi.com/me`；禁止引入遥测。
3. **手动切换永远可用**：`swap` 不依赖额度查询、`/me` 和网络，原子写入 + 快照回滚的逻辑不得破坏（切换路径加固见 ADR-0015）。额度查询成功后不等 `/me`：`/me` 只在后台补缓存，失败不影响主流程。自动切换及其全部子开关默认关闭，只改自动切换的判定或排序，不影响手动切换。自动切换判定只读已收集的额度快照、不新增额度查询，被动触发同样零新增查询；执行只走既有 `activate`；不引入常驻后台进程。hook 只经 CLI `install-hook` 显式写入 Kimi Code 配置，其他路径不得隐式改它。各开关与参数的默认值、阈值、轮询档位、凭证待续期放行与被动触发口径以 CONTEXT.md 和 ADR-0006、ADR-0008–0017 为准，改动只落在那里。
4. **token 刷新必须与官方客户端协调**：有官方锁协议的平台只能复用官方锁，不得自创并行刷新机制；仅在无官方锁的平台组合（当前：Windows + TS 客户端），允许经「主动续期」开关（默认关）依赖官方 100ms 重读采纳机制做查询前轮换，前提是原子写回（先账号库后 live，写序见 ADR-0002）。refresh token 是一次性轮换，被拒绝后记录指纹并停止重试。
5. 代码注释、doc comment 用中文；用户可见输出、错误文本用简洁中文（GUI）/英文（CLI 内部标识）。
6. 不做 git 提交、推送、打 tag，除非用户明确要求。

## 常用命令

```bash
npm run check                          # 全量检查：check:ui → cargo fmt --check → cargo check → cargo test（Rust 均为 --workspace）
git config core.hooksPath .githooks    # 每个克隆启用一次：pre-push hook 推送前跑 npm run check
cargo check --workspace
cargo test --workspace
npx tsc --noEmit && npm test && npm run build   # src-ui 类型检查 + vitest + 前端产物
cargo build --release -p kimi-switch-cli
cargo build --release -p kimi-switch   # GUI：须带 default feature custom-protocol，禁止连 127.0.0.1:1420
npm run build:desktop                  # 本机更新：check:ui → GUI → hook 所调的 release CLI；先退出运行中的 kimi-switch.exe
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
