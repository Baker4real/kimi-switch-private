---
type: adr
title: 跟随启动：Kimi Code 会话开始时经 SessionStart hook 拉起 GUI
status: accepted
tags: [auto-swap, follow-launch, session-start-hook, install-hook, gui]
intent: Record the follow-launch decision. Polling-only auto-swap paths (exhaustion after a quota snapshot, pace, resident return, pre-reset burn and its return) exist only while the GUI runs, so forgetting to open the GUI silently drops them; the passive StopFailure path only covers exhaustion after a request has already failed. An observer-only CLI subcommand behind Kimi Code's official SessionStart hook (matcher startup|resume, installed by install-hook together with StopFailure, removed by --uninstall) launches the GUI minimized and without stealing focus when it is not running, leaves a running GUI untouched, is gated by auto_swap.enabled, treats every Kimi Code client alike, launches only from the interactive desktop session on Windows (SSH and other non-interactive sessions, and non-Windows platforms, are skipped and traced), never writes stdout, and appends one on_session_start audit line per event without cwd or session title. No follow-close, no SessionHeartbeat keepalive, no tray, no close confirmation. It launches the GUI only and adds no swap trigger, so ADR-0017 decision 10 stands with its rationale amended. Rejected - Windows logon autostart, a resident process watcher, heartbeat keepalive, ungated launching, cross-session launch through a scheduled task, and launching inside SSH sessions.
documents:
  config_keys:
    - auto_swap.enabled
---
# 跟随启动：Kimi Code 会话开始时经 SessionStart hook 拉起 GUI

自动切换里有一大块只在 GUI 运行时存在：额度快照之后的耗尽判定、pace 预切换、回驻、重置前抢用
与回程，都由 GUI 前端的静默轮询驱动（ADR-0016），CLI 没有持续轮询。GUI 没开时只剩配额失败被动
切换（ADR-0017），它要等一轮请求已经撞上额度耗尽才切。用户忘了打开 GUI，前面这些路径就整段
缺席，而且没有任何提示。本机审计截至 2026-10-29：成功的自动切换 49 次，被动触发 7 次，其余
42 次都来自额度快照之后的判定（pace 14、重置前抢用 11、抢用回程 4、回驻 5、耗尽 8）。ADR-0017
决策 10 以「hook 已覆盖『工具未运行』场景」为由不引入第二触发源，这句理由只覆盖了耗尽之后一种
情形。规格见 spec #195，实现拆为 #196–#198，真机验收 #199。

关键事实（2026-10-29 核实）：

- Kimi Code 桌面版（1.2.0，Electron）安装包内带与 CLI 同一套 agent 内核和 hook 引擎，读同一个
  `~/.kimi-code/config.toml`；本机已在用的 `SessionStart`、`StopFailure` 条目对它同样生效。
- 官方 [hooks 文档](https://www.kimi.com/code/docs/kimi-code-cli/customization/hooks.html)：
  `SessionStart` 在会话开始或恢复时触发，matcher 取 `startup` / `resume`，只能观察、不能阻断；
  stdin 带 `client_type`；退出码 0 时 stdout 可能被加进模型上下文。
- GUI 没有托盘，关窗口即退出；单实例回调在第二次启动时会还原窗口并抢焦点。单实例锁按 Windows
  会话隔离：tauri-plugin-single-instance 2.4.3 的互斥量名是 `{id}-sim`，不带 `Global\` 前缀。

## 决策

1. **触发：官方 `SessionStart` hook。** `install-hook` 在 Kimi Code 的 `config.toml` 写入
   `event = "SessionStart"`、`matcher = "startup|resume"` 的条目，命令是 CLI 观察型子命令（下称
   拉起器）：GUI 未运行时把它拉起，退出码恒为 0，毫秒级返回，stdout 不输出任何内容。触发点是
   会话开始或恢复，不是 Kimi Code 窗口打开；会话开始总在该会话的第一次请求之前。

2. **只拉起，不判定。** 拉起器不读额度、不判定、不切换、不写自动切换状态。GUI 起来之后照既有的
   快照轮询与判定工作。所以跟随启动不是新的自动切换触发源，ADR-0017 决策 10 不变，只修订它的
   理由（见该决策的修订注记）。

3. **总开关门控。** `auto_swap.enabled` 关闭时拉起器什么都不做，与被动路径同闸（ADR-0017
   决策 2）。目的就是让自动切换在场；总开关关着还拉起，拉起的只是一个额度面板。不新增配置项。

4. **窗口：最小化、不抢焦点。** 拉起的 GUI 最小化到任务栏，不到前台；最小化时轮询照常
   （ADR-0016 实测）。GUI 已在运行时窗口状态不动，单实例回调忽略这次拉起。用户手动启动仍按现状
   还原并聚焦。不加托盘。

5. **生命周期：不跟随关闭。** Kimi Code 退出后 GUI 继续运行，直到用户自己关。用户在会话中途关掉
   GUI，就等下一次会话开始或恢复再拉起：不用 `SessionHeartbeat` 拉回，不弹关闭确认。AGENTS.md
   规则 3「不引入常驻后台进程」的解读：拉起的是用户随时能关的普通最小化窗口，不是常驻后台进程。
   代价：第一次会话之后，GUI 通常一直开到用户手动关，其间静默轮询照常请求官方接口。

6. **范围：全部客户端，只限 Windows 本机桌面会话。** 桌面版、`kimi` CLI、IDE/ACP 等 Kimi Code
   客户端同等对待，它们消耗的是同一个当前账号的额度。经 SSH 等非交互会话开的 Kimi Code 会话
   不拉起、只留痕：单实例锁按会话隔离，在那里拉起的 GUI 用户看不见、关不掉，还会和桌面上的 GUI
   成为两份轮询，各自轮换停泊账号的一次性 refresh token。SSH 场景按用户决定不做专门支持。非
   Windows 平台（macOS、Linux）同样只留痕、不拉起：交互会话判定、「本会话已有 GUI 进程」判定和
   拉起方式都按 Windows 设计，其他平台的等价做法这次不做（拆票时与用户确认）。

7. **安装与卸载随 `install-hook`。** `install-hook` 默认同时装 `StopFailure` 与 `SessionStart`
   两条，`--uninstall` 两条都移除。幂等、刷新 command 与 timeout、收敛重复条目、保留用户其它
   hook 与注释、解析失败绝不覆写，都沿用 ADR-0017 决策 8；新条目按 `event = "SessionStart"` 且
   `command` 含拉起器子命令名识别。存量安装重跑一次 `install-hook` 即补上新条目。GUI 不检测、
   也不提示 hook 有没有装。

8. **留痕：每个事件一行。** 照 ADR-0017「留痕」的先例，每个 `SessionStart` 事件在审计日志恰好
   追加一行 `action = "on_session_start"`：hook 事件名、来源（`startup` / `resume`）、客户端
   类型，以及结果——拉起、已在运行、总开关关闭、非交互会话、平台不支持、拉起失败。不写工作目录
   与会话标题。
   拉起失败也只记在这里：不弹窗，不写 stdout。

9. **进程细节。** 拉起器取 CLI 同目录的 `kimi-switch.exe`。拉起的 GUI 工作目录设为程序所在目录，
   不继承会话的项目目录，否则 Windows 上那个目录会被占住、删不掉。GUI 脱离 hook 的进程树，
   Kimi Code 退出时不被连带结束。

## Considered Options

- **Windows 登录自启 GUI**：最简单，也能防忘开；但与是否在用 Kimi Code 无关，整天运行、整天
  轮询官方接口。
- **常驻进程监视 `Kimi Code.exe` 启动**：直接违反 AGENTS.md 规则 3。
- **`SessionHeartbeat` 每 60 秒把 GUI 拉回来**：会话期间 GUI 关不掉；用户关掉 GUI 是明确的选择，
  不是忘开。ADR-0017 决策 10 也因复杂度拒绝过把它当触发源。
- **托盘或关闭确认**：都是新的界面面。误关真成问题时，先加关闭确认。
- **不门控（总开关关闭也拉起）**：拉起的只是额度面板，不是本功能的目的。
- **单独的安装参数**：总开关已让 `SessionStart` 条目在自动切换关闭时不起作用，多一个参数只多
  一步。
- **SSH 会话经计划任务把 GUI 拉到桌面会话**：能覆盖远程会话，但多一项系统级持久配置；用户决定
  不考虑 SSH。
- **在 SSH 会话里直接拉起**：看不见、关不掉，可能和桌面 GUI 成为两份并行轮换 refresh token。

## Windows 真机验收（2026-10-30）

在 Kimi Code Desktop 1.2.0、kimi-switch 1.4.17 的 release GUI/CLI 上完成 #199。
构建使用 `npm run build:desktop`，源码包含本次跟随启动提交及此前已部署的 OAuth 锁修复、中文
文案补丁；工作区里这些独立补丁仍由原任务持有。固定 release 路径、桌面快捷方式目标与图标、
安装后两份 exe 的 SHA-256 均独立回读一致。

- **双 hook 安装**：使用固定路径 release CLI 重跑 `install-hook`，StopFailure 与 SessionStart
  各一条，timeout 均为 10 秒，后者 matcher 为 `startup|resume`。用户其他配置和 hook 的解析值
  与文本保持不变；再次安装文件字节和 mtime 均不变。
- **打开窗口与恢复会话**：退出整个 Kimi Code 进程再启动时，本机桌面版自动恢复上次会话，
  记录 `source=resume client_type=kimi_code_desktop decision=launched`。不能把这次拉起归因于
  单纯显示窗口；进入空白的新会话页面、尚未发首条消息时没有新增 SessionStart。
- **新会话**：在专用验收会话发送一条仅要求回复 OK、禁止工具调用的消息，产生
  `source=startup client_type=kimi_code_desktop decision=already_running`，原 GUI 窗口不变。
- **窗口与轮询**：后台启动由隐藏窗口直接转为最小化。以约 20 毫秒间隔采样窗口可见性、
  最小化状态和前台 PID，未观察到可见且未最小化的中间态，前台一直留在 Kimi Code。
  GUI 连续最小化约 198 秒；额度缓存写入时刻为 11:06:35.748 → 11:08:12.384 →
  11:09:42.846（UTC+8），间隔 96.6 / 90.5 秒，符合常态 90 秒加请求耗时的节奏。
  这项采样不声明排除了小于采样间隔的显示变化。
- **重复启动**：真实 CLI 两次输入 SessionStart 均 stdout 为空，分别在 278.0 / 42.7 毫秒内返回，
  决定为 launched / already_running。直接以 `--background` 二次启动，在已有 GUI 最小化、
  前台、后台三种状态下均保留原窗口及焦点，只留下原实例；普通二次启动仍还原并聚焦。
- **独立生命周期**：从 Kimi Code 菜单执行完整退出，确认所有 `Kimi Code.exe` 进程消失后，
  真实 hook 拉起的 GUI 原 PID 和窗口继续存活；关闭 GUI 仍直接退出。
- **总开关门控**：临时关闭 `auto_swap.enabled`，退出 GUI 后再恢复 Kimi Code 会话，得到
  `decision=disabled` 且未拉起 GUI。验收结束已恢复全部原有自动切换开关值。
- **每事件一行**：上述真实 resume/startup/禁用恢复事件各新增一条 `on_session_start`，仅含
  事件、来源、客户端类型与决定，没有 cwd、会话标题或 stdin 原文。

本机原始证据：`D:/agents/run-evidence/kimi-switch/spec-195-20261030/` 下的
`hook-install.json`、`synthetic-*.json`、`watch-synthetic-minimized.json`、
`watch-client-open.json`、`watch-real-startup-existing.json`、`watch-parent-exit.json`、
`watch-disabled-resume.json`、`duplicate-*.json`、`settings-restored.json`、`shortcut.json`。
SSH 场景按规格不做真机验收；非 Windows 行为只由平台分支与自动化测试覆盖。
