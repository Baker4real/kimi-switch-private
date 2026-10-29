---
type: adr
title: 凭证侧：轮换写回比对与切换读取进锁
status: accepted
tags: [token-rotation, swap, swap-lock, credential-store, race]
intent: Record the credential-side decisions of spec 185, starting with ticket 186. Every token rotation the tool performs sends its network request outside any lock and commits inside the swap lock against the refresh token it consumed - the stored credential is replaced only while it still holds that token, the live credential is written only while the account is current, the live file is still that account's and still holds that token, store before live; a store write error still lets the live compare-and-write run (the store error is only warned when live took the new credential), and when the swap lock cannot be entered at all (3 s timeout, or the lock file cannot be created, opened or locked) the store is still compare-written while live is left alone, so a new credential is never dropped for a lock fault. The credential store gains an atomic read-compare-write (set_if, atomic in FileStore under its own lock). activate's target read, capture-on-leave and pre-swap identity move into the same swap-lock critical section as the write, post-swap verify and registry commit, so a switch never writes a pre-rotation credential and never misses an intermediate account's live credential; the legacy keychain migration of the target and the live owner happens before the lock, so in-lock reads only hit the credential file; manual swap stays network-free and never waits for an in-flight rotation. Ticket 187 routes the current account's proactive renewal through the same commit - switching away while a renewal is in flight no longer undoes the switch, and the renewed credential still lands in that account's store when the compare passes; dead-token fingerprint clearing and the renewal gate are unchanged. Ticket 188 routes the current account's 401 coordinated recovery through the same commit while it still holds the official refresh lock (official refresh lock -> swap lock, the official lock released only after the commit) - switching away while a recovery is in flight no longer overwrites the live credential, and the new credential lands in the original account's store by compare; dead-token fingerprint discipline is unchanged, and the refresh is sent only while the quota attempt's remaining time passes the same budget gate as the parked forced rotation (forced_rotation_fits), otherwise the lock is released and the 401 stays unrecovered. Ticket 189 makes every live-to-store backfill (capture-on-leave including the post-swap salvage, the incidental backfill when reading the current account's credential, and the adoption branch of proactive renewal) a compare-and-write through set_if - the same refresh token writes, a store without a refresh token is written, different refresh tokens compare expiry (access token JWT exp first, then expires_at) and a strictly older live never overwrites the store, either side without an expiry keeps live winning, the guard for a live without a refresh token now covers all three, and live is never repaired; this revises ADR-0002 to newer-wins-when-comparable. Ticket 190 makes parked rotation on-demand - a parked account rotates before a quota query only when its access token has less than max(300 s, expires_in x 0.5) left (the same credential_expiry hook as the backfill compare; an unparsable expiry or a missing access token still rotates first), and a 401 before that forces one rotation plus one retry; both rotations commit through the writeback compare. The forced rotation is sent only while the quota attempt's remaining time (deadline published by query_quota_with_retry) covers the runtime's declared refresh timeout plus 2 s, otherwise the first 401 is returned and the next query retries with a full budget, so the per-attempt timeout can never cancel a refresh whose request was already sent.
documents:
  symbols:
    - commit_rotation_with
    - store_rotation_if_unchanged
    - live_still_holds_consumed
    - set_if
    - with_swap_lock
    - renew_active_proactively
    - recover_active_401_at
    - commit_rotation_and_release
    - backfill_live_into_store_with
    - backfill_verdict
    - migrate_legacy_copies_before_swap_lock
    - credential_expiry
    - parked_rotation_due
    - forced_rotation_fits
    - attempt_time_left
    - with_attempt_deadline
    - refresh_timeout
---
# 凭证侧：轮换写回比对与切换读取进锁

spec #185 的凭证侧决策记录。#186 落地轮换写回比对、凭证存储的原子比对写和 activate 读取进锁；
#187 把当前账号主动续期改走同一提交规则（决策 6）；#188 把 401 协调恢复改走同一提交规则
（决策 7）；#189 让 live 回灌账号库先比新旧（决策 8）；#190 把停泊账号改为按需轮换（决策 9）。
自动切换侧决策记在 ADR-0020。

## 背景

spec #185 问题 1：后台额度查询正在为某个停泊账号做 token 轮换（请求已发出，服务端已消耗旧
refresh token）时，用户切到这个账号（ADR-0015 决策 2 允许后台重列期间立刻再切）。切换在交换锁
之外读到轮换前的库存凭证，写进当前登录文件；轮换结果随后只写回库存，当前登录文件留着已作废的
refresh token；下一次读取当前账号凭证又以 live 为准回灌库存（ADR-0002 修订），新凭证的唯一副本
被销毁。几分钟后官方客户端续期被拒，账号只能重新授权。复现用例修复前为红：放行后当前登录文件
仍持被消耗的 refresh token。

同源的另一处：activate 的目标凭证读取、capture-on-leave 与切换前身份记录都在交换锁之外。两个
切换交错时，后一个切换的 capture-on-leave 在前一个切换提交之前就已跑完；前一个切换的账号在 live
里被官方客户端轮换出的新凭证，随后被后一个切换覆盖，没有回灌进库存。

spec #185 问题 2（#190）：停泊账号每次额度查询前都轮换（ADR-0003「查询前必轮换」），GUI 开着时
每个停泊账号约 40 次/小时。每次轮换都消耗一次性 refresh token，多一次就多一次「响应丢失即掉号」
的机会，也放大了上面的竞态窗口。官方客户端自己只在剩余不足 max(5 分钟, 半寿命) 时才刷新。

## 决策

### 1. 轮换写回比对

本工具做的 token 轮换，网络请求在锁外完成，之后进入交换锁提交，对照「本次消耗的 refresh
token」：

1. 重读该账号的库存凭证：refresh token 仍等于被消耗的那个，才写入新凭证；否则放弃库存写入并记
   warn。「否则」包括重新授权、另一进程的轮换、回灌已经更新过它，也包括库存里已没有这一项
   （账号已删除，不复活）。
2. 重读 registry 当前标记与当前登录文件：该账号是当前账号、当前登录文件身份是它、且其 refresh
   token 仍等于被消耗的那个，才把新凭证原子写进当前登录文件（tmp + rename + 文件级 fsync，沿用
   切换写法）。当前登录文件已经换人或已被别处更新时，绝不覆盖它。两步各自判定。
3. 写序固定先库存后 live（ADR-0002）。

比对只看 refresh token，不比整串：同一凭证的序列化差异（字段顺序、`expires_at`）不得导致误判。
本次查询照用轮换得到的新凭证，不论库存是否因比对不符放弃了写入。

库存一步出错（凭证存储 IO 故障）不跳过第 2 步：照样比对并写当前登录文件，此刻它可能是新 refresh
token 唯一能落的地方。第 2 步写进了当前登录文件，新凭证已有归宿：库存错误只记 warn，提交返回成功，
本次查询照用新凭证，之后的回灌新旧比对（决策 8）把它补回库存。第 2 步没写（不是当前账号、当前
登录文件已换人或已更新）或同样出错：如实返回库存错误。

覆盖的写回点：停泊账号额度查询前的轮换（#186；#190 起只在临期时轮换，含未临期遇 401 的强制
轮换，见决策 9）、当前账号主动续期（#187，见决策 6）、当前账号的 401 协调恢复（#188，见决策 7）。

### 2. 凭证存储的原子「读 → 比对 → 写」

`CredentialStore::set_if`：读出当前值交给调用方判定，判定通过才写入，返回是否写入；比对口径由
调用方决定。`FileStore` 在自身排他锁（`<path>.lock`）下完成读、判定、写，`set` 走同一实现。
它只比对凭证文件里的当前值，锁内不读 legacy 钥匙串：迁移由 `get` 负责，钥匙串可能弹授权框，
持锁等它会卡住其他凭证读写。交换锁内同样不碰钥匙串：activate 进交换锁之前先经 `get` 把要读的
库存副本迁进凭证文件（决策 4）。trait 的默认实现读写之间不持锁，只给没有锁可用的后端兜底；生产
装配是 `FileStore`。

### 3. 交换锁进不去

提交等交换锁最多 3 秒（ADR-0015 决策 4）。等不到，或锁文件建不了、打不开、加锁报非竞争类错误
（如 sudo 跑过留下的 root 属主锁文件）时，临界区一步都没执行：放弃写当前登录文件，库存仍在凭证
存储自身的锁下按比对写入，记 warn，查询照常完成。新凭证绝不因交换锁不可用而丢弃。临界区进去之后
出的错按决策 1 处理，不再回退。

### 4. activate 读取进锁

activate 的目标凭证读取、capture-on-leave、切换前身份记录（供抢救判定，ADR-0015 决策 7），与
写入、写后回读校验、registry 提交放进同一段交换锁临界区。交换锁按线程可重入，切换骨架
（`swap_with_snapshot_protected`）与 registry 写入在这段临界区里照常进锁。capture-on-leave 失败
仍只 warn、不阻断切换（ADR-0015 决策 11）。

legacy 钥匙串迁移在锁外：进交换锁之前，先经 `get` 读一次目标账号与当前登录文件所属账号
（capture-on-leave 的回灌对象）的库存，还在钥匙串里的副本借此迁进凭证文件，锁内的读取只命中凭证
文件。钥匙串可能弹授权框，持交换锁等它会让同时到来的轮换提交回退为只写库存（决策 3）、手动切换
等锁超时。预读与进锁之间若有轮换提交，它写的是凭证文件，锁内读取照样看得到。生产装配挂 legacy
钥匙串回退（`FileStore::with_legacy_keyring`），这一步对它生效。

切换与轮换提交因此串行：轮换先提交，切换读到的就是新凭证；切换先提交，轮换提交看到该账号已是
当前账号、当前登录文件仍持被消耗的 token，把新凭证同步进去。两个切换交错时，后一个切换在锁内
回灌的是前一个切换之后的 live。

手动切换零网络、不等待任何在途轮换的红线不变：轮换的网络请求不持锁，切换至多在另一次提交的
本地写入上等毫秒级。

### 5. 锁顺序

官方刷新锁（如有）→ 交换锁 → 凭证存储锁。切换路径从不拿官方刷新锁，凭证存储从不拿交换锁，
不会死锁。交换锁内不做网络请求。

### 6. 当前账号主动续期的写回（#187）

spec #185 问题 1 的另一处：开启主动续期（ADR-0003 第 2 条）时，续期请求已发出、用户切到了别的
账号，续期结果原先先写库存、再无条件写当前登录文件，把刚切走的账号写回去，静默撤销切换（registry
记的当前账号与当前登录文件从此对不上）。复现用例修复前为红：放行后当前登录文件是原账号的续期结果。

现在续期轮换成功后按决策 1 提交，被消耗的凭证是续期前现读的当前登录文件：

- 续期在途时切走了：当前登录文件已经换人，不写它，切换不被撤销。切走时 capture-on-leave 回灌进
  库存的仍是被消耗的那份，库存比对通过，续期得到的新凭证照样落进原账号的库存，下次切回即用它。
- 续期期间没有发生切换：与原先一致，先库存后 live，live 原子写。
- 当前登录文件已不再持被消耗的 token（被官方客户端等别处更新过）时同样不覆盖它；原先续期结果会
  无条件盖上去。交换锁进不去时按决策 3 只写库存、不碰当前登录文件。
- 死 token 指纹的清除规则不变：提交完成后只清掉被消耗那份凭证的拒止指纹；续期被拒仍记指纹停试。
- 主动续期门（开关 × 锁探测 × 临期 60 秒）不变。官方已抢先轮换时直接采纳当前登录文件的分支不走
  本规则，那里没有本工具的轮换结果要提交。

### 7. 401 协调恢复的写回（#188）

401 协调恢复只在有官方刷新锁的客户端组合上运行：非 Windows 上的 TS 客户端，以及任意平台上
1.31 及以上的 Python 客户端。它持官方刷新锁完成网络刷新，再在交换锁内经轮换写回比对
（`commit_rotation_with`，与停泊轮换、主动续期同一实现）提交，提交完才释放官方刷新锁：

- 写当前登录文件前再确认三点：当前登录文件仍属原账号，仍持被消耗的 refresh token，原账号仍是
  registry 当前账号。三点都成立才写它。
- 任一项不符（恢复在途时切走了）：不碰当前登录文件，新凭证按比对写进原账号的库存凭证。
- 未发生切换时照常写当前登录文件。库存仍持被消耗的 token 时先替换库存（先库存后 live，
  ADR-0002）；此前这条路径只写当前登录文件。
- 交换锁进不去时同决策 3：只按比对写库存，不碰当前登录文件。
- 死 token 指纹纪律不变：刷新被拒时记录指纹并停试，轮换成功后清掉被消耗 token 的指纹。
- 本次查询照用新 access token 重试，不论新凭证落在当前登录文件还是库存。
- 预算门与停泊账号的强制轮换（决策 9）同一条规则、同一个函数（引擎公开的
  `forced_rotation_fits`）：等官方刷新锁最多 15 秒，可能已用掉本次 attempt 的大半预算。拿到锁、
  发刷新前核对剩余时间，不够轮换超时上限（`refresh_timeout`，10 秒）加 2 秒就不发，放锁按
  未恢复的 401 返回（照旧归类为凭证过期型 401 或裸 401），留给下次查询。

官方刷新锁在提交所在的阻塞任务里释放，查询被取消也不会在写回途中提前放锁。Kimi adapter
（`KimiRuntime`）持有与引擎同一份凭证存储和注册表，经引擎公开的 `commit_rotation_with` 提交，
不另写一套比对。

### 8. 回灌新旧比对（#189）

live → 账号库的回灌原先一律以 live 为准。库存已经持有较新的凭证、当前登录文件却还是较旧的那份时，
之后一次读取当前账号凭证或切走时的 capture-on-leave，都会用旧 live 覆盖库存，销毁一次性 refresh
token 的唯一有效副本。这样的窗口有两个来源：重新授权写入库存后 live 写失败（ADR-0002 已接受的
边界），以及轮换提交只写了库存（交换锁进不去，决策 3；主动续期 live 写失败，ADR-0003 已接受的
边界）。复现用例修复前为红：重新授权窗口期内的查询与切换都把库存撤回旧凭证。

现在三处回灌共用一条规则，先比对再写：capture-on-leave（含写后回读校验的抢救，ADR-0015 决策 7）、
读取当前账号凭证时的顺手回灌、主动续期发现官方已抢先轮换时的采纳回灌。从当前登录文件导入
（GUI 导入、`login kimi`，`import_active`）同样走这条规则：跳过写入时保留库存、记 warn，registry
照常更新；设备码授权等新授权结果（`import_raw`）照旧直接覆盖。

1. live 缺 refresh token 而库存有：跳过。这是 capture-on-leave 的既有守卫（官方墓碑化的登录文件
   不污染账号库），现在三处回灌都适用。
2. 两份凭证的 refresh token 相同：照写，同一凭证的其他字段跟着同步。库存没有 refresh token：照写，
   没有一次性 token 要保护。
3. 两份都持 refresh token 且不同：比较过期时刻。过期时刻优先取 access token 的 JWT `exp`，缺失时
   回退凭证里的 `expires_at`（`FileBlobRuntime::credential_expiry`，Kimi 适配器实现，默认解析不出）。
   live 严格较旧：不覆盖库存，记 warn。live 更晚或相等、任一方解析不出：维持现状，live 覆盖。
4. 读、比对、写在凭证存储自身的锁下一次完成（`set_if`，决策 2），不需要交换锁。读取当前账号凭证
   时的回灌在交换锁外，与锁内的轮换提交也不会互相覆盖。`set_if` 锁内只看凭证文件，所以比对前先经
   `get` 读一次库存，让 legacy 钥匙串里还没迁移的副本先落进凭证文件（原先守卫就靠 `get` 看到它）。
   交换锁内的回灌（capture-on-leave 与抢救）由 activate 在进锁前预读过（决策 4），锁内这次 `get`
   只命中凭证文件。
5. 只防覆盖，不修 live：库存较新时不主动改写当前登录文件。读取仍返回当前登录文件，本次查询照用
   它；新凭证在切走再切回时进入当前登录文件。

重新授权的写序（先库存后 live）不变。ADR-0002「窗口期一律以 live 为准」因此修订为「可比较时新者胜，
不可比较时以 live 为准」。

### 9. 停泊按需轮换（#190）

停泊账号额度查询前，只在 access token 临期时先轮换再查询：剩余不足 max(300 秒, `expires_in` × 0.5)，
与官方 OAuth 管理器同一阈值（spec #185 外部证据），已过期同样算临期。

- 过期时刻与决策 8 同一口径：优先取 access token 的 JWT `exp`，缺失时回退凭证的 `expires_at`；
  `expires_in` 缺失时阈值取 300 秒。
- 凭证里没有 access token、或过期时刻解析不出：维持旧行为，先轮换再查询。
- 未临期而额度查询返回 HTTP 401（服务端提前作废了 access token）：强制轮换一次，再重试额度查询
  一次。
  - 强制轮换被拒按既有 DeadToken 指纹纪律处理：记录指纹，本次不重试查询，返回首个 401；指纹在案
    之后再遇 401 也不再轮换。
  - 重试仍是 401：原样返回。
  - 两种都按既有停泊 401 口径呈现（裸 401，即额度查询失败），不归为凭证待续期。
  - 发出强制轮换前核对本次查询的剩余时间：至少是轮换请求的超时上限加 2 秒余量才发；不够就不发，
    返回首个 401（同样按停泊 401 口径呈现），留给下次查询用完整预算再强制轮换。
- 临期轮换与强制轮换都经决策 1 的轮换写回比对提交。
- 当前账号的路径不变，主动续期门（开关 × 锁探测 × 临期 60 秒，ADR-0003）不动。
- 阈值只写在本 ADR 与 CONTEXT.md，不进配置（AGENTS 规则 3）。

分工：runtime 经决策 8 的同一个钩子 `FileBlobRuntime::credential_expiry` 报告过期信息，返回过期
时刻与 `expires_in`（Kimi 读 JWT `exp`、`expires_at` 与 `expires_in`）；阈值判定在引擎
（`parked_rotation_due`），凭证里没有 access token 时引擎不问钩子、直接先轮换。不提供这个钩子的
runtime 返回 `None`，与旧版一样每次查询前都轮换。

强制轮换的预算门：旧行为在单次查询开头轮换，轮换请求的 10 秒超时总落在 20 秒单次预算
（`quota.fetch_timeout_ms` 默认值）之内，外层超时从不在轮换响应返回前取消它。强制轮换排在一次
401 查询之后，起步时预算可能已所剩无几；照发就让「请求已发出、服务端已消耗一次性 refresh token、
响应被外层超时丢掉、只能重新授权」变得可达。机制：

- `query_quota_with_retry` 的每次 attempt 经 `quota_query::with_attempt_deadline` 运行：它既施加
  外层超时，也经 task-local 公布超时时刻，引擎经 `quota_query::attempt_time_left` 读剩余时间。
- runtime 经 `FileBlobRuntime::refresh_timeout` 声明轮换请求发出后最多多久必定返回。Kimi 为 10 秒：
  连接到响应体读完都在这一个超时里，代理回退直连也只用剩余时间（ADR-0005）。
- 判定在引擎（`forced_rotation_fits`）：剩余时间 ≥ 上限 + 2 秒才发。2 秒余量盖住发请求前的本地
  准备与调度抖动。没有外层超时（不经 `query_quota_with_retry` 的调用）时照发；有外层超时而 runtime
  说不出上限时不发（fail-closed）。
- 轮换响应一返回，提交即交给 `spawn_blocking`（决策 1）：外层超时这时取消查询也打断不了它。所以
  只要响应在超时前返回，新凭证就一定落地。

估算：15 分钟 access token（`expires_in` = 900）的阈值是 max(300, 450) = 450 秒。按 90 秒的停泊
查询节奏，轮换后第 6 次查询（第 540 秒，剩 360 秒）才临期，每个停泊账号的轮换从约 40 次/小时降到
约 7 次/小时（3600 ÷ 540 ≈ 6.7）。额度查询节奏、适应性轮询档位、`/me` 后台补缓存与会员档位补章
都不变。

## 剩余风险

- 轮换结果同步进当前登录文件之前，官方客户端若已用旧 token 续期被拒并写入墓碑，该会话需要
  重新登录；库存保有新凭证，下次切回即可恢复。官方 100ms 重读采纳机制能覆盖大部分交错。
- 交换锁进不去、且该账号此刻是当前账号时（停泊轮换期间被切成当前账号，或当前账号主动续期、
  401 协调恢复），当前登录文件留着被消耗的 token：官方客户端下次续期会被拒，该会话需要重新登录，
  或切走再切回。库存里的新凭证由回灌新旧比对（决策 8）保住：轮换结果比被消耗的那份过期得晚，
  之后读取当前账号凭证与切走时的 capture-on-leave 都不再用旧 live 覆盖它。任一方解析不出过期时刻
  时仍以 live 为准，新凭证会被覆盖。Kimi 的 access token 是带 `exp` 的 JWT（主动续期门与凭证
  过期型 401 的判定都依赖它），这一支要等凭证格式变了才会出现。
- 库存一步写入出错、而该账号此刻不是当前账号（或当前登录文件已换人、已更新）时，新凭证两边都
  没落下，提交如实报错；服务端已消耗旧 refresh token，该账号要重新授权。库存可写时这一支不出现。
- 停泊账号未临期期间，refresh token 若被别处消耗（如跨设备共用同一账号，spec #185 Out of
  Scope），要到下一次临期轮换才被拒并记录指纹；在此之前它仍可能成为自动切换的停泊候选。
- 临期轮换仍排在单次查询开头，靠 `quota.fetch_timeout_ms`（默认 20 秒）大于轮换请求的 10 秒超时，
  保证外层超时不在响应返回前取消它。用户把单次超时调到 10 秒左右或更低时，这一处保留 #190 之前就有的
  暴露面（旧行为每次查询都先轮换）。当前账号主动续期同样排在查询开头，不在本票范围。
- 预算门只认 `with_attempt_deadline` 公布的超时时刻。以后若有调用方在 `Provider::query_quota`
  外面另加超时，必须经它加（见 `attempt_time_left` 的文档），否则强制轮换与 401 协调恢复会当作
  没有外层超时照发。
- 慢 401 之后的强制轮换被推迟到下次查询：该账号这一轮仍是 401，并进 90 秒鉴权退避（ADR-0009），
  退避结束后的查询拿到完整预算再强制轮换。

## 验收

引擎 FakeRuntime 新增可暂停轮换闸门 `RotationGate`：轮换请求发出后停住，由测试放行。竞态用例
靠它构造「轮换在途 × 切换」交错，不靠 sleep 赌时序。activate 的交错用例在「已调用 activate、
尚未拿到交换锁」这一刻，经测试探针插入另一段持锁写入（`before_next_activate_lock`）。

`crates/providers/common/src/engine.rs`：

- `parked_rotation_in_flight_then_switch_to_it_lands_new_token_in_live_and_store`：复现用例，修复前
  为红。
- `parked_rotation_in_flight_then_switch_elsewhere_keeps_live_and_stores_new_token`
- `parked_rotation_writeback_yields_when_store_was_updated_meanwhile`：修复前为红，重新授权的结果被
  覆盖。
- `parked_rotation_commit_without_swap_lock_still_stores_new_token`：交换锁一直被占时不写 live，
  库存按比对写入。
- `parked_rotation_commit_when_swap_lock_cannot_be_entered_still_stores_new_token`：锁文件路径被
  目录占住、打不开时同样只写库存。修复前为红：只有等待超时才回退，新凭证被丢弃。
- `parked_rotation_store_write_error_still_lands_new_token_in_live`：库存写入出错时当前登录文件仍
  拿到新凭证。修复前为红：库存错误直接返回，两边都只剩被消耗的 token。
- `activate_reads_target_credentials_inside_swap_lock`：修复前为红。
- `interleaved_switches_capture_on_leave_keeps_intermediate_account_credentials`：修复前为红。
- `activate_still_switches_when_capture_on_leave_fails`
- `activate_migrates_legacy_keyring_copies_before_taking_swap_lock`：目标账号与当前登录文件所属
  账号的库存都还在 legacy 钥匙串里，探针在钥匙串访问时另开句柄 try-lock 交换锁。修复前为红：两次
  钥匙串访问都发生在交换锁内。
- `proactive_renewal_in_flight_then_switch_away_keeps_switch_and_stores_new_token`（#187）：复现
  用例，修复前为红，当前登录文件被写回原账号的续期结果。
- `reauth_live_write_failure_window_then_query_quota_keeps_new_credentials_in_store`、
  `reauth_live_write_failure_window_then_activate_keeps_new_credentials_in_store`（#189）：复现用例，
  修复前为红，重新授权窗口期内的查询与切换把库存撤回旧凭证。它们由原先钉住 ADR-0002 已接受行为的
  两个用例改写，凭证带上了过期时刻；切换用例最后切回该账号，当前登录文件拿到重新授权的结果。
- `proactive_renewal_adoption_keeps_newer_stored_credentials`（#189）：修复前为红，采纳回灌用较旧的
  live 覆盖了库存。
- `import_active_keeps_newer_stored_credentials`：修复前为红，从当前登录文件导入无条件覆盖库存，
  较旧的 live 盖掉了较新的凭证。
- 回灌比对各分支（#189，经 capture-on-leave 通道）：`backfill_skips_live_older_than_stored_credentials`、
  `backfill_overwrites_when_live_is_newer_or_equally_new`、
  `backfill_lets_live_win_when_either_side_has_no_expiry`、
  `backfill_writes_earlier_expiring_live_when_stored_refresh_token_is_same_or_absent`。
- `backfill_guard_sees_stored_copy_not_yet_migrated_from_legacy_keyring`（#189）：去掉比对前那次
  `get` 时为红，缺 refresh token 的 live 盖掉了还在 legacy 钥匙串里的库存副本。

`crates/providers/kimi/src/kimi_files.rs`：
`credential_expiry_prefers_jwt_exp_then_falls_back_to_expires_at`（#189）。

`crates/core/src/store.rs`：`set_if_writes_only_when_caller_accepts_current_value`、
`set_if_is_atomic_across_concurrent_writers`（把 `FileStore::set_if` 换成读写之间不持锁的实现时，
三次运行都只剩 10/40 次递增）。

`crates/providers/kimi/src/oauth.rs`（#188；Python 文件锁协议，不加平台门控，Windows 本机可跑）：

- `recovery_in_flight_then_switch_keeps_live_and_stores_new_token`：复现用例，修复前为红，恢复结果
  覆盖了已切成另一账号的当前登录文件。本地 OAuth 替身收下刷新请求后停住，测试在交换锁内完成
  切换再放行。
- `recovery_without_switch_writes_live_and_store`
- `recovery_too_late_in_attempt_leaves_refresh_to_next_query`：attempt 只剩轮换超时上限加 1 秒时
  不发刷新、放开官方刷新锁、返回未恢复。加预算门前为红：照发刷新并返回新 access token。

既有 401 恢复、DeadToken 指纹、取消后释放锁用例保持绿；只在非 Windows 编译的几例只改了调用参数。

既有用例保持绿：写后回读校验、一次性 token 抢救、快照回滚、fsync 注入失败、启动一致性自检、
切换零网络断言；主动续期三分支与先库存后 live 的写序、DeadToken 指纹、续期门纯判定
（`proactive_renewal_wanted_decision_pins_all_branches`）；capture-on-leave 守卫三态（#189 后守卫
经同一回灌函数生效）。

停泊按需轮换（#190），`crates/providers/common/src/engine.rs`（FakeRuntime 新增额度查询计数、
「这些 access token 查询返回 401」、首次额度查询延迟与轮换响应延迟）：

- `parked_query_before_expiry_does_not_rotate`：修复前为红，轮换计数为 1。
- `parked_query_near_or_past_expiry_rotates_first`
- `parked_query_without_access_token_rotates_first`：`expires_at` 未临期但没有 access token，照样
  先轮换（钩子与决策 8 合并后由引擎把关）。
- `parked_rotation_threshold_is_max_of_300s_and_half_lifetime`：阈值边界，恰好等于阈值不算临期。
- `parked_401_before_expiry_forces_one_rotation_and_retries_once`：修复前为红，401 原样返回。
- `parked_forced_rotation_rejected_records_fingerprint_and_stops_retrying`
- `parked_retry_still_401_keeps_parked_401_presentation`
- `parked_forced_rotation_in_flight_then_switch_to_it_lands_new_token_in_live_and_store`：强制轮换
  同样走轮换写回比对。
- `parked_slow_401_leaves_forced_rotation_to_next_query_with_full_budget`：慢 401，走生产的
  `query_quota_with_retry`、暂停时钟，401 在 20 秒预算用掉 15 秒后才到。修复前为红：剩 5 秒照发
  强制轮换，外层超时在响应返回前取消查询，重试的 attempt 又拿同一 refresh token 轮换一次（轮换
  计数为 2，真实服务端上第二次必被拒）。
- `forced_rotation_fits_only_when_time_left_covers_refresh_timeout_plus_margin`：预算门边界。

`crates/core/src/quota_query.rs`：`attempt_time_left_is_published_only_inside_an_attempt`。

Kimi 的过期信息由上面 #189 的 `credential_expiry_prefers_jwt_exp_then_falls_back_to_expires_at`
覆盖，#190 在其中补了 `expires_in` 的断言。没有过期信息仍先轮换由既有 `query_quota_refreshes_parked_account_and_persists_rotation` 覆盖；停泊
DeadToken 指纹用例与上面的竞态用例保持绿。

## Considered Options

- **轮换整段持交换锁（含网络请求）**：刷新请求超时上限 10 秒，远超交换锁 3 秒等待上限，切换会在
  轮换期间超时失败，违背「手动切换永远可用、毫秒级」。
- **切换等在途轮换完成再写**：切换开始依赖网络时延，同样违背上一条。
- **进程内互斥协调轮换与切换**：GUI 与 hook 唤起的 CLI 是不同进程，进程内锁协调不了；交换锁本来
  就是跨进程文件锁。
- **比对整串**：同一凭证的序列化差异会被误判为「已被别处更新」，放弃写入等于丢掉新凭证。
- **只要账号是当前账号就写当前登录文件**：会覆盖官方客户端或别处刚写进去的更新凭证。要求当前
  登录文件仍持被消耗的 token，覆盖的才一定是已作废的那份。
- **交换锁等不到就整体放弃写回或报错**：服务端已消耗旧 refresh token，丢掉轮换结果即丢号，违背
  凭证安全最高优先级。
- **持久化同步标记或版本号**：要改存储格式（ADR-0002 修订曾因此否决方案 A）；对照被消耗的
  refresh token 已足够判定，不引入新的持久状态。
- **只把目标凭证读取进锁，capture-on-leave 留在锁外**：两个切换交错时仍会漏回灌中间账号的当前
  凭证。
- **401 协调恢复只写当前登录文件，比对不符才写库存**：与恢复原本的写法一致，库存出错也挡不住
  当前登录文件；但轮换写回比对会多出第二套口径。改走同一份 `commit_rotation_with` 后，未发生
  切换时库存也拿到新凭证；库存出错仍照写当前登录文件（决策 1），原写法的这点好处保留。
- **库存出错就整体返回错误**：先库存后 live 的写序里，库存一步出错即返回会跳过 live，当前账号的
  新 refresh token 两边都落不下。
- **回灌不可比较时拒灌（库存优先）**（#189）：官方客户端合法刷新、live 领先库存是主场景
  （ADR-0002 修订否决朴素方案 A 的理由），解析不出过期时刻就拒灌会把这类新凭证挡在库存外，切走时
  随 live 被覆盖而丢失。不可比较时维持以 live 为准，新规则不引入新的失败。
- **只比 refresh token，不同就拒灌**（#189）：官方每次续期都换 refresh token，这样会挡掉所有合法
  刷新。refresh token 只用来认出「同一份凭证」，新旧要靠过期时刻判断。
- **库存较新时主动把它写回当前登录文件**（#189）：这是切换之外的一次 live 写入，要与官方客户端的
  刷新协调（ADR-0003），超出本票；只防覆盖已经保住新凭证，切走再切回即生效。
- **停泊轮换阈值做成配置项**：扩大配置面（AGENTS 规则 3）；跟随官方 OAuth 管理器的阈值即可。
- **停泊轮换沿用主动续期的临期 60 秒**：与官方口径不一致；余量太小，本机与服务端时钟有偏差时，
  更多查询要先撞 401、再靠强制轮换兜底。
- **未临期遇 401 不强制轮换**：服务端提前作废 access token 时，该账号要一直查询失败到临期
  （15 分钟 token 最长约 7 分半），期间还进 90 秒鉴权退避（ADR-0009）。
- **强制轮换后多次重试**：每次轮换都消耗一次性 refresh token；新 access token 仍 401 说明问题不在
  access token，再轮换只会多消耗。
- **强制轮换移出单次查询超时（另起后台任务）**：CLI 进程退出、运行时关闭时后台任务照样被丢弃，
  还要给轮换结果另找交接路径；只为这一处改轮换的任务所有权模型，不划算。
- **引擎按配置的单次超时自己推算剩余时间**：引擎与重试包装之间形成隐式耦合，包装改了预算、或另有
  调用方加了别的超时，推算就悄悄失真。公布真实的超时时刻更直接。
- **剩余时间不够就缩短轮换请求的超时**：请求一旦发出，超时只会更早放弃响应，服务端照样消耗了
  refresh token，等于主动制造「响应丢失即掉号」。
- **停泊按需轮换另设一个过期钩子**（#190）：与回灌新旧比对读的是同一组字段（JWT `exp` 优先、回退
  `expires_at`），两个钩子迟早口径漂移；合成一个钩子，多带一个 `expires_in`。
