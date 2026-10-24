import { describe, expect, it, vi } from "vitest";

import type { Snapshot, UiMessage } from "./api";
import {
  SILENT_QUOTA_BUSY_BACKOFF_MS,
  SILENT_QUOTA_CRITICAL_INTERVAL_MS,
  SILENT_QUOTA_ELEVATED_INTERVAL_MS,
  SILENT_QUOTA_INTERVAL_MS,
  SILENT_QUOTA_PENDING_RENEWAL_INTERVAL_MS,
  adaptiveSilentQuotaIntervalMs,
  createSilentQuotaScheduler,
  createSnapshotGenerationGuard,
  currentAccountMaxUsedRatio,
  silentQuotaIntervalMs,
  type SilentQuotaClock,
  type SilentQuotaHost,
  type SilentQuotaScope,
} from "./silent-quota";

type AccountRow = Snapshot["accounts"][number];

function sampleAccount(overrides: Partial<AccountRow> = {}): AccountRow {
  return {
    id: "a1",
    label: "工作机",
    active: true,
    membership: null,
    membership_expiry: null,
    membership_remaining_days: null,
    membership_period_days: null,
    membership_expiry_hint: false,
    quotas: [],
    error: null,
    credential_expired: false,
    cached_at: null,
    display_name: "工作机",
    official_nickname_known: false,
    auto_swap_opt_out: false,
    ...overrides,
  };
}

/** 停泊账号（非当前）：加密档语义里它只应被全量轮刷新。 */
function parkedAccount(id = "a2", overrides: Partial<AccountRow> = {}): AccountRow {
  return sampleAccount({ id, label: "停泊", display_name: "停泊", active: false, ...overrides });
}

/** 单窗口额度行；ratio 与 `QuotaView.ratio` 同口径（0..1 已用比例）。 */
function quotaRow(ratio: number | null): AccountRow["quotas"][number] {
  return {
    window: "5 小时窗口",
    ratio,
    text: ratio === null ? "n/a" : `${Math.round(ratio * 100)}%`,
    reset: null,
    tone: "high",
  };
}

/** 当前账号带一个已用比例窗口。 */
function currentAccountWithRatio(ratio: number | null): AccountRow {
  return sampleAccount({ quotas: [quotaRow(ratio)] });
}

function snapshotWithAccounts(
  accounts: AccountRow[],
  message: UiMessage | null = null,
): Snapshot {
  return { ...sampleSnapshot(message), accounts, account_count: accounts.length };
}

function sampleSnapshot(message: UiMessage | null = null): Snapshot {
  return {
    accounts: [sampleAccount()],
    current_label: "工作机",
    account_count: 1,
    urgent_count: 0,
    proactive_renewal: false,
    auto_swap: false,
    auto_swap_prefer_lower_tier: false,
    auto_swap_resident_lower_tier: false,
    auto_swap_pre_reset_burn: false,
    auto_swap_pace_pre_swap: false,
    message,
  };
}

function autoSwapSuccess(): UiMessage {
  return {
    text: "已自动切换到 工作机",
    tone: "ok",
    kind: "auto_swap_success",
  };
}

function deferred<T>(): {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (reason?: unknown) => void;
} {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

async function flushMicrotasks(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

type FakeClock = SilentQuotaClock & { advance(ms: number): void };

function createFakeClock(start = 1_000_000): FakeClock {
  let now = start;
  let nextId = 1;
  const scheduled: { id: number; due: number; handler: () => void }[] = [];

  const clock: FakeClock = {
    now: () => now,
    setTimeout(handler, delayMs) {
      const id = nextId++;
      scheduled.push({ id, due: now + delayMs, handler });
      return id;
    },
    clearTimeout(id) {
      const index = scheduled.findIndex((item) => item.id === id);
      if (index >= 0) {
        scheduled.splice(index, 1);
      }
    },
    advance(ms) {
      now += ms;
      for (;;) {
        const index = scheduled.findIndex((item) => item.due <= now);
        if (index < 0) {
          break;
        }
        const [timer] = scheduled.splice(index, 1);
        timer.handler();
      }
    },
  };
  return clock;
}

function createHarness(
  overrides: Partial<{
    accountCount: number | null;
    accounts: Snapshot["accounts"] | null;
  }> = {},
) {
  const clock = createFakeClock();
  const generation = createSnapshotGenerationGuard();
  const applied: Array<{ snapshot: Snapshot; generation: number }> = [];
  const toasts: Array<{ text: string; isError: boolean }> = [];
  const accountCount =
    overrides.accountCount === undefined ? 1 : overrides.accountCount;
  const flags = {
    busy: false,
    accountCount,
    accounts:
      overrides.accounts !== undefined
        ? overrides.accounts
        : accountCount === null
          ? null
          : [sampleAccount()],
  };
  // 请求计数（issue #161）：listCalls 记每轮收集范围；queriesByAccount 按 scope
  // 语义累计每账号「发网络请求」次数——"current" 轮只对当前账号计数，"all" 轮对全部计数。
  const listCalls: SilentQuotaScope[] = [];
  const queriesByAccount = new Map<string, number>();
  let listImpl: (scope: SilentQuotaScope) => Promise<Snapshot> = () =>
    Promise.resolve(sampleSnapshot());
  const listAccounts = vi.fn((scope: SilentQuotaScope) => {
    listCalls.push(scope);
    for (const row of flags.accounts ?? []) {
      if (scope === "current" && !row.active) {
        continue;
      }
      queriesByAccount.set(row.id, (queriesByAccount.get(row.id) ?? 0) + 1);
    }
    return listImpl(scope);
  });
  const host: SilentQuotaHost = {
    isBusy: () => flags.busy,
    accountCount: () => flags.accountCount,
    accounts: () => flags.accounts,
    listAccounts,
    // 按生产契约（issue #183）：应用调度器自身轮次的快照后可重新 arm，
    // 但不发用户快照通知（noteApplied）；两条节奏的簿记归调度器。
    applySnapshot(snapshot, generation) {
      applied.push({ snapshot, generation });
      flags.accounts = snapshot.accounts;
      flags.accountCount = snapshot.accounts.length;
      scheduler.arm();
    },
    toast(text, isError) {
      toasts.push({ text, isError });
    },
  };
  const scheduler = createSilentQuotaScheduler({ clock, host, generation });
  return {
    clock,
    generation,
    applied,
    toasts,
    flags,
    listAccounts,
    listCalls,
    queriesByAccount,
    scheduler,
    setList(impl: (scope: SilentQuotaScope) => Promise<Snapshot>) {
      listImpl = impl;
    },
  };
}

describe("adaptiveSilentQuotaIntervalMs 档位边界（Seam 1）", () => {
  it("< 80% 维持 90s（含无比例数据）", () => {
    expect(adaptiveSilentQuotaIntervalMs(null)).toBe(90_000);
    expect(adaptiveSilentQuotaIntervalMs(0)).toBe(90_000);
    expect(adaptiveSilentQuotaIntervalMs(0.5)).toBe(90_000);
    expect(adaptiveSilentQuotaIntervalMs(0.79)).toBe(90_000);
    expect(adaptiveSilentQuotaIntervalMs(0.7999999)).toBe(90_000);
  });

  it("恰达 80% → 30s；两档之间 → 30s", () => {
    expect(adaptiveSilentQuotaIntervalMs(0.8)).toBe(30_000);
    expect(adaptiveSilentQuotaIntervalMs(0.81)).toBe(30_000);
    expect(adaptiveSilentQuotaIntervalMs(0.9)).toBe(30_000);
    expect(adaptiveSilentQuotaIntervalMs(0.9499999)).toBe(30_000);
  });

  it("恰达 95% → 10s；更高截断在 10s（下限，不再更密）", () => {
    expect(adaptiveSilentQuotaIntervalMs(0.95)).toBe(10_000);
    expect(adaptiveSilentQuotaIntervalMs(0.96)).toBe(10_000);
    expect(adaptiveSilentQuotaIntervalMs(1)).toBe(10_000);
    expect(adaptiveSilentQuotaIntervalMs(1.5)).toBe(10_000);
  });
});

describe("currentAccountMaxUsedRatio 只看当前账号", () => {
  it("取 active 行所有窗口的最大非空比例；无数据为 null", () => {
    expect(currentAccountMaxUsedRatio(null)).toBeNull();
    expect(currentAccountMaxUsedRatio([])).toBeNull();
    expect(
      currentAccountMaxUsedRatio([
        { active: true, quotas: [], credential_expired: false },
      ]),
    ).toBeNull();
    expect(
      currentAccountMaxUsedRatio([
        { active: true, quotas: [{ ratio: null }], credential_expired: false },
      ]),
    ).toBeNull();
    expect(
      currentAccountMaxUsedRatio([
        { active: true, quotas: [{ ratio: 0.3 }, { ratio: 0.85 }], credential_expired: false },
      ]),
    ).toBe(0.85);
  });

  it("停泊账号比例不参与；无 active 行为 null", () => {
    expect(
      currentAccountMaxUsedRatio([
        { active: false, quotas: [{ ratio: 0.99 }], credential_expired: false },
      ]),
    ).toBeNull();
    expect(
      currentAccountMaxUsedRatio([
        { active: false, quotas: [{ ratio: 0.99 }], credential_expired: false },
        { active: true, quotas: [{ ratio: 0.42 }], credential_expired: false },
      ]),
    ).toBe(0.42);
  });
});

describe("silentQuotaIntervalMs", () => {
  it("有待续期行返回 5s，否则走适应性档位", () => {
    expect(silentQuotaIntervalMs(null)).toBe(90_000);
    expect(silentQuotaIntervalMs([])).toBe(90_000);
    const healthy = { active: true, quotas: [{ ratio: 0.2 }], credential_expired: false };
    expect(silentQuotaIntervalMs([healthy])).toBe(90_000);
    expect(
      silentQuotaIntervalMs([{ ...healthy, credential_expired: true }]),
    ).toBe(5_000);
    expect(
      silentQuotaIntervalMs([healthy, { ...healthy, active: false, credential_expired: true }]),
    ).toBe(5_000);
  });

  it("5s 待续期档优先于适应性加密档", () => {
    // 当前账号已到 95%+ 的 10s 档，只要有任一待续期行仍取 5s。
    const nearFull = { active: true, quotas: [{ ratio: 0.97 }], credential_expired: false };
    expect(silentQuotaIntervalMs([nearFull])).toBe(10_000);
    expect(
      silentQuotaIntervalMs([nearFull, { active: false, quotas: [], credential_expired: true }]),
    ).toBe(5_000);
  });

  it("档位由当前账号最大比例决定，含恰好 80% / 95% 边界", () => {
    const tier = (ratio: number | null) =>
      silentQuotaIntervalMs([
        { active: true, quotas: [{ ratio }], credential_expired: false },
      ]);
    expect(tier(0.79)).toBe(90_000);
    expect(tier(0.8)).toBe(30_000);
    expect(tier(0.9)).toBe(30_000);
    expect(tier(0.95)).toBe(10_000);
    // 停泊账号再满也不加密：只看当前账号。
    expect(
      silentQuotaIntervalMs([
        { active: true, quotas: [{ ratio: 0.5 }], credential_expired: false },
        { active: false, quotas: [{ ratio: 0.99 }], credential_expired: false },
      ]),
    ).toBe(90_000);
  });
});

describe("createSnapshotGenerationGuard 世代比较方向", () => {
  it("用户世代已占用则更早静默结果丢弃", () => {
    const guard = createSnapshotGenerationGuard();
    const silentGeneration = guard.next();
    const userGeneration = guard.reserveUser();
    expect(userGeneration).toBeGreaterThan(silentGeneration);
    expect(guard.applyIfCurrent(silentGeneration)).toBe(false);
    expect(guard.applyIfCurrent(userGeneration)).toBe(true);
  });

  it("同世代或更新世代可应用", () => {
    const guard = createSnapshotGenerationGuard();
    expect(guard.applyIfCurrent(1)).toBe(true);
    expect(guard.applyIfCurrent(1)).toBe(true);
    expect(guard.applyIfCurrent(2)).toBe(true);
    expect(guard.applyIfCurrent(1)).toBe(false);
  });
});

describe("静默额度调度器", () => {
  it("钉住 90 秒间隔常量", () => {
    expect(SILENT_QUOTA_INTERVAL_MS).toBe(90_000);
  });

  it("钉住待续期加密间隔常量", () => {
    expect(SILENT_QUOTA_PENDING_RENEWAL_INTERVAL_MS).toBe(5_000);
  });

  it("钉住适应性加密档常量（issue #161）", () => {
    expect(SILENT_QUOTA_ELEVATED_INTERVAL_MS).toBe(30_000);
    expect(SILENT_QUOTA_CRITICAL_INTERVAL_MS).toBe(10_000);
  });

  it("钉住 fire 时 busy 短退避常量（reviewer P1-1）", () => {
    expect(SILENT_QUOTA_BUSY_BACKOFF_MS).toBe(5_000);
  });

  it("比例越过 80% 后下一周期切 30s，加密轮只查当前账号", async () => {
    const parked = parkedAccount();
    const h = createHarness({ accountCount: 2, accounts: [sampleAccount(), parked] });
    h.setList(() =>
      Promise.resolve(snapshotWithAccounts([currentAccountWithRatio(0.85), parked])),
    );
    h.scheduler.noteApplied();
    h.scheduler.arm();
    // 常态 90s：第一轮全量。
    h.clock.advance(90_000);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    expect(h.listCalls[0]).toBe("all");

    // 快照回来当前账号 85% → 档位切 30s，下一周期即按新档排程，范围只查当前账号。
    h.clock.advance(29_999);
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    h.clock.advance(1);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(2);
    expect(h.listCalls[1]).toBe("current");
  });

  it("比例越过 95% 后切 10s 下限档", async () => {
    const h = createHarness();
    h.setList(() => Promise.resolve(snapshotWithAccounts([currentAccountWithRatio(0.97)])));
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(90_000);
    await flushMicrotasks();
    expect(h.listCalls[0]).toBe("all");
    // 97% → 10s 档：每 10s 一轮 "current"，不再更密。
    for (let i = 0; i < 3; i += 1) {
      h.clock.advance(9_999);
      expect(h.listAccounts).toHaveBeenCalledTimes(1 + i);
      h.clock.advance(1);
      await flushMicrotasks();
      expect(h.listAccounts).toHaveBeenCalledTimes(2 + i);
      expect(h.listCalls[1 + i]).toBe("current");
    }
  });

  it("比例回落 80% 以下恢复 90s 常态节奏", async () => {
    const parked = parkedAccount();
    const h = createHarness({
      accountCount: 2,
      accounts: [currentAccountWithRatio(0.85), parked],
    });
    h.setList(() =>
      Promise.resolve(snapshotWithAccounts([currentAccountWithRatio(0.5), parked])),
    );
    h.scheduler.noteApplied();
    h.scheduler.arm();
    // 85% → 30s 加密档：先来一轮 current。
    h.clock.advance(30_000);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    expect(h.listCalls[0]).toBe("current");
    // 快照回落 50% → 下一周期按 90s 全量原节奏（全量轮到期在 noteApplied 后 90s）。
    h.clock.advance(59_999);
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    h.clock.advance(1);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(2);
    expect(h.listCalls[1]).toBe("all");
  });

  it("加密档期间停泊账号维持 90s 全量节奏（请求计数）", async () => {
    const parked = parkedAccount("a2");
    const h = createHarness({
      accountCount: 2,
      accounts: [currentAccountWithRatio(0.97), parked],
    });
    h.setList(() =>
      Promise.resolve(snapshotWithAccounts([currentAccountWithRatio(0.97), parked])),
    );
    h.scheduler.noteApplied();
    h.scheduler.arm();
    // 90s 窗口、10s 档：+10…+80 共 8 轮 "current"，+90 档位到期与全量原节奏同时，
    // 优先全量（顺带刷新当前账号）。
    for (let i = 0; i < 9; i += 1) {
      h.clock.advance(10_000);
      await flushMicrotasks();
    }
    expect(h.listAccounts).toHaveBeenCalledTimes(9);
    expect(h.listCalls.filter((scope) => scope === "current")).toHaveLength(8);
    expect(h.listCalls.filter((scope) => scope === "all")).toHaveLength(1);
    // 网络请求只对当前账号加密：当前账号每轮都被查询（9 次），
    // 停泊账号只命中全量轮（1 次），维持 90s 原节奏。
    expect(h.queriesByAccount.get("a1")).toBe(9);
    expect(h.queriesByAccount.get("a2")).toBe(1);
  });

  it("待续期 5s 档优先于适应性加密档且为全量轮", async () => {
    const parked = parkedAccount("a2", { credential_expired: true });
    const h = createHarness({
      accountCount: 2,
      accounts: [currentAccountWithRatio(0.97), parked],
    });
    h.scheduler.noteApplied();
    h.scheduler.arm();
    // 当前账号 97%（本应 10s 档）+ 停泊账号待续期 → 仍按 5s 全量轮。
    h.clock.advance(4_999);
    expect(h.listAccounts).not.toHaveBeenCalled();
    h.clock.advance(1);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    expect(h.listCalls[0]).toBe("all");
  });

  it("迟到快照丢弃：用户世代已占用后不应用更早的静默结果", async () => {
    const h = createHarness();
    const pending = deferred<Snapshot>();
    h.setList(() => pending.promise);
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(90_000);
    expect(h.listAccounts).toHaveBeenCalledTimes(1);

    h.generation.reserveUser();
    const message = autoSwapSuccess();
    pending.resolve(sampleSnapshot(message));
    await flushMicrotasks();

    expect(h.applied).toEqual([]);
    expect(h.toasts).toEqual([{ text: message.text, isError: false }]);
  });

  it("迟到快照丢弃时 generic 通知不 toast", async () => {
    const h = createHarness();
    const pending = deferred<Snapshot>();
    h.setList(() => pending.promise);
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(90_000);

    h.generation.reserveUser();
    pending.resolve(
      sampleSnapshot({
        text: "已自动切换到 工作机",
        tone: "ok",
        kind: "generic",
      }),
    );
    await flushMicrotasks();

    expect(h.applied).toEqual([]);
    expect(h.toasts).toEqual([]);
  });

  it("busy 中完成丢弃快照，但自动切换通知仍 toast", async () => {
    const h = createHarness();
    const pending = deferred<Snapshot>();
    h.setList(() => pending.promise);
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(90_000);

    h.flags.busy = true;
    const message = autoSwapSuccess();
    pending.resolve(sampleSnapshot(message));
    await flushMicrotasks();

    expect(h.applied).toEqual([]);
    expect(h.toasts).toEqual([{ text: message.text, isError: false }]);
  });

  it("busy 丢弃时按 kind 判定通知，不靠中文", async () => {
    const h = createHarness();
    const pending = deferred<Snapshot>();
    h.setList(() => pending.promise);
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(90_000);

    h.flags.busy = true;
    pending.resolve(
      sampleSnapshot({
        text: "已自动切换到 工作机",
        tone: "ok",
        kind: "generic",
      }),
    );
    await flushMicrotasks();

    expect(h.applied).toEqual([]);
    expect(h.toasts).toEqual([]);
  });

  it("busy 丢弃时自动切换失败通知仍按错误 toast", async () => {
    const h = createHarness();
    const pending = deferred<Snapshot>();
    h.setList(() => pending.promise);
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(90_000);

    h.flags.busy = true;
    const failed: UiMessage = {
      text: "自动切换失败",
      tone: "err",
      kind: "auto_swap_failed",
    };
    pending.resolve(sampleSnapshot(failed));
    await flushMicrotasks();

    expect(h.applied).toEqual([]);
    expect(h.toasts).toEqual([{ text: failed.text, isError: true }]);
  });

  it("fire 时 busy 且操作随后失败：短退避保活，busy 解除后照常执行", async () => {
    const h = createHarness();
    h.scheduler.noteApplied();
    h.scheduler.arm();
    // 定时器恰在用户操作（如「正在查询额度…」）在途时到期。
    h.flags.busy = true;
    h.clock.advance(90_000);
    // busy：不发请求，进入 5s 短退避（此前无退避，裸早退即停摆）。
    expect(h.listAccounts).not.toHaveBeenCalled();
    h.clock.advance(4_999);
    expect(h.listAccounts).not.toHaveBeenCalled();
    // 用户操作失败：只 toast、不 applySnapshot（外部无人再 arm）——旧代码
    // 至此调度器永久停摆；只有退避重 arm 能复活。
    h.flags.busy = false;
    h.clock.advance(1);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    // 复活后节奏照常：完成轮从完成时刻起算下一轮 90s。
    h.clock.advance(89_999);
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    h.clock.advance(1);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(2);
  });

  it("busy 持续期间只按退避节奏排程，不打爆请求计数", async () => {
    const h = createHarness();
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.flags.busy = true;
    h.clock.advance(90_000); // fire 撞 busy → 进入退避
    // busy 持续 60s（跨 12 个退避窗口）：一次请求都不发。
    for (let i = 0; i < 12; i += 1) {
      h.clock.advance(5_000);
    }
    expect(h.listAccounts).not.toHaveBeenCalled();
    // busy 解除后最近一个退避窗口恢复执行。
    h.flags.busy = false;
    h.clock.advance(5_000);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
  });

  it("fire 时静默在飞（inFlight）：退避不并发重复收集，在飞轮完成即接管排程", async () => {
    const h = createHarness();
    const pending = deferred<Snapshot>();
    h.setList(() => pending.promise);
    h.scheduler.noteApplied();
    h.scheduler.arm();
    // 第一轮静默收集在飞（inFlight）。
    h.clock.advance(90_000);
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    // 外部成功快照（applySnapshot → noteApplied + arm）排出的下一轮到期，
    // 但第一轮仍在飞 → 撞上 inFlight 进退避，不发起第二次并发收集。
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(90_000);
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    h.clock.advance(4_999);
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    // 在飞轮完成：finally 记账并 arm，退避定时器被 arm 清掉——不双跑。
    pending.resolve(sampleSnapshot(autoSwapSuccess()));
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    h.clock.advance(5_000);
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
    // 完成时刻（1_184_999）起算 90s 后照常下一轮。
    h.clock.advance(85_000);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(2);
  });

  it("pagehide 后不再 arm", () => {
    const h = createHarness();
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.scheduler.stop();
    h.clock.advance(90_000);
    h.scheduler.arm();
    h.clock.advance(90_000);
    expect(h.listAccounts).not.toHaveBeenCalled();
  });

  it("首载失败不自动静默重试；成功快照后才按 90s 节奏", async () => {
    const h = createHarness({ accountCount: null });
    h.scheduler.arm();
    h.clock.advance(90_000);
    h.clock.advance(90_000);
    expect(h.listAccounts).not.toHaveBeenCalled();

    h.flags.accountCount = 1;
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(89_999);
    expect(h.listAccounts).not.toHaveBeenCalled();
    h.clock.advance(1);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
  });

  it("成功静默查询把自动切换通知交给 applySnapshot，普通 toast 滤掉", async () => {
    const h = createHarness();
    const message = autoSwapSuccess();
    h.setList(() =>
      Promise.resolve(
        sampleSnapshot({
          text: "刚刚更新过",
          tone: "info",
          kind: "generic",
        }),
      ),
    );
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(90_000);
    await flushMicrotasks();
    expect(h.applied).toHaveLength(1);
    expect(h.applied[0]?.snapshot.message).toBeNull();

    const h2 = createHarness();
    h2.setList(() => Promise.resolve(sampleSnapshot(message)));
    h2.scheduler.noteApplied();
    h2.scheduler.arm();
    h2.clock.advance(90_000);
    await flushMicrotasks();
    expect(h2.applied[0]?.snapshot.message).toEqual(message);
    expect(h2.toasts).toEqual([]);
  });

  it("快照含凭证待续期行时 5s 后静默列出", async () => {
    const h = createHarness({
      accounts: [{ ...sampleAccount(), credential_expired: true }],
    });
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(4_999);
    expect(h.listAccounts).not.toHaveBeenCalled();
    h.clock.advance(1);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
  });

  it("快照不含凭证待续期行时仍 90s", async () => {
    const h = createHarness();
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(89_999);
    expect(h.listAccounts).not.toHaveBeenCalled();
    h.clock.advance(1);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
  });

  it("任一账号待续期即加密；待续期消失后回到 90s", async () => {
    const parked = { ...sampleAccount(), id: "a2", label: "停泊", display_name: "停泊" };
    const h = createHarness({
      accountCount: 2,
      accounts: [sampleAccount(), { ...parked, credential_expired: true }],
    });
    h.setList(() =>
      Promise.resolve({
        ...sampleSnapshot(),
        accounts: [sampleAccount(), { ...parked, credential_expired: false }],
        account_count: 2,
      }),
    );
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(5_000);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);

    h.clock.advance(5_000);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);

    h.clock.advance(85_000);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(2);
  });

  it("界面隐藏时排程照常，按 90s 间隔执行", async () => {
    const h = createHarness();
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(89_999);
    expect(h.listAccounts).not.toHaveBeenCalled();
    h.clock.advance(1);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
  });

  it("界面隐藏期间连续多个间隔照常轮询，无暂停与补跳", async () => {
    const h = createHarness();
    h.scheduler.noteApplied();
    h.scheduler.arm();
    // 解耦后（issue #160）隐藏状态不影响排程：每个间隔到期都执行，
    // 不存在「隐藏暂停、回可见补跳」，也就不存在 catch-up 排程。
    h.clock.advance(90_000);
    await flushMicrotasks();
    h.clock.advance(90_000);
    await flushMicrotasks();
    h.clock.advance(90_000);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(3);
  });

  it("待续期加密间隔在界面隐藏时同样照常执行", async () => {
    const h = createHarness({
      accounts: [{ ...sampleAccount(), credential_expired: true }],
    });
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.clock.advance(4_999);
    expect(h.listAccounts).not.toHaveBeenCalled();
    h.clock.advance(1);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
  });
});
