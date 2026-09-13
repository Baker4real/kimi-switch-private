---
type: adr
title: 静默轮询可见性解耦与适应性档位
status: accepted
tags: [quota, silent-polling, adaptive-interval, visibility]
intent: Record that silent quota polling no longer pauses on document.hidden (spec #152 decision 13), gains a fire-time busy backoff to keep the scheduler alive, and switches to adaptive intervals driven by the current account's max used ratio (90s / 30s / 10s, decision 12) with a dual-rhythm scheduler where elevated rounds re-query only the current account, a 10s focus throttle floor on the backend, the 5s pending-renewal tier taking precedence, plus the Windows visibility baseline from tauri#10592.
documents:
  symbols:
    - adaptiveSilentQuotaIntervalMs
    - silentQuotaIntervalMs
    - currentAccountMaxUsedRatio
    - QUOTA_FOCUS_REFRESH_INTERVAL_MS
---
# 静默轮询可见性解耦与适应性档位

自动切换的判定只在额度快照完成后发生，静默轮询间隔 90s，最坏有 ≤90 秒的失败请求间隙
（ADR-0008 已明文接受）；且轮询原先在界面不可见（`document.hidden`）时暂停排程——用户常驻
最小化的使用习惯下，守护是否在场取决于 tauri#10592（Windows 最小化不改变 visibilityState，
仍 open）这个上游 bug 的偶然。spec #152 决策 12–13（实现 #160 / #161）做两件事：把可见性
从排程与执行决策中解耦，并把静默间隔改成随当前账号用况自适应的档位，把耗尽检测间隙从
分钟级压到秒级。术语见 CONTEXT.md「适应性轮询」。约束不变：判定只读已收集的额度快照、
不新打网；不引入常驻 daemon；档位只在 GUI 运行期间生效（静默轮询是前端调度器，CLI 无
持续轮询）。

## 决策

1. **`document.hidden` 不再参与排程与执行决策（#160，spec 决策 13）。** 排程（`arm`）与
   执行（`run`）都不再看可见性，界面隐藏/最小化时轮询与自动切换检测照常运行；既有的
   「不可见补跳」语义随之移除。解耦后 tauri#10592 存在与否不再影响本工具行为——守护
   在场不再依赖上游 bug 的偶然。

2. **fire 时撞上 busy 的短退避保活（#160 reviewer P1-1）。** 定时器 fire 时若用户操作在途
   或静默收集在飞，改排一个 5 秒短退避后重新 arm，不再裸早退：移除可见性解耦后，旧的
   「show → arm」兜底复活源不复存在，裸早退会让调度器永久停摆。退避期间只排定时器、不发
   请求（不忙轮询）；退避到期由 `arm()` 重算档位与到期时间，不欠账也不抢跑；`stop()` 与
   stopped 状态照常拒绝复活。

3. **适应性档位是「当前账号最大已用比例」的纯函数（#161，spec 决策 12）。**
   `< 80% → 90s`；`≥ 80% → 30s`；`≥ 95% → 10s`。10s 是下限，比例再高也不再加密。无当前
   行或当前行没有任何可用比例（如查询失败行）按 `< 80%` 常态处理。比例只取 active 行所有
   额度窗口里最大的非空 ratio——停泊账号的比例不参与，加密只由当前账号驱动。

4. **双节奏：当前账号加密，停泊账号维持 90s 原节奏（#161）。** 调度器记两个时刻：最近一次
   全量收集（含用户发起快照）决定停泊账号的 90s 节奏；最近一次覆盖当前账号的收集决定当前
   账号的档位节奏。加密档期间两档取更早到期的一档，同时到期优先全量（顺带刷新当前账号）。
   常态 90s 轮与待续期 5s 轮保持单一全量节奏。

5. **加密轮只重查当前账号（#161）。** 30s / 10s 档的收集范围是 "current"：只对当前账号发
   网络请求，停泊账号用缓存行组装快照——请求量不随加密档升高（对官方接口的增量请求只有
   当前账号自己的加密）。全量轮（"all"）仍是常态 90s、待续期 5s 与停泊账号的节奏。

6. **后端聚焦节流窗放宽到 10s 下限（#161）。** 后端成功缓存节流原是统一 90s
   （`min_refresh_interval_ms`），加密档的 30s / 10s 轮会被它拦空。聚焦收集时目标账号
   （当前账号）的成功节流窗放宽到 `QUOTA_FOCUS_REFRESH_INTERVAL_MS = 10s`（用户配置更小
   则从其小）；停泊账号与其他收集路径仍走 90s。已接受细节：聚焦轮 10s 节流下限与 10s 档位
   间隔相等，边界抖动时当轮跳过、当前账号实际间隔偶发翻倍到 20s——只影响检测间隙的边界
   情形，不值得为它再开豁免通道。

7. **凭证待续期 5s 档优先于适应性档位（spec 决策 12）。** 快照含凭证待续期行时静默间隔为
   5s（ADR-0009 的恢复信号节奏），高于任何适应性档位；该档是单一全量节奏。档位顺序：
   待续期 5s > 临界 10s > 高用量 30s > 常态 90s。

8. **档位不进配置，调参 out of scope。** 90 / 30 / 10s 与 80% / 95% 两条档位线是纯函数内
   常量，等实测数据再说（spec Out of Scope）。前端拿不到运行时配置，与 core
   `QUOTA_MIN_REFRESH_INTERVAL_MS` 默认值保持一致是既有惯例。

9. **首载失败不自动静默重试（既有行为，保留）。** 尚无成功快照时不预约静默定时器；成功
   快照后才按档位排程。静默失败不 toast，有上次成功快照则条留在当前界面。

## Windows visibility 实测 baseline

**上游现状（2026-09-13 核实）**：tauri-apps/tauri#10592 处于 OPEN，自 2024-08 开帖
以来无任何评论或修复进展。报告：Windows 10 + WebView2 下 `visibilitychange` 事件从不
触发、最小化时 `visibilityState` 恒为 `"visible"`；并链接上游 WebView2 缺陷
（MicrosoftEdge/WebView2Feedback#2681，微软方面说明最小化窗口不会改变子控件的
Visibility 属性）。此前的修复尝试 #9246 引起渲染闪烁，已被 #9465 回退。

**本机实测（2026-09-13）**：

- 环境：Windows 11（10.0.26200）+ WebView2（CDP serve_rev @cc2931e6363af1d7）、
  tauri v2.11.5 dev 构建（`npm run tauri dev`，vite dev server @127.0.0.1:1420）。
- 方法（可复现）：
  1. `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9223" npm run tauri dev`
     启动 dev GUI（应用有 single-instance 插件，须先关闭正式实例）；
  2. 经 CDP `Runtime.evaluate` 注入 1s 周期页内日志：
     `setInterval(()=>__log.push(new Date().toLocaleTimeString()+' '+document.visibilityState+' h='+document.hidden),1000)`；
  3. Win32 `ShowWindowAsync(hwnd, SW_MINIMIZE)` 最小化，`IsIconic(hwnd)==True` 验证真实
     最小化态；期间随时 CDP 回读 `__log` 尾部；`SW_RESTORE` 还原。
- 结果：
  - 前台基线：`visibilityState="visible"`、`hidden=false`（两次直读一致：09:39:12 /
    09:44:07）。
  - **最小化期间（约 3.5 分钟，IsIconic=True）：`visibilityState` 恒为 `"visible"`、
    `hidden` 恒为 `false`**——tauri#10592 在本机完全复现；页内 setInterval 时间戳每秒
    连续推进，定时器未被暂停或降频。
  - **静默轮询照常推进（#160 验收第 2 条）**：最小化期间 `quota_cache.json` mtime 推进
    09:39:15 → 09:40:45 → 09:42:16 → 09:43:46，约 90s 节奏；当前账号最大已用 27%
    （< 80%），处于 90s 常态档，行为正确。
  - 还原后 `visibilityState` 仍为 `"visible"`（无变化事件触发）。
  - 未测场景（如实标注）：窗口被其他全屏窗口完全遮挡（occlusion）未测；tauri#10592
    提及遮挡路径，本结论仅覆盖最小化。

**结论**：Windows 最小化下 WebView2 不改变 `visibilityState`（上游 bug 仍 OPEN，
2024-08 后无活动），旧版「hidden 暂停」在 Windows 最小化场景本就不会生效——此前的
守护覆盖纯属 bug 掩护。可见性解耦（决策 1）后轮询不再读取该状态，无论 baseline 取值
如何、上游修复与否，本工具行为都不受影响；本次实测同时证明最小化下快照轮询按档位
节奏照常推进。若未来想重新引入任何可见性优化（如暂停纯展示刷新），以本 baseline 为
起点重测。

## Considered Options

- **等待 tauri#10592 修复后再做可见性相关设计**：open 逾两年、维护者称无好修法；而守护
  在场是当下的需求。解耦让 bug 与行为无关，是唯一不受上游节奏绑架的选项。
- **加密档全量重查**：停泊账号请求量会随加密档线性放大，违背「平时不打搅官方接口」的
  既有基调；只有当前账号的用况驱动加密，双节奏是最小代价的对应。
- **10s 以下更密的档位（如 5s 耗尽冲刺档）**：5s 已是待续期恢复观察档的节奏，与配额检测
  混用同一密度会拉高故障时刻的接口压力；10s 下限 + 被动切换（ADR-0017）的秒级兜底已把
  检测间隙压到足够小。
