import { isErrorMessageTone, type Snapshot, type UiMessage } from "./api";
import { silentQuotaMessage } from "./auto-swap";

/** 与 core `QUOTA_MIN_REFRESH_INTERVAL_MS` 默认一致；前端拿不到运行时配置。 */
export const SILENT_QUOTA_INTERVAL_MS = 90_000;

export interface SnapshotGenerationGuard {
  next(): number;
  /** 用户操作一开始就占住世代，已在飞的静默结果会被当成落后快照丢掉。 */
  reserveUser(): number;
  /**
   * 世代比较方向：`generation < applied` 才丢弃。
   * 同世代或更新世代可应用并记下 applied。
   */
  applyIfCurrent(generation: number): boolean;
}

export function createSnapshotGenerationGuard(): SnapshotGenerationGuard {
  let snapshotGeneration = 0;
  let appliedGeneration = 0;

  function next(): number {
    snapshotGeneration += 1;
    return snapshotGeneration;
  }

  return {
    next,
    reserveUser() {
      const generation = next();
      appliedGeneration = generation;
      return generation;
    },
    applyIfCurrent(generation) {
      if (generation < appliedGeneration) {
        return false;
      }
      appliedGeneration = generation;
      return true;
    },
  };
}

export interface SilentQuotaClock {
  now(): number;
  setTimeout(handler: () => void, delayMs: number): number;
  clearTimeout(id: number): void;
}

export interface SilentQuotaHost {
  isHidden(): boolean;
  isBusy(): boolean;
  /** 尚无成功快照为 null；空账号列表不预约。 */
  accountCount(): number | null;
  listAccounts(): Promise<Snapshot>;
  applySnapshot(snapshot: Snapshot, generation: number): void;
  toast(text: string, isError: boolean): void;
}

export interface SilentQuotaScheduler {
  readonly generation: SnapshotGenerationGuard;
  arm(): void;
  noteApplied(): void;
  pause(): void;
  stop(): void;
}

function emitAutoSwapToast(
  toast: SilentQuotaHost["toast"],
  message: UiMessage | null,
): void {
  if (!message) {
    return;
  }
  toast(message.text, isErrorMessageTone(message.tone));
}

export function createSilentQuotaScheduler(opts: {
  clock: SilentQuotaClock;
  host: SilentQuotaHost;
  generation?: SnapshotGenerationGuard;
}): SilentQuotaScheduler {
  const { clock, host } = opts;
  const generation = opts.generation ?? createSnapshotGenerationGuard();
  let lastSnapshotAt = 0;
  let timer = 0;
  let inFlight = false;
  let stopped = false;

  function stopTimer(): void {
    if (timer !== 0) {
      clock.clearTimeout(timer);
      timer = 0;
    }
  }

  function arm(): void {
    stopTimer();
    if (stopped || host.isHidden()) {
      return;
    }
    const count = host.accountCount();
    // 首载失败不自动静默重试；成功快照后才按 90s 节奏。
    if (count === null || count === 0) {
      return;
    }
    const delay = Math.max(0, SILENT_QUOTA_INTERVAL_MS - (clock.now() - lastSnapshotAt));
    timer = clock.setTimeout(() => {
      void run();
    }, delay);
  }

  async function run(): Promise<void> {
    if (stopped || host.isHidden() || host.isBusy() || inFlight) {
      return;
    }
    const count = host.accountCount();
    if (count === null || count === 0) {
      return;
    }
    inFlight = true;
    const snapshotGeneration = generation.next();
    try {
      const next = await host.listAccounts();
      if (stopped) {
        return;
      }
      const message = silentQuotaMessage(next.message);
      const snapshot = { ...next, message };
      if (host.isBusy()) {
        emitAutoSwapToast(host.toast, message);
        return;
      }
      if (!generation.applyIfCurrent(snapshotGeneration)) {
        emitAutoSwapToast(host.toast, message);
        return;
      }
      host.applySnapshot(snapshot, snapshotGeneration);
    } catch {
      // 静默失败不 toast；有上次成功快照则条留在当前界面。静默列出不弹会员 toast。
    } finally {
      inFlight = false;
      lastSnapshotAt = clock.now();
      arm();
    }
  }

  return {
    generation,
    arm,
    noteApplied() {
      lastSnapshotAt = clock.now();
    },
    pause() {
      stopTimer();
    },
    stop() {
      if (stopped) {
        return;
      }
      stopped = true;
      stopTimer();
    },
  };
}
