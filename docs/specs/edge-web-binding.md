---
type: report
title: Edge 免粘贴绑定网页登录
tags: [spec, membership, bind-web, edge, extension]
intent: 定义 GUI 通过 Edge 扩展自动取得网页凭证并绑定会员到期信息的产品契约。
documents:
  endpoints:
    - POST /apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscription
  symbols:
    - bind_web
    - begin_web_bind
    - complete_web_bind
    - WEB_STORE_FIELD
---
# Edge 免粘贴绑定网页登录

## Problem Statement

当前“绑定网页登录”要求用户打开 Kimi 控制台，在浏览器开发者工具中找到 `localStorage.access_token`，复制后粘贴到 kimi-switch。这个流程容易误操作、暴露凭证，也让用户误以为网页凭证与 Kimi Code 的 Code OAuth 凭证是同一件事。

目标是让用户在指定账号卡上发起一次绑定后，由 Edge 当前控制台页面把网页凭证交给本机 kimi-switch，用户不再手动复制或粘贴；现有身份门、网页凭证独立存储和会员到期读取契约必须继续成立。

## Solution

GUI 提供“从 Edge 自动读取并绑定”。kimi-switch 为目标账号创建一个短时、一次性的绑定挑战，启动 Edge 打开 `https://www.kimi.com/code/console` 并在 URL fragment 中携带挑战信息。配套 Edge 扩展只对该控制台路径声明页面脚本权限；没有本次挑战 fragment 时不读取任何网页凭证。

当用户在 Edge 登录目标网页账号后，扩展从该页面的 `localStorage.access_token` 读取网页凭证，把它通过扩展后台发往本机一次性 loopback listener。token 不进入 Kimi 页面 URL、DOM、日志、扩展 storage、GUI Snapshot 或主 WebView。Rust 后端收到后复用现有 `bind_web` 管道：先做目标账号身份门，再写独立的 `web` field，最后请求固定的 `www.kimi.com` `GetSubscription` 并更新会员元数据。

Edge 扩展需要用户手动安装和撤销；首版为项目内可审计的私有分发包。自动读取失败、扩展未安装、Edge 未登录、挑战超时或用户取消时，GUI 仍提供原有手动粘贴入口。CLI `bind-web` 继续从 stdin 隐藏读取。

## User Stories

1. 作为账号库用户，我希望在某张账号卡上点击一次“从 Edge 自动读取并绑定”，以免手动复制网页 access token。
2. 作为账号库用户，我希望软件自动打开 Edge 的 Kimi 控制台，以便从空状态开始绑定。
3. 作为账号库用户，我希望自动流程只使用当前 Edge 配置文件和控制台页面，以免误读其他浏览器 profile 的凭证。
4. 作为账号库用户，我希望扩展只在带本次绑定挑战的控制台页面读取，以免打开普通控制台页面就触发凭证采集。
5. 作为账号库用户，我希望不需要再点击扩展按钮，以便整个绑定流程只由 kimi-switch 的一次操作启动。
6. 作为账号库用户，我希望 Edge 未安装扩展时看到可理解的失败状态，以便改用手动粘贴或按安装说明处理。
7. 作为账号库用户，我希望 Edge 未登录或网页没有 `access_token` 时绑定失败且旧绑定保持不变。
8. 作为账号库用户，我希望 Edge 当前登录身份与所选账号不一致时绑定被拒绝，以免把会员到期信息绑错账号。
9. 作为账号库用户，我希望重新绑定同一账号仍覆盖旧网页凭证，以便网页凭证轮换后能恢复会员到期读取。
10. 作为账号库用户，我希望绑定到另一账号时不会自动寻找或改绑其他账号卡，以便账号选择始终由我控制。
11. 作为账号库用户，我希望自动读取失败后仍能手动粘贴网页 access token，以便浏览器权限、企业策略或扩展故障不会阻断绑定。
12. 作为账号库用户，我希望取消绑定挑战不会写入 token、extra 或会员到期信息，以便取消是无副作用的。
13. 作为账号库用户，我希望挑战过期、重复回调和跨会话回调都被拒绝，以便旧页面或恶意请求不能消费新绑定。
14. 作为账号库用户，我希望网页凭证只保存在现有私有凭证仓库的 `web` field，以便它不与 Code OAuth blob 混淆。
15. 作为账号库用户，我希望会员接口失败时看到“网页凭证已保存但会员读取失败”的可区分状态，以便知道可以稍后刷新而无需再次绑定。
16. 作为账号库用户，我希望成功绑定后继续看到现有的会员到期展示和刷新行为，以便自动读取不改变产品语义。
17. 作为账号库用户，我希望删除账号时网页凭证和会员到期数据继续一并清理，以便自动绑定不会留下孤儿凭证。
18. 作为 CLI 用户，我希望 `bind-web` 继续支持 stdin 隐藏输入，以便脚本和无 GUI 环境不依赖 Edge。
19. 作为隐私敏感用户，我希望扩展不读取 cookie、不扫描其他标签页、不缓存 token、不向远端发送 token，以便自动化的授权范围可审计。
20. 作为维护者，我希望扩展和 Rust 回调协议可独立测试，以便 Edge 版本变化时能定位是页面提取、loopback 交接还是绑定管道的问题。
21. 作为发布维护者，我希望首版扩展可在项目内私有分发并由用户手动安装，以便先完成真实 Edge 验收，再决定是否提交 Edge Add-ons 商店审核。

## Implementation Decisions

- 自动路径只存在于 GUI；CLI 的 `bind-web` 继续从 stdin 读取，并保留不提供 `--token` 旗标的安全约束。
- Edge 扩展使用 Manifest V3。页面脚本匹配 `https://www.kimi.com/code/console*`，但只有检测到 kimi-switch 本次挑战 fragment 才读取 `localStorage.access_token`。页面脚本读取后立即清除 fragment。
- 挑战由 Rust 生成短时随机 nonce、目标账号会话和随机 loopback 端口。URL fragment 只携带挑战元数据，不携带网页凭证；挑战一次性消费，完成、取消、超时或失败后失效。
- 扩展服务 worker 把 token 发送到固定路径 `/kimi-switch/web-bind` 的 `http://127.0.0.1:<port>`。页面脚本不直接跨源请求；扩展不写 token 到 storage。loopback host permission 与 Kimi 页面脚本权限分开声明。
- Rust listener 只绑定 loopback，限制请求方法、路径、请求头和正文大小，校验 nonce 后立即关闭会话；无效请求不推进绑定状态。listener 不记录 token 或原始请求正文。
- GUI 只展示“打开 Edge、读取中、校验中、成功/失败”状态，不向 WebView 返回 token、JWT payload 或网页身份字段。
- 自动路径收到 token 后必须调用既有 `bind_web`：目标账号身份由 JWT payload 的 `sub`，回退 `user_id`，与目标账号 id 比对；不匹配或不可识别时不发会员请求、不写 `web`、不改 extra、不自动改绑。
- 通过身份门后沿用现行写序：先写 `web` field，再请求 `GetSubscription`，再更新会员到期和会员等级 extra。会员请求失败不回滚已验证的网页凭证，但必须通过 UI 状态与完整成功区分。
- `swap`、额度查询、静默轮询和启动流程不因本功能新增 `www.kimi.com` 请求；已绑定账号的既有用户刷新、导入/login 补读语义保持不变。
- Edge 扩展随项目提供可审计源码/构建产物和安装说明。首版允许用户手动安装、更新和撤销；不静默安装、不运行时下载、不扫描 profile、不要求远程调试模式。
- Edge 当前登录账号、浏览器 profile 和账号卡必须由用户在当前操作中选择；系统不遍历其他标签页或 profile。页面缺失时由 GUI 打开控制台，未登录/扩展缺失/读取失败转手动粘贴。
- 当前账号、停泊账号、自动切换、主动续期和 Code OAuth token 的语义不因本功能改变。

## Testing Decisions

- 测试只观察外部行为和现有公开 seam，不断言私有字段布局或实现细节。优先在 GUI 命令、Rust callback broker、既有 `bind_web` 管道和 Edge 扩展协议边界测试。
- Rust broker seam：挑战包含控制台 origin 和 fragment 元数据；有效 nonce + 合法 POST 只交付一次；错误方法、路径、nonce、大小、空 token、重复回调、过期和取消均不交付；目标账号串线拒绝。
- 既有 provider seam：继续覆盖身份不匹配不写不请求、身份匹配先写 `web`、会员请求失败保留网页凭证、无到期日清理旧 extra、删除账号清理网页 field，以及不把 Code OAuth blob 当网页凭证。
- GUI seam：自动绑定按钮启动 Edge 挑战并展示状态；成功展示现有 Snapshot；自动路径失败后手动粘贴仍可用；关闭/取消对话框清理挑战和输入框；token 不出 Snapshot 或错误日志。
- 扩展静态契约：Manifest 只包含控制台路径脚本和 loopback host permission；脚本没有挑战时不读取 storage；读取后清除 fragment；service worker 只向固定 loopback 路径发送，不接受任意 URL。
- CLI 回归：`bind-web <id|N>` stdin 行为保持，`--token` 仍被拒绝。
- 真实 Edge 验收必须覆盖：正确账号免粘贴绑定、另一账号身份门拒绝、未登录/无 token、扩展未安装、多个 Edge profile、用户取消、超时、重复回调、会员接口失败和手动兜底。单元测试或前端构建通过不能替代真实 Edge 验收。

## Out of Scope

- 读取浏览器 cookie、IndexedDB、浏览器配置目录、其他标签页或其他 profile。
- 后台轮询网页 token、自动续绑、自动续期、自动登录、自动切换账号或跨账号匹配。
- 把 Kimi Code 的 Code OAuth access token 复用为网页 `web` token。
- 在 Tauri WebView 内嵌 Kimi 登录页或放宽主窗口的远程页面信任边界。
- 要求 Edge 以远程调试参数启动，或实现 CDP/Playwright 读取日常浏览器。
- 静默安装/更新扩展、运行时下载扩展或首版直接承诺 Edge Add-ons 商店发布。
- 修改 CLI 的凭证输入契约、会员到期数据源、网页 field 命名、身份门和写入顺序。

## Further Notes

- 现有 ADR-0004 仍是网页凭证分离、身份门、写序和 `GetSubscription` 的权威来源；本 spec 和 ADR-0018 只新增“用户主动发起的 Edge Local Storage 提取”路径。
- 这是高风险凭证交接功能。发布报告必须把“候选实现、dry-run、构建通过、真实 Edge 交接、live verified”分开；未完成真实 Edge 验收不得宣称功能闭环。
- 真实 Edge 行为还需核实目标 Edge 版本的 loopback Local Network Access、CORS/no-cors、企业策略和扩展安装限制；这些是验收项，不应在单元测试中假定为已验证。
