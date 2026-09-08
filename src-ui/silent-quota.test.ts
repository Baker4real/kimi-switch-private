import { describe, expect, it, vi } from "vitest";

import type { Snapshot, UiMessage } from "./api";
import {
  SILENT_QUOTA_INTERVAL_MS,
  SILENT_QUOTA_PENDING_RENEWAL_INTERVAL_MS,
  createSilentQuotaScheduler,
  createSnapshotGenerationGuard,
  silentQuotaIntervalMs,
  type SilentQuotaClock,
  type SilentQuotaHost,
} from "./silent-quota";

function sampleAccount(): Snapshot["accounts"][number] {
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
  };
}

function sampleSnapshot(message: UiMessage | null = null): Snapshot {
  return {
    accounts: [sampleAccount()],
    current_label: "工作机",
    account_count: 1,
    urgent_count: 0,
    theme: "classic",
    proactive_renewal: false,
    auto_swap: false,
    auto_swap_prefer_lower_tier: false,
    auto_swap_resident_lower_tier: false,
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
    hidden: false,
    busy: false,
    accountCount,
    accounts:
      overrides.accounts !== undefined
        ? overrides.accounts
        : accountCount === null
          ? null
          : [sampleAccount()],
  };
  let listImpl: () => Promise<Snapshot> = () => Promise.resolve(sampleSnapshot());
  const listAccounts = vi.fn(() => listImpl());
  const host: SilentQuotaHost = {
    isHidden: () => flags.hidden,
    isBusy: () => flags.busy,
    accountCount: () => flags.accountCount,
    accounts: () => flags.accounts,
    listAccounts,
    applySnapshot(snapshot, generation) {
      applied.push({ snapshot, generation });
      flags.accounts = snapshot.accounts;
      flags.accountCount = snapshot.accounts.length;
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
    scheduler,
    setList(impl: () => Promise<Snapshot>) {
      listImpl = impl;
    },
  };
}

describe("silentQuotaIntervalMs", () => {
  it("有待续期行返回 5s，否则 90s", () => {
    expect(silentQuotaIntervalMs(null)).toBe(90_000);
    expect(silentQuotaIntervalMs([])).toBe(90_000);
    expect(silentQuotaIntervalMs([{ credential_expired: false }])).toBe(90_000);
    expect(silentQuotaIntervalMs([{ credential_expired: true }])).toBe(5_000);
    expect(
      silentQuotaIntervalMs([
        { credential_expired: false },
        { credential_expired: true },
      ]),
    ).toBe(5_000);
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

  it("窗口隐藏暂停，到期也不列出", () => {
    const h = createHarness();
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.flags.hidden = true;
    h.scheduler.pause();
    h.clock.advance(90_000);
    expect(h.listAccounts).not.toHaveBeenCalled();
  });

  it("回到可见且间隔已过则立即补一跳", async () => {
    const h = createHarness();
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.flags.hidden = true;
    h.scheduler.pause();
    h.clock.advance(90_000);
    expect(h.listAccounts).not.toHaveBeenCalled();
    h.flags.hidden = false;
    h.scheduler.arm();
    h.clock.advance(0);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
  });

  it("待续期加密期间隐藏暂停，回可见立即补一跳", async () => {
    const h = createHarness({
      accounts: [{ ...sampleAccount(), credential_expired: true }],
    });
    h.scheduler.noteApplied();
    h.scheduler.arm();
    h.flags.hidden = true;
    h.scheduler.pause();
    h.clock.advance(5_000);
    expect(h.listAccounts).not.toHaveBeenCalled();
    h.flags.hidden = false;
    h.scheduler.arm();
    h.clock.advance(0);
    await flushMicrotasks();
    expect(h.listAccounts).toHaveBeenCalledTimes(1);
  });
});
