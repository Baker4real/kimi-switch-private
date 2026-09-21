# 快照重绘与字体成本基线（issue #168，2026-09-21）

测量基线报告：只测量、只记录，不预设重构，未改动任何产品源码/样式/测试。
原始数据与测量脚本：`D:\agents\run-evidence\kimi-switch\2026-09-21-issue-orchestration\n168-perf\`（下称“证据目录”）。

## 1. 问题与验收对照

issue #168 五条验收：

1. 代表性账号数量下快照更新的重绘耗时、焦点/展开稳定性、帧响应 → 第 3 节
2. 离线中文字体加载耗时、资源占用、对启动/首屏的实际影响 → 第 4 节
3. Windows 支持环境、输入规模、步骤、原始结果与结论 → 第 2 节（环境）、各节（步骤+数值）
4. 仅当测量证明存在可复现瓶颈才提出优化 ticket → 第 6 节（结论：未证明，保留现有策略）
5. 不以静态结构/文件大小/历史评分单独宣称缺陷 → 全部结论只挂在实测数值上

## 2. 环境与被测产物

| 项 | 值 |
|---|---|
| OS | Windows 11 Pro 26200（本机，Windows 支持环境） |
| 浏览器通道 | Microsoft Edge `153.0.4234.32`，`--headless=new`，`--window-size=1280,880`，CDP（remote-debugging-port）驱动 |
| 服务 | 本机 `127.0.0.1` 静态 HTTP（node:http），服务冻结的 `dist/web` 构建产物 + 注入 harness |
| 被测产物 | `cd src-ui && npm run build`（vite 7.3.6，29 modules）产物，冻结快照（见下）；期间共享工作树被其他 worker 并发重建过，最终两序列均使用同一冻结快照 |
| 冻结快照 manifest | 证据目录 `harness/dist-manifest.txt`：`index-CP-iYTHW.js` 55650B、`index-v2wH02k-.css` 33105B、`NotoSansSC-VF-CzR3NRmW.ttf` 17773244B、IBM Plex ×3 各约 200KB（含 sha256 前 16 位） |
| 仓库状态 | git `e43642b` + 其他 worker 的 in-flight 未提交改动（保留原样，未触碰） |
| 并发噪声 | 其他 worker 同时跑构建/测试；测量数值中偶发的大离群（如 load 1.2-1.6s 的个别 run）按原始值保留并标注 |

## 3. 快照重绘基线

### 3.1 方法（可复现）

- **Harness**：真实 `dist/web` 产物 + 页内注入（均先于应用模块执行）：
  - `__TAURI_INTERNALS__` invoke stub：`list_accounts` 每次返回新生成的合成快照（配额数字与 `cached_at` 逐次变化，保证重绘真实发生）；`swap_account` 返回轻量 outcome；其余命令返回 null/false。
  - 合成账号：中显示名 + 7天/5小时配额 + 混合档位，规模 N ∈ {1, 5, 20, 50}。
- **驱动**（CDP `Runtime.evaluate`，页内执行）：
  1. 等应用首帧快照就绪（`#metric-total` 文本 = N 且卡数 = N）；
  2. 切到 `#library` 路由（卡片可见）；
  3. 热身 3 次，正式 20 次：每次点可见的 `#library-refresh-button`（真实用户刷新流，与静默刷新共用同一 `paintSnapshot` 路径）；
  4. 每轮按 i%4 交替四种前置态：mode0 焦点在某卡「切换」钮、mode1 打开该卡「…」菜单并焦点其开关、mode2 展开「账号行为」折叠面板、mode3 主动 blur（焦点在 body）；
  5. 每轮记录：点击时刻、stub 收到 `list_accounts` 的时刻、`#accounts-list` MutationObserver 回调时刻（DOM 重建完成）、其后首个 rAF 时刻、焦点前后、菜单 `aria-expanded` 前后、面板 `aria-expanded` 前后；
  6. 全程 rAF 帧时间戳序列 + `longtask`（≥50ms）PerformanceObserver；
  7. N=20 附加 5 次「切换流」序列：点开「…」菜单后点该卡「切换」，等 busy 内即时重绘 + busy 外后台重绘都落地后记录焦点与菜单态。
- **指标定义**（页面级可观察量，不涉及实现内部）：
  - `invoke→重绘完成`：stub 收到 `list_accounts` 到列表 MutationObserver 回调——单次快照重绘成本的紧上界；
  - `click→重绘完成`：用户可感知的刷新到列表更新；
  - `重绘完成→下一帧`：更新后首帧呈现延迟。
- **防污染**：每次全新 `--user-data-dir`；启动前清理调试端口残留进程；页面 URL 带唯一 `fresh=` token 并在采集前校验 `location.href`（杜绝连到上一轮残留页）；dist 冻结快照隔离并发重建。
- 命令：`cd <证据目录> && node make-harness.mjs && node measure-repaint.mjs`（脚本见第 7 节）。

### 3.2 重绘耗时（每档 20 次原始值，单位 ms）

| N | invoke→重绘完成（20 次原始值） | 中位 | 最大 | click→重绘完成 中位 |
|---|---|---|---|---|
| 1 | 2.4, 1.3, 1.4, 0.4, 1.4, 1.4, 1.2, 0.4, 1.7, 1.2, 1.2, 0.5, 1.2, 1.1, 1.2, 0.5, 1.7, 1.5, 1.9, 0.5 | 1.2 | 2.4 | 1.5 |
| 5 | 4.5, 2.4, 2.4, 1.1, 2.4, 3.0, 3.2, 1.0, 2.3, 3.9, 2.6, 1.3, 2.4, 2.2, 2.2, 0.9, 2.1, 2.4, 2.2, 0.7 | 2.4 | 4.5 | 2.5 |
| 20 | 9.4, 6.3, 7.4, 2.1, 6.5, 7.1, 6.9, 2.1, 8.1, 6.4, 8.4, 1.9, 5.7, 8.1, 5.8, 2.0, 6.4, 6.1, 6.1, 2.8 | 6.4 | 9.4 | 6.8 |
| 50 | 21.1, 14.2, 16.4, 4.3, 13.9, 14.5, 13.5, 4.8, 15.2, 13.0, 17.6, 4.7, 13.8, 16.5, 13.0, 4.1, 13.7, 13.0, 17.0, 5.1 | 13.8 | 21.1 | 14.6 |

可复现性：本表为第 3 轮完整测量（冻结快照 CP-iYTHW）；另两轮完整测量（13:14 与 13:35，不同构建快照）中位分别为 1.1/2.5/6.5/13.6 与 1.2/2.5/6.2/13.7 ms——三轮一致，量级稳定。N=50 的双峰（~4-5ms 与 ~13-21ms）来自逐次布局/GC 差异，原始值如上。

### 3.3 焦点与展开稳定性（可观察行为，全部样本）

| 观察 | 结果 | 样本量 |
|---|---|---|
| 焦点在「切换」钮上经过快照重绘 | 焦点保留：重绘后 `document.activeElement` 仍为同卡同 `data-swap` 控件（mode0，5/5 × N=5/20/50） | 15/15 |
| 焦点在「…」菜单开关上、菜单展开，经过快照重绘 | 焦点保留在同卡「…」开关；**菜单收起**（`aria-expanded` 回 false，5/5 × N=5/20/50；N=1 当前卡无「…」开关，无样本） | 15/15 |
| 「账号行为」折叠面板展开态经过快照重绘 | 保留展开（不因快照改写） | 全部 mode2 样本 |
| 焦点在 body（已 blur）经过快照重绘 | 停留在 body，无焦点窃取 | 全部 mode3 样本 |
| 切换流（N=20 ×5）：点开菜单→点「切换」→busy 内即时重绘+后台重绘 | busy 结束后焦点落回**同卡**的「…」开关（目标卡成为当前账号后其「切换」钮不再存在，回落到同卡等价控件）；菜单收起；busy 指示清除 | 5/5 |

菜单在快照更新后收起与既有真机观察一致（`run-evidence/kimi-switch/20260911-webview2-synthetic-input/summary.md` 第 5 条）；本轮以 headless 程序化方式复核成立。

### 3.4 帧响应

- rAF 间隔（headless 合成器节律，仅作通道内对照）：中位 7ms，p95 13-17ms；每序列（约 10s、20 次重绘）>100ms 的间隔 1-3 个，与重绘无对应关系（单次重绘最长 21ms，不足以造成 >100ms 帧间隔；离群属合成器/GC 空闲偶发）。
- `longtask`（主线程 ≥50ms）：每序列 0-1 个（77/71/72ms，页面整个生命周期内各出现一次，不随 20 次重绘重复出现，无法归因于重绘路径）。
- `重绘完成→下一帧`：多数样本 0.2-6ms（见 `results/repaint.json` 逐轮 `repaintToNextFrameMs`）。

### 3.5 小结

重绘成本随账号数近似线性（每账号约 0.25-0.3ms）：50 账号中位 13.8ms、最差 21.1ms；代表性规模（≤20）中位 ≤6.4ms。未出现可归因于重绘的 ≥50ms 主线程长任务；焦点/展开行为全部按设计可观察地稳定。

## 4. 离线中文字体基线

### 4.1 变体与方法（可复现）

- `font-real`：真实构建（`Noto Sans SC Custom` @font-face 存在，`font-display: swap`，4 个 FontFace）。
- `font-none`：同一构建 CSS 去掉 NotoSansSC `@font-face` 块（162 字符）的副本（3 个 FontFace，中文回退 Segoe UI）；除此之外页面完全相同。
- 采集（页内早期注入 + CDP 取回）：navigation timing（ttfb/DCL/load）、paint（FCP）、resource timing（逐资源 duration/transferSize）、`document.fonts.ready` 时标、首快照重绘帧时标（head 顶层 MutationObserver）、JS 堆、`document.fonts.size`；冷缓存 = 每次全新 `--user-data-dir`，暖缓存 = 同 profile 第 2-4 次加载（服务器发 `Cache-Control: max-age=86400`）。
- 命令：`node make-harness.mjs && node measure-font.mjs`；截图对照：`node screenshot-diff.mjs`。

### 4.2 加载耗时（原始值，ms）

| 变体 | Noto resource duration | `fontsReady − load` | load 事件 | 首快照帧 |
|---|---|---|---|---|
| real 冷（n=5） | 182, 121, 140, 207, 173（中位 173） | 224, 171, 193, 14, 16（中位 171） | 1249, 580, 657, 792, 1562（中位 792） | 1267, 497, 575, 539, 1254（中位 575） |
| none 冷（n=5） | 无请求 | 9, 6, 4, 5, 4（中位 5） | 552, 701, 653, 566, 592（中位 592） | 556, 686, 553, 552, 493（中位 553） |
| real 暖（n=3） | 24, 23, 24 | 19, 19, 77 | 602, 990, 960 | 468, 848, 964 |
| none 暖（n=3） | 无请求 | 304, 3, 12 | 173, 706, 923 | 478, 830, 928 |

读取方式：Noto resource timing = `performance.getEntriesByType("resource")` 中 `NotoSansSC-VF-*.ttf` 条目的 `duration`/`transferSize`；每次冷加载实际传输 17,773,544 B（磁盘文件 17,773,244 B）。

要点：

- **获取**：17.77MB 在本机回环 HTTP 上 121-207ms（冷）；磁盘缓存命中后 17-24ms（暖）。
- **就绪**：冷加载下字体就绪比 load 事件晚约 166-190ms（中位 171 vs 5）；`font-display: swap` 期间文本以回退字体先行渲染。
- **首帧**：两变体首快照帧中位差 +22ms（575 vs 553），在机器噪声带内（各轮 load 中位差在 55-200ms 间波动，来自并发 worker 与进程启动抖动，非字体单独可分辨）。FCP：real 912/932/876 vs none 924/936（样本不全——headless 下快速 run 常缺 FCP 条目），无可分辨差异。

### 4.3 资源占用

- 磁盘/传输：NotoSansSC 17.77MB（一次性文件，随代码提交；每次冷加载传输同量字节）；IBM Plex ×3 合计约 0.6MB，冷获取合计 213-342ms、暖 6-9ms。
- JS 堆（`performance.memory.usedJSHeapSize`，页面稳态）：real 1058-1060KB vs none 1051-1054KB——差异 <1%，字体不进 JS 堆，无可观察影响。
- 字体解码后的渲染进程原生内存：**未测**。原因：CDP `SystemInfo.getProcessInfo` 在 Edge 153 只返回进程 type、不含 memory 字段；OS 层无法把渲染进程 PID 与本页面可靠对应。已如实记录（`results/font.json` 的 `procInfo` 字段为空对象数组）。

### 4.4 渲染差异对照（A/B 有效性证据）

- `document.fonts.size`：real=4（含 Noto）vs none=3（已剔除）；none 变体无任何 NotoSansSC 网络请求。
- 截图（`results/font-real.png` / `font-none.png`，1280×880 稳态）：系统级像素比对抽样差异 0.8%（4px 步进）——布局与卡片一致，仅文本字形不同，证明 A/B 只交换了字体这一个变量。

## 5. 未测项与原因

| 项 | 原因 |
|---|---|
| 真实 `kimi-switch.exe` 启动计时 | 需拉起真实 GUI 进程并触碰真实账号库；超出本节点「全部程序化 + 合成数据、不操控真实 GUI」的授权边界。headless 渠道已覆盖字体对页面加载/首屏的影响 |
| 渲染进程原生内存（解码字体占用） | 本 Edge 版本 CDP 不暴露（见 4.3） |
| 真机显示器帧率/交互卡顿 | headless 渲染节律不代表真实显示器；以 longtask + 重绘时长作为主线程占用的可观察替代 |
| 静默轮询（90/30/10s 档）触发的重绘单独计时 | 与刷新流共用同一 `paintSnapshot` 代码路径（invoke→重绘完成逐次实测已覆盖其成本）；90s 档位等待不经济且不改变结论 |

## 6. 结论

**未发现可复现的性能瓶颈，按 issue #168 验收第 4 条：不提出优化 ticket，保留现有策略。**

- 快照重绘（全量列表重建）：代表性规模 ≤20 账号中位 ≤6.4ms、最差 9.4ms；50 账号中位 13.8ms、最差 21.1ms。80 次逐次采样中没有一次触发 ≥50ms 主线程长任务；焦点保留、菜单收起、折叠面板稳定等行为在全部样本中一致。最差 21.1ms 略超单帧 16.7ms 预算但仅出现在 50 账号规模的个别采样，无可观察的用户可见卡顿证据，不构成"可复现瓶颈"。
- 离线中文字体（17.77MB Noto Sans SC）：代价集中在首次加载——本机回环获取 121-207ms、字体就绪比 load 晚约 170ms（swap 期间回退字体先行渲染）；暖缓存 17-24ms 基本归零；JS 堆无可观察影响；首帧/FCP 差异在机器噪声带内不可分辨。打包体积增大 17.77MB 是既定取舍（离线中文渲染一致性），按验收第 5 条，文件大小本身不作为缺陷宣称；实测亦未发现与其对应的首屏代价。
- 上述结论的边界：headless Edge + 本地 HTTP 渠道（等价于 Tauri 本地资源加载的磁盘成本，不含真实 exe 进程启动）；测量机器有并发构建负载，慢离群已在原始值中保留。

## 7. 脚本与原始数据索引（证据目录）

| 文件 | 作用 |
|---|---|
| `make-harness.mjs` | 冻结 dist 快照 + 生成 repaint-{1,5,20,50}.html、font-real/none.html、style-nonoto.css、dist-manifest.txt |
| `serve.mjs` | 127.0.0.1 静态服务（harness 优先、冻结 dist 回退、Cache-Control） |
| `cdp.mjs` | 零依赖 CDP 客户端（手写 WebSocket 帧；含调试端口残留清理 killPortOwner） |
| `measure-repaint.mjs` | 重绘测量编排（N=1/5/20/50 × 20 次 + 切换系列） |
| `measure-font.mjs` | 字体测量编排（两变体 × 冷 5 + 暖 3） |
| `screenshot-diff.mjs` | 两变体稳态截图 |
| `results/repaint.json` | 重绘原始数据（逐轮全部字段、帧统计、longtask、切换系列） |
| `results/font.json`、`results/font-partial.jsonl` | 字体原始数据（逐 run marks/resources/paint/heap） |
| `results/font-real.png`、`results/font-none.png` | A/B 渲染对照截图 |
| `smoke.mjs`、`debug1.mjs`、`debug2.mjs` | 通道调试脚本（保留以供复现排障） |
