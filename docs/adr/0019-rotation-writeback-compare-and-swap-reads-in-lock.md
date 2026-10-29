---
type: adr
title: 凭证侧：轮换写回比对与切换读取进锁
status: accepted
tags: [token-rotation, swap, swap-lock, credential-store, race]
intent: Record the credential-side decisions of spec 185, starting with ticket 186. Every token rotation the tool performs sends its network request outside any lock and commits inside the swap lock against the refresh token it consumed - the stored credential is replaced only while it still holds that token, the live credential is written only while the account is current, the live file is still that account's and still holds that token, store before live, and on a 3 s swap-lock timeout the store is still compare-written while live is left alone, so a new credential is never dropped. The credential store gains an atomic read-compare-write (set_if, atomic in FileStore under its own lock). activate's target read, capture-on-leave and pre-swap identity move into the same swap-lock critical section as the write, post-swap verify and registry commit, so a switch never writes a pre-rotation credential and never misses an intermediate account's live credential; manual swap stays network-free and never waits for an in-flight rotation. Ticket 187 routes the current account's proactive renewal through the same commit - switching away while a renewal is in flight no longer undoes the switch, and the renewed credential still lands in that account's store when the compare passes; dead-token fingerprint clearing and the renewal gate are unchanged. Ticket 189 makes every live-to-store backfill (capture-on-leave including the post-swap salvage, the incidental backfill when reading the current account's credential, and the adoption branch of proactive renewal) a compare-and-write through set_if - the same refresh token writes, a store without a refresh token is written, different refresh tokens compare expiry (access token JWT exp first, then expires_at) and a strictly older live never overwrites the store, either side without an expiry keeps live winning, the guard for a live without a refresh token now covers all three, and live is never repaired; this revises ADR-0002 to newer-wins-when-comparable. Later credential-side tickets of spec 185 (401 coordinated recovery, parked on-demand rotation) extend this record.
documents:
  symbols:
    - commit_rotation_with
    - store_rotation_if_unchanged
    - live_still_holds_consumed
    - set_if
    - with_swap_lock
    - renew_active_proactively
    - backfill_live_into_store_with
    - backfill_verdict
    - credential_expiry
---
# 凭证侧：轮换写回比对与切换读取进锁

spec #185 的凭证侧决策记录。#186 落地轮换写回比对、凭证存储的原子比对写和 activate 读取进锁；
#187 把当前账号主动续期改走同一提交规则（决策 6）；#189 让 live 回灌账号库先比新旧（决策 7）；
spec #185 后续凭证侧票（401 协调恢复、停泊按需轮换）在本 ADR 续写。自动切换侧决策记在 ADR-0020。

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

覆盖的写回点：停泊账号额度查询前的轮换（#186）、当前账号主动续期（#187，见决策 6）。

### 2. 凭证存储的原子「读 → 比对 → 写」

`CredentialStore::set_if`：读出当前值交给调用方判定，判定通过才写入，返回是否写入；比对口径由
调用方决定。`FileStore` 在自身排他锁（`<path>.lock`）下完成读、判定、写，`set` 走同一实现。
它只比对凭证文件里的当前值，锁内不读 legacy 钥匙串：迁移由 `get` 负责，钥匙串可能弹授权框，
持锁等它会卡住其他凭证读写。trait 的默认实现读写之间不持锁，只给没有锁可用的后端兜底；生产
装配是 `FileStore`。

### 3. 交换锁等不到

提交等交换锁最多 3 秒（ADR-0015 决策 4）。等不到时放弃写当前登录文件，库存仍在凭证存储自身的
锁下按比对写入，记 warn，查询照常完成。新凭证绝不丢弃。

### 4. activate 读取进锁

activate 的目标凭证读取、capture-on-leave、切换前身份记录（供抢救判定，ADR-0015 决策 7），与
写入、写后回读校验、registry 提交放进同一段交换锁临界区。交换锁按线程可重入，切换骨架
（`swap_with_snapshot_protected`）与 registry 写入在这段临界区里照常进锁。capture-on-leave 失败
仍只 warn、不阻断切换（ADR-0015 决策 11）。

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
  无条件盖上去。交换锁等不到时按决策 3 只写库存、不碰当前登录文件。
- 死 token 指纹的清除规则不变：提交完成后只清掉被消耗那份凭证的拒止指纹；续期被拒仍记指纹停试。
- 主动续期门（开关 × 锁探测 × 临期 60 秒）不变。官方已抢先轮换时直接采纳当前登录文件的分支不走
  本规则，那里没有本工具的轮换结果要提交。

### 7. 回灌新旧比对（#189）

live → 账号库的回灌原先一律以 live 为准。库存已经持有较新的凭证、当前登录文件却还是较旧的那份时，
之后一次读取当前账号凭证或切走时的 capture-on-leave，都会用旧 live 覆盖库存，销毁一次性 refresh
token 的唯一有效副本。这样的窗口有两个来源：重新授权写入库存后 live 写失败（ADR-0002 已接受的
边界），以及轮换提交只写了库存（交换锁等不到，决策 3；主动续期 live 写失败，ADR-0003 已接受的
边界）。复现用例修复前为红：重新授权窗口期内的查询与切换都把库存撤回旧凭证。

现在三处回灌共用一条规则，先比对再写：capture-on-leave（含写后回读校验的抢救，ADR-0015 决策 7）、
读取当前账号凭证时的顺手回灌、主动续期发现官方已抢先轮换时的采纳回灌。

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
5. 只防覆盖，不修 live：库存较新时不主动改写当前登录文件。读取仍返回当前登录文件，本次查询照用
   它；新凭证在切走再切回时进入当前登录文件。

重新授权的写序（先库存后 live）不变。ADR-0002「窗口期一律以 live 为准」因此修订为「可比较时新者胜，
不可比较时以 live 为准」。

## 剩余风险

- 轮换结果同步进当前登录文件之前，官方客户端若已用旧 token 续期被拒并写入墓碑，该会话需要
  重新登录；库存保有新凭证，下次切回即可恢复。官方 100ms 重读采纳机制能覆盖大部分交错。
- 交换锁等不到、且该账号此刻是当前账号时（停泊轮换期间被切成当前账号，或当前账号主动续期），
  当前登录文件留着被消耗的 token：官方客户端下次续期会被拒，该会话需要重新登录，或切走再切回。
  库存里的新凭证由回灌新旧比对（决策 7）保住：轮换结果比被消耗的那份过期得晚，之后读取当前账号
  凭证与切走时的 capture-on-leave 都不再用旧 live 覆盖它。任一方解析不出过期时刻时仍以 live 为准，
  新凭证会被覆盖。Kimi 的 access token 是带 `exp` 的 JWT（主动续期门与凭证过期型 401 的判定都依赖
  它），这一支要等凭证格式变了才会出现。
- 401 协调恢复的写回点尚未改走本规则（spec #185 后续凭证侧票）。

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
- `activate_reads_target_credentials_inside_swap_lock`：修复前为红。
- `interleaved_switches_capture_on_leave_keeps_intermediate_account_credentials`：修复前为红。
- `activate_still_switches_when_capture_on_leave_fails`
- `proactive_renewal_in_flight_then_switch_away_keeps_switch_and_stores_new_token`（#187）：复现
  用例，修复前为红，当前登录文件被写回原账号的续期结果。
- `reauth_live_write_failure_window_then_query_quota_keeps_new_credentials_in_store`、
  `reauth_live_write_failure_window_then_activate_keeps_new_credentials_in_store`（#189）：复现用例，
  修复前为红，重新授权窗口期内的查询与切换把库存撤回旧凭证。它们由原先钉住 ADR-0002 已接受行为的
  两个用例改写，凭证带上了过期时刻；切换用例最后切回该账号，当前登录文件拿到重新授权的结果。
- `proactive_renewal_adoption_keeps_newer_stored_credentials`（#189）：修复前为红，采纳回灌用较旧的
  live 覆盖了库存。
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

既有用例保持绿：写后回读校验、一次性 token 抢救、快照回滚、fsync 注入失败、启动一致性自检、
切换零网络断言；主动续期三分支与先库存后 live 的写序、DeadToken 指纹、续期门纯判定
（`proactive_renewal_wanted_decision_pins_all_branches`）；capture-on-leave 守卫三态（#189 后守卫
经同一回灌函数生效）。

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
- **回灌不可比较时拒灌（库存优先）**（#189）：官方客户端合法刷新、live 领先库存是主场景
  （ADR-0002 修订否决朴素方案 A 的理由），解析不出过期时刻就拒灌会把这类新凭证挡在库存外，切走时
  随 live 被覆盖而丢失。不可比较时维持以 live 为准，新规则不引入新的失败。
- **只比 refresh token，不同就拒灌**（#189）：官方每次续期都换 refresh token，这样会挡掉所有合法
  刷新。refresh token 只用来认出「同一份凭证」，新旧要靠过期时刻判断。
- **库存较新时主动把它写回当前登录文件**（#189）：这是切换之外的一次 live 写入，要与官方客户端的
  刷新协调（ADR-0003），超出本票；只防覆盖已经保住新凭证，切走再切回即生效。
