# kimi-switch Edge 绑定助手

私有分发的 Edge 扩展，只服务于 GUI 的「从 Edge 自动读取并绑定」流程（Windows）。分发包就是本目录源码：`manifest.json` / `content.js` / `service-worker.js` 三个纯文本文件，无构建、无打包脚本、无运行时下载，可直接逐行审计；权限面由 [`src-ui/edge-extension-contract.test.ts`](../src-ui/edge-extension-contract.test.ts) 静态钉住（含与 Rust 回调协议的字面量对齐）。

## 安装

前提：Windows、已安装 Microsoft Edge、kimi-switch GUI 可运行。

1. 在 Edge 打开 `edge://extensions`，开启「开发人员模式」。
2. 选择「加载解压缩的扩展」，选中本目录（`edge-extension/`，即包含 `manifest.json` 的目录）。
3. 在 kimi-switch 的某张账号卡上点击「绑定 / 重新绑定」→「从 Edge 自动读取并绑定」。程序会打开带一次性挑战的 Kimi 控制台页面。
4. 在 Edge 登录目标 Kimi 网页账号。扩展只在该挑战页面读取 `localStorage.access_token`，然后把它一次性发回本机 GUI，完成绑定。绑定结果会短暂显示在挑战页面。

安装不需要以任何特殊参数（包括远程调试）启动 Edge。

## 更新

仓库里的扩展代码变化后（拉取新代码或重新同步本目录）：

1. 打开 `edge://extensions`，找到「kimi-switch Edge 绑定助手」卡片。
2. 点击卡片上的「重新加载」；版本号以 `manifest.json` 的 `version` 为准。

未打包的扩展不会被 Edge 自动更新，kimi-switch 也不会在运行时下载或推送扩展代码；每次更新都由用户手动重新加载。

## 撤销

1. 打开 `edge://extensions`，在「kimi-switch Edge 绑定助手」卡片上点「移除」。
2. 撤销即收口，无需额外清理：
   - 每次绑定挑战都是 kimi-switch 进程内存里的一次性会话——90 秒自动过期，关闭绑定对话框立即取消，一次回调成功后立即作废。移除扩展后，仍在等待的挑战会照常超时收口，GUI 显示失败引导（「重试」或手动粘贴），不残留任何状态。
   - 扩展本身不保存 token 或其他数据（无扩展 storage 权限，代码里也没有存储访问），移除即干净。
   - 已绑定到账号卡的网页凭证保存在 kimi-switch 的本机私有凭证仓库，不依赖扩展存活；撤销只是移除「自动读取」通道，手动粘贴入口始终保留。

## 权限面与边界

manifest 权限逐条对照（以 `manifest.json` 为准）：

| manifest 声明 | 含义 |
|---|---|
| `permissions: []` | 不申请任何扩展 API：没有 tabs、cookies、storage、history、downloads、nativeMessaging 等 |
| `host_permissions: ["http://127.0.0.1/*"]` | 唯一网络权限是本机 loopback 回调，不含任何远端域名 |
| content script 只匹配 `https://www.kimi.com/code/console*` | 页面脚本只注入 Kimi 控制台路径，不进 iframe |
| 无 `web_accessible_resources`、无 `externally_connectable` | 网页无法主动调用扩展资源或消息入口 |

由此成立的边界声明：

- 不读取 cookie，不读 IndexedDB / sessionStorage，对 localStorage 只有一次只读访问（仅 `access_token`，不写不删）。
- 不碰其他标签页：没有 tabs 权限，页面脚本也只在控制台路径且带本次挑战时运行。
- 不扫描浏览器 profile：扩展只运行在用户安装它的那个 profile 里，不枚举、不访问其他 profile 或浏览器配置目录。
- 不保存网页凭证：token 只在扩展内部消息与一次性回调里出现，不进 URL、日志或扩展存储。
- 不静默安装、不运行时下载：安装、更新、撤销全部由用户在 `edge://extensions` 手动完成。
- 不要求远程调试模式：不以 `--remote-debugging-port` 等参数启动 Edge，不使用 CDP。

## 扩展不可用时的路径

未安装、被企业策略停用（如开发人员模式被策略禁用）、或站点权限被收回时，自动读取不会完成：GUI 绑定对话框在等待超时后显示失败状态，引导「重试」或在对话框下方手动粘贴网页 `access_token`。手动粘贴与自动读取走同一条绑定管道，效果一致。

## 分发状态

当前为项目内私有分发包（本目录源码），尚未提交 Edge Add-ons 商店审核。真实 Edge 交接、企业策略与 Local Network Access 尚未在目标机器核对；按 2026-09-23 的决定，这不再作为 #170 / #176 的关闭条件。本地测试不写成该项已通过。
