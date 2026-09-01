# 绑定网页登录：会员到期读取的凭证分离与身份门

「会员到期」指官方付费档位对这张账号失效的那一天。这份数据只存在于网页消费者
接口 `www.kimi.com` 的 `GetSubscription`（Connect RPC），`/usages` 与 Code OAuth
设备码流程都不携带它。issue #50（2026-09-01 实现）落地绑定管道：
CLI `bind-web <id|N>` 从 stdin 读网页 token，身份门通过后把会员到期写进账号
extra，默认列表即出现「有效期至」。展示格式化与读侧 schema 由前置票 #49 钉定，
本 ADR 只约束写入侧。

## 决策

1. **为何走 `www.kimi.com` 的 `GetSubscription`**：这是官方网页客户端自己渲染
   「有效期至」的数据源（`subscription.currentEndTime` / `nextBillingTime`），
   没有第二处权威来源。`/usages` 只有额度窗口与档位名，Code OAuth 的 token
   也不具备读取会员订阅的 scope。请求形态对齐官方网页：POST
   `/apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscription`，
   头 `connect-protocol-version: 1` + JSON 编码，UA `kimi-switch`（不用裸默认
   UA），超时 15s。域名是固定常量（`web_membership::MEMBERSHIP_BASE_URL`），
   不做环境变量覆写；本票的网络白名单因此扩为
   `auth.kimi.com` / `api.kimi.com` / `www.kimi.com`（最后者仅限本路径，
   见 AGENTS.md 规则 2，同一变更集修订）。请求节奏只有「绑定时一次」；
   后续刷新（何时重读、何时静默）是 #52 的决策空间，本票不预设。

2. **为何网页消费者凭证与 Code OAuth 分离存储**：网页 token 与 Code OAuth
   blob 是两套不同信任域的凭证（签发对象、scope、生命周期都不同），任何一侧
   覆盖另一侧都会造成「刷新一个丢一个」。存储上同账号各占凭证仓库一个 field：
   Code blob 走 provider 自报 `store_field`（默认 `"blob"`），网页 token 走独立
   常量 `kimi_switch_core::membership_expiry::WEB_STORE_FIELD`（`"web"`，core
   定义使 `remove_account` 无需签名变更即可清理，GUI 删除路径自动受益）。
   到期瞬间是非秘密元数据，写进账号 extra（#49 schema 的两个 RFC3339 UTC 键），
   不进凭证仓库。重新授权（`replace_account_blob`）只写 Code store_field 与
   live，同一账号 id 的网页绑定原样保留，也不在授权流程里打网页接口。
   `swap` / `activate` / 额度查询路径零改动、零网页请求（由引擎测试钉住）。

3. **为何不用浏览器 cookie**：cookie 是浏览器会话级凭证，读取它等于冒充用户
   的浏览器身份，超出用户明确授权的范围，且依赖抓取页面/存储布局，官方一改
   即坏。本设计只接受用户主动复制的网页 token（用户从网页端自己拿出来粘贴），
   授权边界清晰、可撤销（重新绑定即覆盖，`rm` 即删除）。

4. **为何不提供 `--token` 旗标**：命令行参数会进入 shell 历史与进程列表
   （ps / 任务管理器），等于把长期凭证抄写多份。token 只从 stdin 读：TTY 下
   `dialoguer::Password` 隐藏回显，管道/重定向按行读取（脚本可组合）。

5. **为何 `expireTime` 不是会员到期**：`GetSubscription` 响应里的
   `balances[].expireTime` 是各额度余额的月度重置周期，额度重置日与会员到期
   是两件事；一律忽略（解析只读 `subscription.currentEndTime` /
   `nextBillingTime`，夹具含 `expireTime` 干扰项并由测试钉住）。同理
   `goods.title` / `membershipLevel` 仅核对参考，不落库——档位展示仍以
   `/usages` 的 `user.membership.level` 为准，避免两处数据源打架。

## 身份门与写序

与按卡重新授权（ADR-0002）同型的门：解码网页 JWT payload 取 `sub`（回退
`user_id`），必须等于目标卡账号 id。不验签——只做本地身份比对，token 真伪由
`GetSubscription` 的 401 兜底。对不上或无法辨认 → 英文错误（不含 token）、
不写网页 field、不写 extra、不打网络（库存逐项不变，由管道测试钉住）。

写序固定：**先网页 field，后 extra**。身份门通过后先把网页 token 落库（此时
身份已验证，token 即有效绑定），再请求 `GetSubscription`；会员读取失败（网络/
HTTP）不回滚网页 field——到期日期可由重试或 #52 的刷新补齐，回滚反而把用户
刚验证过的凭证丢掉。extra 写入走「读出 Account → 只动两个 membership 键 →
registry upsert」，保留 dedup / scope 等既有键；导入/同步路径从 metadata 整体
重建 extra 的既有行为不变（重建后到期日缺失，展示回退绑定弱提示，等下次读取
补齐）。`None` 的瞬间移除对应键：重新绑定后旧日期不得残留。

取消续费但 active 的订阅照常返回到期日，不产生「已取消」语义（夹具
`SUBSCRIPTION_STATUS_CANCEL` + `active: true` 由测试钉住）。删除账号时
`remove_account` 连网页 field 一并清理，extra 到期键随 registry 行整体消失。
