---
type: adr
title: 切换路径正确性与时延加固：即时返回、写后回读校验、fsync、锁上限与启动自检
status: accepted
tags: [swap, latency, correctness, fsync, startup-check]
intent: Record the manual-swap hardening set (spec #152 decisions 1-10): GUI swap returns immediately with SwapOutcome after activate commits, activate duration_ms audit with a measured P50, a 3s swap-lock wait limit, post-swap live identity readback verification with rollback and one-time-token salvage, file-level fsync across all swap-path writes with the registry in the rollback set, a live-is-truth startup consistency check, and swap-as-usual with a dead-credential warning.
documents:
  symbols:
    - SwapOutcome
    - verify_live_identity_after_swap
    - startup_consistency_check
    - switch_lock
    - with_duration_ms
---
# 切换路径正确性与时延加固：即时返回、写后回读校验、fsync、锁上限与启动自检

activate 本身是纯本地原子写 + 快照回滚 + 文件锁（零网络），但 GUI 切换命令在 activate 之后
同步等待一次可能打网的快照重列，用户感知的「切换慢」主要在这段等待；同时写入无 fsync、
写完不回读校验身份、registry 与 live 可能不一致且无自检、切到已失效的凭证无提示。spec #152
（实现 #153–#157）沿手动切换路径逐项加固：感知时延靠「即时返回」而不是把 activate 做快，
正确性靠回读校验 + fsync + 启动自检。红线不变：手动切换永不依赖网络、额度查询与 `/me`。

## 决策

1. **GUI 切换命令在 activate 提交后立即返回（#157）。** `swap_account` 在 activate（含
   audit 追加、手动保持期落盘、本地缓存档位读取）完成后组装轻量 `SwapOutcome`
   （`account_id` / `display_name` / `message` / `credential_warning`）立即返回，不再同步
   等待、也不再主动发起额度快照重列。保持期必须先于返回落盘：后台重列中的回驻判定看到它，
   才不会把用户刚选的账号又改掉（耗尽被迫切换不受保持期影响，ADR-0008）。

2. **前端据成功响应翻转视图，后台补齐（#157）。** 切换成功后前端据 `account_id` 把所点
   卡片立即标记为当前账号（由成功响应背书，非盲目乐观），各行额度数字、会员档位沿用既有
   快照缓存值，界面不空白、不错配账号；随后的快照重列由前端后台发起，不打忙碌闸门、不挡
   再次切换——后台刷新期间可立刻再切另一张卡。后台结果经既有世代守卫应用，用户期间又
   发起新操作时这份落后快照自然落空。已接受细节：立即视图只翻转当前标记、不重排序，
   切换目标暂不在列表首位，后台刷新到齐后归位。
   **修订注记（ADR-0019）**：「后台刷新期间可立刻再切」照旧允许。后台重列会为停泊账号做
   token 轮换，此前轮换在途时切到该账号会丢号；现在轮换结果经轮换写回比对在交换锁内提交，
   与切换互相可见。

3. **activate 段耗时写入审计 `duration_ms`（#153）。** `AuditEvent` 新增可选 `duration_ms`
   字段（`Option<u64>`），旧记录无此字段、读取侧向后兼容。GUI 手动切换、CLI `swap`、GUI
   自动切换与配额失败被动切换（ADR-0017）全部在 activate 段同口径计时。不做 CI 时延断言；
   设计预算 P50 < 200ms，落地后本地实测（见下）。

4. **交换排他锁等待 3 秒上限（#153）。** `switch_lock` 获取排他锁最多等 3 秒（20ms 轮询），
   超时返回 `Error::SwitchLockTimeout`，用户可见文案即 Display 本身：「另一切换操作未完成，
   请重试」；GUI 经 `CommandError::SwitchLockTimeout`（kind `switch_lock_timeout`）、CLI 经
   既有错误链透传呈现。超时发生在进入临界区之前，live 与 registry 均未被改动。并发写保护
   靠锁，锁的可用性靠有界等待兜底——不再无限阻塞。
   **修订注记（ADR-0019）**：临界区现在从 activate 的目标凭证读取开始，capture-on-leave 与
   切换前身份记录也在锁内（见文末修订），超时时它们同样都不执行。

5. **写后回读身份校验，不一致即回滚（#154）。** activate 把目标 blob 写进 live 并落盘后，
   立即重读 live、经 `parse_metadata` 解析官方身份、与目标账号比对（复用切换路径既有身份
   辨认口径，不另造第二套）。不一致 → 返回错误，走 `swap_with_snapshot` 既有 commit 失败
   路径回滚：此刻 registry 尚未提交，live 恢复切换前内容、registry 保持原状——对外语义
   「要么切对、要么回到原状」，绝不停留在半切换中间态还报告成功。纯本地读 + 解析，零网络。

6. **unknown ≠ mismatch（#154）。** live 重读失败、或解析不出任何身份，只记 warn 放行：
   写入内容来自账号库，解析不出身份是账号数据自身的问题而非切换写错；校验只拦「写入结果
   身份与目标确证不符」，宁可漏报也不把可用切换误回滚。与 ADR-0009「无法确证不算失效」
   同向，与本地凭证快检的 unknown ≠ 失效（ADR-0017）同一家族。

7. **mismatch 恰为切换前 live 身份时，先抢救一次性 token 再回滚（#154）。** 回读到的身份
   恰是切换前 live 身份，视为「官方客户端并发写回旧账号」特征：此刻 live 可能携带旧账号
   新轮换的一次性 refresh token 的唯一副本（refresh token 一次性轮换，被备份覆盖即不可逆
   销毁）。回滚恢复备份**之前**，先经既有 capture-on-leave 通道把当前 live 内容抢救进其
   owner 账号的账号库存储；抢救失败如实并入错误文案并明示「该账号可能需要重新授权」，
   live 仍按备份恢复。其他 mismatch（既非旧身份也非目标，如第三方身份）维持纯回滚：无证据
   表明 live 携带任何账号的新凭证。

8. **切换路径全部写入补文件级 fsync（#153）。** live 凭证文件、registry（`set_active`）、
   capture-on-leave 的账号库存储、切换前快照备份与回滚恢复写，统一在「临时文件 + rename」
   之后做文件级 fsync；不做目录 fsync（目录项持久性依赖 NTFS 元数据日志）。fsync 失败如实
   上抛、进入既有回滚/错误路径，绝不静默成功。registry 的 commit 阶段写入纳入快照回滚集
   （`swap_with_snapshot_protected` 的 protected 集），其 fsync 失败与 live 同生命周期回滚。

9. **启动一致性自检，以 live 为准（#155）。** GUI 与常规 CLI 启动均执行一次
   `startup_consistency_check`（纯本地读）：live 身份在账号库但 active 标记不符（指向别人/
   无人持有/多人持有）→ 按 ADR-0002「对外一律以 live 为准」自动纠正 active 并记审计；
   live 身份不在账号库（用户在工具外重新登录过）→ 清除 active 标记、提示可导入收进账号库；
   墓碑命中（用户显式删除过的账号仍留在 live）→ 跳过，不纠正也不提示导入。live 不可读、
   身份无法辨认或内部错误 → 零副作用零输出（与决策 6 同向，无法确证就不动注册表）；
   自检是修复性维护不是门禁，任何写入失败只 warn，绝不阻塞启动。被动切换子命令
   （ADR-0017）也跑同一自检做 live 对齐，结果只记 debug 不打扰会话。

10. **切到已确认失效的凭证：照常切换 + 提示（#156）。** 目标凭证本地已确认失效时切换照常
    成功，完成后 GUI toast 与 CLI stderr 附加同文警告「该账号凭证已失效，建议重新授权」；
    用户掌控节奏又知晓风险。失效口径只认 invalid_grant 指纹命中（裁定与理由见 ADR-0017
    决策 8；#156 与 #158 同口径），纯本地读、fail-open，命中只附加提示、绝不改变切换结果。
    不做「将过期」预警，不做网络预检。

11. **capture-on-leave 失败维持 warn 不阻断（spec 决策 10，现状显式保留）。** 已知取舍
    （记录不修）：手动保持期写入同样是 warn-only——activate 成功但保持期写失败时，后台
    重列中的回驻判定看不到保持期（既有语义，同族）。触发前提是罕见本地 IO 故障，升级为
    阻断会让一次成功的切换反悔成失败，代价大于收益。

## 实测（P50，2026-09 落地时）

21 次真实 CLI `swap` 于 tempdir（含文件锁、快照、capture-on-leave、注册表写与全部新增
fsync；debug 构建、零网络、合成 blob），逐条读审计 `duration_ms`：

- **activate P50 = 7ms**，对照 200ms 设计预算余量充足。fsync 的加入未把切换推离毫秒级。
- release 复测命令：`cargo test -p kimi-switch-cli swap_duration_p50 -- --ignored --nocapture`。
- 该测试是手动执行的测量工具（`#[ignore]`），不是 CI 断言。

## Considered Options

- **把 activate 做得更快来消除感知时延**：activate 本就毫秒级（实测 P50 7ms），慢的是切换
  命令串接的快照重列（节流窗外每账号最坏 20s 超时 + 1 次重试）。解耦等待比压缩本地写入
  有效得多，也守住了「切换零网络」红线。
- **目录 fsync**：Windows 上目录句柄 fsync 语义受限，目录项持久性由 NTFS 元数据日志兜底；
  文件级 fsync 已覆盖「掉电丢最后一次切换」的主要风险面，不再为理论完备引入平台差异代码。
- **校验失败时保留新 live 并只报错**：写错身份的 live 是中间态，任何「停在错身份上」的
  方案都比回到原状差；回滚 + 抢救是唯一同时满足「要么切对、要么回到原状」与凭证安全的路径。

## 修订（ADR-0019，spec #185 / #186）：activate 的读取顺序

原先 activate 先在锁外读取目标凭证、做 capture-on-leave、记录切换前 live 身份，再由
`swap_with_snapshot_protected` 取交换锁完成快照、写入、写后回读校验（决策 5）与 registry 提交。
锁外读取会让切换写进轮换前的旧凭证，也会让交错的两个切换漏回灌中间账号的当前凭证。现在这三项
读取与写入、校验、提交同在一段交换锁临界区（锁按线程可重入）。决策 5–7 的校验与抢救语义不变，
决策 7 的「切换前 live 身份」改在锁内记录；决策 11 不变，capture-on-leave 在锁内失败仍只 warn。
切换零网络、毫秒级不变。详见 ADR-0019。
