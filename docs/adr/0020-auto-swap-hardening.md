---
type: adr
title: 自动切换侧加固：判定口径与并发收敛
status: accepted
tags: [auto-swap, past-reset, earliest-recovery, passive-swap, quota-cache, exhaustion-ledger, expected-current-account, auto-swap-lock]
intent: Record the auto-swap side of spec 185 (implementation decisions D-F), one section per ticket; the credential side (A-C) lives in ADR-0019. Past reset (issue 191, decision E) - when deciding, a quota window whose reset_at is at or before now counts as already reset (used 0, limit unchanged, next reset unknown) for the current and parked accounts alike, across the exhaustion trigger, candidate eligibility, candidate margin, resident return, burn candidates and burn return; pace and the burn trigger already skipped passed resets, the burn-return flip still needs a later reset_at, and prefer_soonest_reset no longer ranks a passed reset_at first (amends ADR-0011 decision 2). The passive overlay applies the same view to its cached base under one shared now, so the authoritative exhausted window survives (amends ADR-0017 decision 5). The passive path reads the quota cache through get_keeping_past_resets so accounts whose windows have all passed keep a snapshot. The earliest-recovery hint returns only future times through earliest_recovery_at, shared by the GUI toast and the CLI line and JSON. Nothing writes the quota cache or changes displayed numbers; no new network request, config key or state field. Exhaustion ledger (issue 192, decision D) - with the master switch on, the passive path records the current account's exhaustion (account, window, observed_at, recovers_at) into the exhaustion_ledger field of auto_swap.json through record_exhaustion, inside the passive lock before the decision and whatever it decides; recovers_at is the cached reset_at of that window when later than observed_at, else observed_at plus 5 hours, 7 days or 31 days (unrecognized windows count as 5 hours). A record is in effect while recovers_at is in the future and the account has no usable snapshot fetched after observed_at; in-effect accounts join the locally invalid ones as excluded parked candidates on every automatic path, so past reset cannot re-admit a just-exhausted account (spec decision E), the current account gets no extra trigger, and manual swap ignores the ledger. earliest_recovery_at takes the state file path and also returns in-effect recovery times; every state write prunes records whose recovery has passed; the quota cache stays untouched (amends ADR-0017 decision 5). Expected current account (issue 193, decision F) - automatic swaps call Provider::activate_if_current with the account current at decision time; inside the swap lock, before reading credentials or writing live, the engine re-reads the registry current mark and returns ActivateOutcome::Superseded on mismatch (no live, registry or snapshot write, no rollback, not an error). GUI snapshot orchestration, CLI status auto-swap (exit 2, reason superseded) and the passive path (trace superseded, exit 0) treat it as no swap: no audit, cooldown, round-trip memory, toast or resident-confirm reset. It replaces the post-decision current-account re-read (amends ADR-0017 decision 7); the GUI pending-auth re-read and the passive in-lock cooldown re-read stay; manual swap keeps the unconditioned activate. Auto-swap serialization lock (issue 194, decision F) - the passive lock moves into core as AutoSwapLock and now serializes every automatic trigger (GUI snapshot orchestration, CLI status auto-swap, passive), keeping the lock file name passive-swap.lock under the state dir so mixed GUI and hook CLI versions still queue together; lock order is this lock then the swap lock, the wait limit is 3 seconds, and the GUI waits through spawn_blocking. Each trigger re-reads the auto-swap state (cooldown and manual hold included), decides, activates with the expected current account and writes cooldown inside the lock, so a trigger arriving in another's commit-to-cooldown gap no longer swaps a second time. A busy lock skips the GUI round silently, makes CLI status stay with exit 2 and reason busy, and leaves passive trace busy with exit 0 and no ledger record; the exhaustion ledger write stays inside the lock, the passive post-decision cooldown re-read stays for older processes, and manual swap never takes this lock (amends ADR-0017 decision 7 scope and name).
documents:
  symbols:
    - reset_passed_windows
    - earliest_recovery_at
    - overlay_exhausted_snapshot
    - get_keeping_past_resets
    - record_exhaustion
    - decide_auto_swap
    - decide_resident_return
    - activate_if_current
    - ActivateOutcome
    - AutoSwapLock
    - auto_swap_lock_file
    - apply_status_auto_swap
    - apply_snapshot_auto_swap
---
# 自动切换侧加固：判定口径与并发收敛

spec #185 的自动切换侧（实现决策 D–F）记在本 ADR，按票分节续写：过点重置（#191，决策 E）
先落地；耗尽账本（#192，决策 D）、期望当前账号（#193）与自动切换串行化锁（#194，决策 F）
落地时各补一节。凭证侧（决策 A–C）见 ADR-0019。

## 过点重置（#191，spec #185 决策 E）

GUI 很久没开时，额度缓存停在上次真实查询的结果。缓存里 `reset_at` 已经过去的窗口其实
已经翻篇，账号早已恢复，判定却可能仍把它当成不可用，误报「没有可自动切换的账号」；
「最早恢复」提示也可能给出一个已经过去的时刻。

复现时看到的机制与 spec 问题 3 的推断（「已过窗口仍按旧比例计算」）不完全一致：

- 展示路径的 `QuotaCache::get` 本就丢掉 `reset_at` 已过的窗口，窗口全部过期时整条返回
  `None`。只读缓存的被动路径照用它：部分窗口过点的账号，余下窗口的判定结果恰好与过点
  重置一致；**窗口全部过点的账号整条失去快照**，按「无快照 → 候选资格线不通过」被排除。
  CLI 被动端到端复现用例先红。
- 判定本身不看 `reset_at` 是否已过：快照里带着已过的时刻时（查询与判定之间刚好过点，或
  调用方直接传入），耗尽线与合格线照旧比例算，快重置偏好把它排成最早（ADR-0011 决策 2
  原文），「最早恢复」也照样给出。

### 决策

1. **判定视图。** `decide_auto_swap_with_trip`（耗尽、pace、抢用、回程）与
   `decide_resident_return`（回驻）在入口处取过点重置视图（`reset_passed_windows`），
   之后读快照的判定都只读这份视图：`Ready` 与 `Cached` 快照里 `reset_at` 早于或等于此刻
   的窗口，已用按 0 计、上限不变，`reset_at` 置空（下一次重置时刻未知）；`Failed` 快照
   原样保留。恰好等于此刻也算过点。当前账号与停泊账号同口径。

2. **作用面一视同仁。** 耗尽触发线、停泊候选合格线、防抖边际（比较过点重置后的最大
   已用）、回驻资格（目标合格线，以及当前账号「任一窗口已达阈值则不回驻」）、抢用候选
   合格线、回程（原账号合格线与目标耗尽线）都读这份视图；被动触发进同一判定单点，自然
   继承。其余口径：
   - pace 与重置前抢用触发本就跳过已过 `reset_at` 的窗口（`window_pace` / `burn_window`），
     不变。
   - 回程「翻过记录的重置点」仍只认快照里比记录值更晚的 `reset_at`，过点本身不算翻过。
     抢用目标就是当前账号，轮询拿到的是新窗口的未来时刻，回程时机不变。
   - 快重置偏好：过点窗口置空后不再给出排序键，已过的时刻不再排成最早。**修订 ADR-0011
     决策 2**「已过期的 `reset_at` 仍按时间戳比较」：过点窗口已经重置，没有快过期的配额
     可抢先烧，按陈旧时刻排第一只是数据残留。

3. **只作用于判定与「最早恢复」提示。** 视图是判定内的副本：不写回 `quota_cache.json`，
   不改 GUI 与 CLI 显示的额度数字。不新增网络请求、配置键或状态字段；手动切换不经判定，
   不受影响。

4. **被动 overlay 的耗尽窗口不参与过点重置。** 耗尽原文是「此刻已耗尽」的权威证据，不能
   被陈旧的 `reset_at` 抵消。做法是结构性的：`overlay_exhausted_snapshot` 先对缓存底按
   `now` 过点重置，再覆写耗尽窗口。
   - 文案点名的窗口若 `reset_at` 已过，那个时刻属于已翻篇的旧窗口，随底一起置空；覆写后
     的耗尽窗口不带已过的时刻，判定的过点重置抹不掉它。
   - 文案辨认不出窗口时，过点窗口按已用 0 参与「已用最高」的挑选：靠陈旧比例挑中一个
     已经翻篇的窗口，不如仍在当期、已用最高的窗口可信。
   - 被动子命令里 overlay 与判定共用同一个 `now`，两处划线一致。

   **修订 ADR-0017 决策 5**「其余窗口保留缓存真值」为「其余窗口保留过点重置后的缓存
   值」；overlay 仍是纯函数、不写额度缓存。spec 决策 E 规定覆写窗口的恢复时刻以耗尽账本
   为准，见「耗尽账本」决策 6。

5. **被动路径读缓存保留过点窗口。** 新增 `QuotaCache::get_keeping_past_resets`：与 `get`
   相同，但保留 `reset_at` 已过的窗口（原样返回缓存值，交给判定过点重置）；缺 `reset_at`
   的窗口仍按兜底 TTL 失效。被动路径的 `cached_quota_snapshot` 改用它。`get` 与 `fresh`
   不变，GUI 行与 CLI 列表的数字照旧。轮询路径的可用停泊快照来自真实查询或节流窗内的
   缓存：`get` / `fresh` 丢掉过点窗口后，余下窗口与过点重置等价；窗口全部过点的账号在
   下一次真实查询后回到候选。

6. **「最早恢复」只给未来时刻，一处计算。** 新增 core `earliest_recovery_at(snapshots,
   now)`：在过点重置视图上，取可用快照（`Ready` / 非 stale `Cached`）窗口里最早的
   `reset_at`。视图已把过点时刻置空，结果必在未来；没有就返回 `None`，提示不写时刻。
   GUI 无候选 toast、CLI 无候选说明行与 `--json` 的 `earliestReset` 由调用方各算一次、
   共用这一个函数；原 `quota_snapshot::earliest_reset_at`（不看此刻）删除。范围沿用
   #113 起的既有口径：全部可用快照，含当前账号——所有停泊账号都不可用时，当前账号自己
   的重置往往就是最先到的恢复。spec 决策 E 的措辞是「各停泊账号」，本票不改范围。耗尽
   账本的恢复时刻也并进这一处，函数随之多了状态文件路径参数，见「耗尽账本」决策 7。

### 被拒方案

- **只把已用清零、保留已过的 `reset_at`**：pace、抢用与回程本就跳过已过时刻，但快重置
  偏好会把它排成最早，「最早恢复」也得另行过滤。置空让「下次重置未知」只有一种表示，
  各处按缺 `reset_at` 统一处理。
- **在判定里直接丢掉过点窗口（与 `QuotaCache::get` 同口径）**：窗口全部过点的账号没有
  已知窗口、资格线不通过，正是要修的误报。
- **改 `QuotaCache::get`，全局保留过点窗口**：GUI 行与 CLI 列表也读 `get`，陈旧的已用比例
  会重新显示出来，违背「不改界面上的额度数字」。只给只读缓存的被动路径开一个保留过点
  窗口的读法。
- **给 overlay 覆写的窗口打豁免标记**（`Quota` 新字段或旁路集合）：要改公开数据结构，
  还要把标记穿过判定各处。overlay 先过点重置底、再覆写，加上同一个 `now`，权威证据已能
  在视图里存活。
- **过点即视为回程「翻过记录的重置点」**：spec 明定回程逻辑不变；抢用目标就是当前账号，
  轮询会拿到新窗口的未来 `reset_at`，回程时机本就可靠。
- **快重置偏好改用带 `now` 的键（已过的时刻排最后或忽略）**：要给比较链穿 `now`；视图
  置空后，既有「未知不占优先位」（ADR-0011 决策 3）已给出同样结果。

### 验收

- 判定单点（`crates/core/src/auto_swap.rs` 测试）：`decide_auto_swap_past_reset_table`
  （停泊账号过点重新合格、恰好等于此刻也算、当前账号耗尽窗口过点不再触发、防抖边际、
  快重置偏好、抢用候选合格线与抢用触发）、`decide_auto_swap_burn_return_past_reset`
  （回程）、`decide_resident_return_past_reset`（回驻）、
  `earliest_recovery_at_skips_passed_resets`、`earliest_recovery_at_counts_only_usable_windows`。
- 被动 overlay 豁免：`crates/core/tests/passive_auto_swap.rs` 的
  `passive_overlay_window_is_exempt_from_past_reset`（文案点名的窗口与辨认不出窗口两种）。
- 缓存读法：`get_keeping_past_resets_keeps_reset_passed_windows_only`。
- CLI 被动端到端（tempdir 入口、真实 activate）：
  `passive_swaps_to_account_whose_cached_exhaustion_already_reset`——当前账号撞上额度
  耗尽，另一账号缓存显示耗尽但 `reset_at` 已过，切到该账号；额度缓存字节不变，观察型
  内核不向上报错（入口恒退出码 0）。
- 提示同口径：CLI `status_auto_swap_no_candidate_skips_passed_reset`（说明行与
  `earliestReset`）、GUI `snapshot_auto_swap_no_candidate_skips_passed_reset`（toast）。

## 耗尽账本（#192，spec #185 决策 D）

被动 overlay 只把「此刻已耗尽」送进本次判定，不写额度缓存（ADR-0017 决策 5）。GUI 没开时，
额度缓存停在最后一次真实查询：alice 撞上额度耗尽，被动切到 bob；冷却过后 bob 也耗尽，第二次
被动判定时 alice 的缓存仍显示有余量，alice 合格，于是切回刚耗尽的 alice，会话的下一个请求又
撞上耗尽。两次触发之间没有真实查询能纠正 alice 的缓存；过点重置还会把缓存里 `reset_at` 已过
的窗口放回候选。CLI 被动端到端复现用例先红（切回 alice）。

### 决策

1. **数据模型。** 自动切换状态文件 `auto_swap.json` 新增字段 `exhaustion_ledger`，每条记录
   含账号 id、耗尽窗口（文案辨认不出时缺省）、观察时刻 `observed_at`、恢复时刻
   `recovers_at`。键为（账号, 窗口），同键以新证据覆盖；不同窗口各记一条，任一条生效即
   生效。缺字段视为空表，空表不写进文件。状态文件不拒绝未知字段，旧版本进程读到账本照常
   工作，改写时丢掉账本也可接受：状态文件本就可丢弃，最坏是少挡一次切回。

2. **只由被动路径写。** 总开关开、分类为额度耗尽时，在被动串行化锁内、判定之前为当前账号
   写一条（`record_exhaustion`），无论之后切走、冷却中、无合格候选还是已被取代。账本、
   overlay 与判定共用同一个 `now`。总开关关闭时不写，切换侧零副作用不变（ADR-0017 决策 2）。
   轮询路径不写，它手里就是真实快照。不写额度缓存，不新增网络请求、配置键或 GUI。

3. **恢复时刻。** 取该账号缓存快照（overlay 之前）里该窗口晚于观察时刻的 `reset_at`；没有
   就从观察时刻按窗口时长估计：5 小时窗 +5 小时、7 天窗 +7 天、月度窗 +31 天。文案辨认
   不出窗口按 5 小时窗处理，缓存里也查 5 小时窗。观察时已过的 `reset_at` 属于已翻篇的旧
   窗口，不用。

4. **生效条件。** 恢复时刻晚于此刻，且该账号没有取得时间晚于观察时刻的可用快照（`Ready` /
   非 stale `Cached`）。更新的真实查询比账本可信：仍显示耗尽，由合格线照常排除；已经恢复
   （提前重置、换了套餐），立刻回到候选。stale 回落与查询失败不算更新的证据。生效与否在
   每次判定时按当时的快照计算，不落盘。

5. **只挡停泊候选。** 生效记录的账号与本地凭证已确认失效的账号并成一个排除集合：
   `decide_auto_swap_from_state` 与 `decide_resident_return_from_state` 从状态文件收集，
   `parked_candidate_admitted` 单点判定。耗尽、pace、重置前抢用、抢用回程、回驻与被动触发
   的停泊候选都不选它。当前账号自己有记录不多触发一次，也不挡它被切走。手动切换不读账本，
   切到有记录的账号照常成功。

6. **落实 spec 决策 E：覆写窗口的恢复时刻以账本为准。** 过点重置把缓存里 `reset_at` 已过的
   窗口按已用 0 计，只看缓存，刚耗尽的账号可能马上「重新合格」。账本生效期间该账号不作
   候选，过点重置放不回它；到了恢复时刻、或有了更新的可用快照，才回到按快照判定。overlay
   本身不变：仍是纯函数，只管本次判定。

7. **「最早恢复」并进账本。** `earliest_recovery_at(path, snapshots, now)` 多了状态文件路径
   参数：在「过点重置」决策 6 的口径之外，再并进生效记录的恢复时刻，取最早；生效记录的
   恢复时刻必在未来。只算快照列表里的账号：快照覆盖账号库全部账号，已删除账号的记录不算。
   范围仍含当前账号。GUI toast、CLI 说明行与 `--json` 的 `earliestReset` 仍共用这一个函数。

8. **随写清理。** 每次写状态文件（冷却、手动保持期、回驻确认计数、往返记忆、记账本）都丢掉
   恢复时刻不晚于本次写入时刻的记录。带时刻参数的写入用参数，回驻确认计数与手动保持期取
   此刻。被更新快照判为失效、恢复时刻未到的记录留到到点再清：它已不起作用，写入方手里也
   没有快照可判。

### 被拒方案

- **把耗尽写进额度缓存**：违背 ADR-0017 决策 5。缓存是真实查询的结果，GUI 行与 CLI 列表
  会显示合成的数字。账本放在可丢弃的状态文件里，只影响判定。
- **账本单独成文件**：多一个文件、多一套读写；自动切换状态文件本就可丢弃，每次判定都读，
  写入路径也现成。
- **轮询路径也写账本**：轮询拿的是真实快照，耗尽账号由合格线直接排除；spec 只让被动路径写。
- **只认缓存里的 `reset_at`，没有就不记**：被动路径的缓存常常缺该窗口，或时刻已过，记录
  要么永不恢复、要么马上恢复。按窗口时长保守估计。
- **写入时按快照删掉失效记录**：写入方手里没有快照。失效在读时按快照判，到点后随写清理。
- **当前账号有记录时额外触发切换**：spec 明定不额外触发；撞上耗尽的那次被动触发已经在判定
  里切走它。

### 验收

- 判定单点（`crates/core/src/auto_swap.rs` 测试）：
  `decide_auto_swap_from_state_ledger_excludes_parked_candidate`（生效、到点失效、更新的可用
  快照让记录失效、当前账号不额外触发）、`exhaustion_record_recovery_time_table`（恢复时刻
  规则表，含过点重置放不回有记录的账号）、`ledger_excludes_parked_candidate_on_every_auto_path`
  （pace、抢用、回程、回驻、被动）、`earliest_recovery_at_counts_ledger_recovery`、
  `exhaustion_ledger_state_file_compat`（存量文件、与其余字段互不覆盖、空表不写、未知字段）、
  `exhaustion_ledger_cleanup_and_overwrite_on_write`。
- CLI 被动端到端（tempdir 入口、真实 activate）：
  `passive_does_not_swap_back_to_just_exhausted_account`（修复前为红）——alice 耗尽切到 bob；
  冷却过后 bob 耗尽，alice 的缓存仍显示有余量也不切回，留痕
  `decision=stay reason=no_eligible_candidate`；额度缓存字节不变，观察型内核不向上报错（入口
  恒退出码 0）；手动切到 alice 照常成功。`passive_disabled_writes_no_exhaustion_ledger`——
  总开关关闭时状态文件与额度缓存字节都不变。

## 期望当前账号（#193，spec #185 决策 F）

自动切换从做出决定到真正写当前登录文件，中间隔着一段时间：GUI 快照编排要等一轮额度查询
收齐才判定，CLI `status` 先查完额度再判定，被动触发判定之后还要去拿交换锁。这段时间里用户
手动切到别的账号，自动切换随后仍按原决定切走，用户刚做的选择被覆盖。

修复前各路径的防线都有缝：

- GUI 快照编排：判定后、activate 前在交换锁外重读当前账号，重读与拿锁之间仍可插进手动切换。
- CLI `status` 自动切换：判定后不重读，直接 activate。
- 被动触发：在被动锁内重读当前账号与冷却（ADR-0017 决策 7），但手动切换不拿被动锁，重读与
  拿交换锁之间同样插得进来。

复现用例先红：引擎用例在比对放到锁外时照样切换；CLI 被动端到端用例里，旧代码把用户刚手动
切到的账号又切走。

### 决策

1. **带前置条件的切换入口。** `Provider` 新增 `activate_if_current(id, expected_current)`，
   返回 `ActivateOutcome`：`Activated`（已切换）或 `Superseded`（已被取代）。调用方传入判定
   时的当前账号，即切换目标的 `from`。这是必需方法，没有默认实现：默认实现碰不到交换锁，只能
   在锁外先查再切，满足不了下面的锁内契约。

2. **比对在交换锁内最先做，只看 registry 当前标记。** 进锁后、读目标凭证与 capture-on-leave
   之前重读 registry：期望账号不在了，或已不是当前账号，就返回 `Superseded`。此时不读写
   凭证、不建切换前快照、不写当前登录文件、不写 registry、不进回滚，也不算错误，只记一条
   info 日志。registry 的写入与切换同在这把交换锁下，手动切换的 registry 提交也在锁内完成，
   所以比对之后到本次提交之间不会再有别的切换插进来。比对一致时与 `activate` 完全相同。

3. **调用方把「已被取代」当作没有切换。** 不写切换审计、冷却和往返记忆，不弹 toast；回驻
   确认计数停在判定时的值，只有切换成功才清零。
   - GUI 快照编排（`apply_snapshot_auto_swap`）：本轮没有任何效果。
   - CLI `status` 自动切换（`apply_status_auto_swap`）：按不切换处理，退出码 2，`--json` 输出
     `stay` 事件，原因 `superseded`。
   - 被动触发：留痕 `decision=stay reason=superseded`，入口退出码照旧恒为 0。

4. **取代「决定之后重读当前账号」。** 锁内比对覆盖了原来锁外重读能拦住的全部情形，还补上了
   重读与拿锁之间的缝。
   - GUI 去掉当前账号重读，保留待授权重读：设备码授权进行中时放弃，与当前账号是谁无关。
   - 被动路径去掉被动锁内的当前账号重读，保留锁内冷却重读：并发的轮询切换刚写下冷却时直接
     放弃，不必再去拿交换锁，留痕同为 `superseded`。
   - CLI `status` 原本没有重读，直接带上前置条件。

   **修订 ADR-0017 决策 7** 中「重读当前账号」的部分；被动串行化锁与锁内冷却重读不变。

5. **手动切换不变。** `swap` 等手动路径照旧调用不带前置条件的 `activate`：手动切换就是用户
   此刻的选择，不该被别的切换取代；仍然零网络、不依赖额度查询。

### 被拒方案

- **比对留在锁外（调用方决定后重读一次）**：就是修复前 GUI 的做法，重读与拿锁之间的缝还在；
  引擎复现用例在这种做法下为红。
- **比对放进提交闭包（写当前登录文件之后、提交 registry 之前）**：比对失败要走回滚，当前
  登录文件已被改写一次、切换前快照也已建好，做不到「已被取代时什么都不动」。放在进锁后最先
  做，失败时没有任何副作用。
- **比对当前登录文件里的账号身份**：要防的是别的切换抢先，而每次切换都在这把锁下提交
  registry 当前标记，这个标记就是「当前账号是谁」的记录。当前登录文件会被官方客户端续期
  改写，身份也可能解析不出，拿它比对会混进与这个竞态无关的误判。当前登录文件被外部改写的
  情形，已由 capture-on-leave 与写后回读校验处理。
- **给 trait 方法加默认实现**：默认实现只能在锁外先查再 `activate`，等于把上面被拒的锁外
  比对藏进接口。各测试替身逐个显式实现，契约更清楚。
- **把「已被取代」当错误返回**：调用方会按切换失败记审计、弹失败提示、CLI 返回 1。它只说明
  用户已经做了选择，应与「不切换」一样安静。

### 验收

- 引擎（`crates/providers/common/src/engine.rs` 测试）：
  `activate_if_current_switches_when_expected_account_is_still_current`（一致时正常切换，
  零网络）、`activate_if_current_is_superseded_by_manual_switch_before_swap_lock`（经
  `before_next_activate_lock` 在进锁前插入一次手动切换；当前登录文件与 registry 字节不变，
  快照目录不存在或为空）。
- GUI 注入（`crates/gui/src/lib.rs` 测试）：
  `snapshot_auto_swap_superseded_skips_toast_audit_and_cooldown`、
  `snapshot_resident_return_superseded_skips_toast_audit_and_cooldown`（不弹 toast、不写审计
  与冷却，回驻确认计数不被清零）；`snapshot_auto_swap_stale_pending_skips_activate`（待授权
  重读保留）。
- CLI（`crates/cli/src/main.rs` 测试）：
  `status_auto_swap_superseded_is_stay_without_audit_or_cooldown`、
  `status_resident_return_superseded_is_stay_without_audit_or_cooldown`（退出码 2、`--json`
  原因 `superseded`、不写审计与冷却）；被动端到端
  `passive_superseded_by_manual_switch_keeps_manual_choice`（tempdir 入口、真实 activate，
  在被动 activate 之前插入一次真实手动切换：手动选的账号保住，没有切换审计和冷却，留痕
  `decision=stay reason=superseded`，观察型内核不向上报错）。
- 手动切换不经前置条件，既有切换用例保持绿。

## 自动切换串行化锁（#194，spec #185 决策 F）

#162 给被动触发加了一把排他文件锁 `<state_dir>/passive-swap.lock`（ADR-0017 决策 7），
只让被动触发彼此排队。GUI 快照编排与 CLI `status` 自动切换不拿它，冷却也在锁外读，
[提交, 写冷却] 间隙对它们仍然敞开：CLI `status` 刚把已耗尽的 alice 切到 bob、冷却还没写，
alice 会话的配额失败 hook 恰在此刻到达。被动触发拿到被动锁，重读账号库时当前账号已是
bob，冷却又读不到，于是把耗尽证据按在 bob 头上，再切一跳到 carol。期望当前账号（#193）
挡不住这一跳：被动触发是在 bob 提交之后才判定的，判定时的当前账号就是 bob，交换锁内比对
一致。CLI 端到端复现用例先红（停在 carol，两条切换审计）。

### 决策

1. **一把锁覆盖全部自动触发。** core 新增 `AutoSwapLock`（`crates/core/src/auto_swap_lock.rs`），
   取代 CLI 私有的 `PassiveSwapLock`。GUI 快照编排（`apply_snapshot_auto_swap`）、CLI `status`
   自动切换（`apply_status_auto_swap`）与被动触发都在锁内走完「重读自动切换状态 → 判定 →
   带期望当前账号的切换 → 写冷却、往返记忆与回驻确认计数」。后到的触发在锁内看到前一个
   持锁者刚写下的冷却，自然落空。总开关关闭时不拿锁；GUI 有进行中的设备码授权时也不拿。

2. **文件名不变。** 锁文件仍是 `<state_dir>/passive-swap.lock`
   （`AppPaths::auto_swap_lock_file`）。GUI 与 hook 所调的 release CLI 不一定同时更新，文件
   同名，新旧版本的被动触发才继续在同一个文件上排队；改名还要迁移。规范名改为「自动切换
   串行化锁」，文件名是历史遗留。

3. **锁顺序与等待。** 固定为本锁 → 交换锁：自动触发先拿本锁，再在 activate 里拿交换锁；
   手动切换只拿交换锁，不会反过来等本锁，构不成死锁环。等待上限对齐交换锁的 3 秒。打不开
   或锁不上的 IO 故障同样当作拿不到，记一条 warn：宁可这一轮不切，也不在锁外判定。临界区里
   没有网络请求：额度查询在拿锁之前完成，锁内只读写本地文件。

4. **冷却与手动保持期在锁内读。** `apply_status_auto_swap` 与 `apply_snapshot_auto_swap`
   不再接收调用方事先读好的这两个值，改收一个拿锁闭包，进锁后自己从状态文件读；往返记忆、
   回驻确认计数与耗尽账本本就由 `decide_*_from_state` 在判定时读。调用方没法再在锁外读状态。

5. **拿不到锁就是另一次自动切换正在进行，各调用方按自己的「不切」口径放弃。**
   - GUI 快照编排：本轮静默跳过，不判定、不弹 toast、不写审计与状态，下一轮快照照常重试。
     等锁放进 `spawn_blocking`，不占异步运行时线程，界面不因等锁卡住。
   - CLI `status`：按不切换处理，与 `superseded` 同口径：退出码 2，`--json` 输出 `stay` 事件，
     原因 `busy`；没有说明行和审计，不写状态。
   - 被动触发：留痕 `decision=stay reason=busy`，入口退出码照旧恒为 0。

6. **耗尽账本仍在锁内写。** `record_exhaustion` 照旧在拿到锁之后、判定之前写（「耗尽账本」
   决策 2），只是锁换成了本锁。被动触发 busy 时不记账本。

7. **被动路径决定之后的冷却重读保留。** 同版本的 GUI 与 CLI `status` 已在同一把锁里排队，
   这道重读拦不到它们；它留给不拿本锁的旧版本 GUI 或 CLI `status`，代价是一次本地读。

8. **手动切换不拿本锁。** `swap` 等手动路径只拿交换锁，自动触发持锁期间照常立刻完成，仍然
   零网络、不依赖额度查询。手动切换与自动切换之间的竞态由期望当前账号在交换锁内处理。

9. **残留：迟到的被动触发。** hook 事件不带账号身份。另一次自动切换已经写完冷却、放了锁，
   被动触发才到，就会把耗尽记在新的当前账号头上：冷却挡住了切换，耗尽账本却照「无论切不切」
   给它记了一条。它此刻是当前账号，不受影响；在下一次真实查询拿到它的可用快照、或到恢复时刻
   之前，它被切走后不作停泊候选。本锁只保证同时到达的触发不在 [提交, 写冷却] 间隙里判定，
   收不掉这种情形。

本节**修订 ADR-0017 决策 7** 的范围与名称：被动串行化锁扩为全部自动触发共用的自动切换
串行化锁，文件名不变。本 ADR 前文「耗尽账本」决策 2 与「期望当前账号」中的「被动串行化锁」
「被动锁」，从本节起读作自动切换串行化锁。

### 被拒方案

- **只在锁内包住 activate，冷却与保持期仍由调用方事先读好传进来**：后到者拿着进锁之前读的
  旧冷却判定，看不见前一个持锁者刚写下的冷却，竞态原样保留。
- **拿锁留在调用方外层，`apply_*` 签名不变**：每个调用方都得记得在锁内读冷却与保持期，
  busy 的处理也落在测不到的薄壳里。拿锁作为参数收进编排，锁内重读与 busy 口径在既有注入
  接缝上就能测。
- **另开新锁文件（如 `auto-swap.lock`）**：版本不一致的 GUI 与 hook 所调 CLI 不再互相排队，
  旧文件也要清理。
- **不等待，拿不到立即放弃**：前一个持锁者只是判定不切（冷却中、没有合格候选）时，后到的
  被动触发本可以照常判定，立即放弃就白白丢掉一次权威证据。临界区只有本地读写和一次切换，
  通常远不到 3 秒。
- **手动切换也拿本锁**：手动切换要立刻生效，让它最多等自动切换 3 秒只多一段延迟。手动与自动
  之间的竞态已由期望当前账号在交换锁内处理。

### 验收

- core（`crates/core/src/auto_swap_lock.rs` 测试）：`lock_file_keeps_the_passive_lock_path`
  （路径仍是 `<state_dir>/passive-swap.lock`，与状态文件同目录）、
  `acquire_async_waits_off_the_runtime_thread`（等锁不占运行时线程）。
- CLI（`crates/cli/src/main.rs` 测试）：端到端 `status_and_passive_concurrent_triggers_swap_exactly_once`
  （修复前为红；tempdir 入口、真实 activate：status 把耗尽的 alice 切到 bob，被动触发在
  [提交, 写冷却] 间隙到达，等满 3 秒后以 busy 放弃；恰好一次切换、一条切换审计，留痕
  `decision=stay reason=busy`，耗尽账本无记录，观察型内核不向上报错）；
  `status_auto_swap_lock_busy_is_stay_without_side_effects`（退出码 2、`--json` 原因 `busy`、
  无审计、不写状态）；`status_auto_swap_rereads_cooldown_after_taking_lock`（等锁期间写下的
  冷却在锁内看得见）；`manual_swap_ignores_auto_swap_lock`（持锁期间手动切换照常完成）。
- GUI 注入（`crates/gui/src/lib.rs` 测试）：`snapshot_auto_swap_lock_busy_skips_round_then_retries`
  （busy 轮无 toast、无审计、不写状态，下一轮照常切换）、
  `snapshot_auto_swap_rereads_cooldown_after_taking_lock`。
- 既有用例保持绿：`passive_concurrent_triggers_converge_without_oscillation`（5 触发恰 1 切）
  与各留痕、额度缓存字节不变用例。
