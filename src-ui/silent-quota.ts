import { isErrorMessageTone, type Snapshot, type UiMessage } from "./api";
import { silentQuotaMessage } from "./auto-swap";

/** 常态间隔，与 core `QUOTA_MIN_REFRESH_INTERVAL_MS` 默认一致；前端拿不到运行时配置。
 * 适应性档位（issue #161）里它是「当前账号最大已用比例 < 80%」的常态档，
 * 同时始终是停泊账号的全量刷新节奏（原节奏，不随加密档变化）。 */
export const SILENT_QUOTA_INTERVAL_MS = 90_000;

/** 适应性加密档·高用量（issue #161）：当前账号最大已用比例 ≥ 80% 时的间隔。 */
export const SILENT_QUOTA_ELEVATED_INTERVAL_MS = 30_000;

/** 适应性加密档·临界（issue #161）：当前账号最大已用比例 ≥ 95% 时的间隔。
 * 10s 是适应性档位的下限，用量再高也不再加密；仅待续期 5s 档优先级更高。 */
export const SILENT_QUOTA_CRITICAL_INTERVAL_MS = 10_000;

/** 快照含凭证待续期行时的静默间隔；无恢复信号时后端 Scheduled 鉴权退避跳过真实查询。
 * 该档优先于适应性档位（spec #152 决策 12）。 */
export const SILENT_QUOTA_PENDING_RENEWAL_INTERVAL_MS = 5_000;

/** fire 时撞上 busy / 静默在飞的短退避（reviewer P1-1，issue #160）：秒级重试排程，
 * 取最短既有档位的节奏——busy 期间只排定时器不发请求，不会忙轮询；
 * 退避到期走 arm() 重算档位与到期时间，不欠账也不抢跑。 */
export const SILENT_QUOTA_BUSY_BACKOFF_MS = 5_000;

/** 间隔推导所需的账号行字段（`Snapshot["accounts"]` 的结构性子集，夹具可只给这些）。 */
export interface SilentQuotaAccountLike {
  active: boolean;
  quotas: ReadonlyArray<{ ratio: number | null }>;
  credential_expired: boolean;
}

/** 当前账号最大已用比例（issue #161）：active 行所有额度窗口里最大的非空 ratio。
 * 无当前行、或当前行没有任何可用比例（如查询失败行）返回 null，调度按常态档处理。
 * 停泊账号的比例不参与——加密只由当前账号驱动。 */
export function currentAccountMaxUsedRatio(
  accounts: ReadonlyArray<SilentQuotaAccountLike> | null,
): number | null {
  const ratios = (accounts ?? [])
    .filter((row) => row.active)
    .flatMap((row) => row.quotas)
    .map((quota) => quota.ratio)
    .filter((ratio): ratio is number => ratio !== null);
  if (ratios.length === 0) {
    return null;
  }
  return Math.max(...ratios);
}

/** 适应性档位纯函数（issue #161，spec #152 决策 12）：
 * 当前账号最大已用比例 < 80% → 90s；达到 80%（≥ 0.8）→ 30s；达到 95%（≥ 0.95）→ 10s。
 * 10s 为下限，比例再高也不再更密；无比例数据（null）按 < 80% 常态处理。 */
export function adaptiveSilentQuotaIntervalMs(maxUsedRatio: number | null): number {
  if (maxUsedRatio !== null && maxUsedRatio >= 0.95) {
    return SILENT_QUOTA_CRITICAL_INTERVAL_MS;
  }
  if (maxUsedRatio !== null && maxUsedRatio >= 0.8) {
    return SILENT_QUOTA_ELEVATED_INTERVAL_MS;
  }
  return SILENT_QUOTA_INTERVAL_MS;
}

/** 静默间隔（issue #161）：快照含凭证待续期行 → 5s（优先于适应性档位）；
 * 否则按当前账号最大已用比例走适应性档位（90s / 30s / 10s）。 */
export function silentQuotaIntervalMs(
  accounts: ReadonlyArray<SilentQuotaAccountLike> | null,
): number {
  if (accounts?.some((row) => row.credential_expired === true)) {
    return SILENT_QUOTA_PENDING_RENEWAL_INTERVAL_MS;
  }
  return adaptiveSilentQuotaIntervalMs(currentAccountMaxUsedRatio(accounts));
}

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
  isBusy(): boolean;
  /** 尚无成功快照为 null；空账号列表不预约。 */
  accountCount(): number | null;
  /** 最近成功快照的账号行；尚无快照为 null。间隔按其中的凭证待续期与当前账号比例决定。 */
  accounts(): Snapshot["accounts"] | null;
  /** 静默列出并收集额度。scope 声明本轮收集范围（issue #161）：
   * "all" 全量收集；"current" 只对当前账号发网络请求、停泊账号用缓存行组装快照。 */
  listAccounts(scope: SilentQuotaScope): Promise<Snapshot>;
  /** 应用调度器自身轮次（全量轮与加密轮）的快照。契约（issue #183）：宿主可以绘制并重新
   * `arm()`，但不得调用 `noteApplied()`——两条节奏的簿记归调度器，本轮完成后按本轮范围
   * 自行记账。宿主若在这里重置节奏，加密档期间停泊账号的 90s 全量轮会被无限顺延。
   * 调度器只在 `generation.applyIfCurrent(generation)` 刚通过后同步调用本方法，宿主不必再查世代。 */
  applySnapshot(snapshot: Snapshot, generation: number): void;
  toast(text: string, isError: boolean): void;
}

/** 静默列出的收集范围（issue #161）：
 * - "all"：全量收集。常态 90s 轮、待续期 5s 轮、以及加密档期间维持停泊账号
 *   90s 原节奏的全量轮都用它；
 * - "current"：只重查当前账号，是加密档（30s / 10s）的加密轮专用——
 *   接近阈值时打网成本可控，停泊账号请求频率不随加密档升高。 */
export type SilentQuotaScope = "all" | "current";

export interface SilentQuotaScheduler {
  readonly generation: SnapshotGenerationGuard;
  arm(): void;
  /** 用户发起的全量快照（启动首载、刷新、账号操作、设备码授权、切换流）已应用：
   * 两条节奏都从此刻起算。只服务用户发起的快照，调度器自身轮次的快照不得发出。 */
  noteApplied(): void;
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
  // 双节奏簿记（issue #161）：lastFullAt 记最近一次全量收集（含用户发起快照），
  // 决定停泊账号的 90s 原节奏；lastCurrentAt 记最近一次覆盖当前账号的收集
  // （全量或 "current" 加密轮都算），决定当前账号的档位节奏。
  let lastFullAt = 0;
  let lastCurrentAt = 0;
  let timer = 0;
  let inFlight = false;
  let stopped = false;
  let armedScope: SilentQuotaScope = "all";

  function stopTimer(): void {
    if (timer !== 0) {
      clock.clearTimeout(timer);
      timer = 0;
    }
  }

  function arm(): void {
    stopTimer();
    // 界面可见性不参与排程（issue #160）：最小化/隐藏时轮询照常。
    if (stopped) {
      return;
    }
    const count = host.accountCount();
    // 首载失败不自动静默重试；成功快照后才按适应性档位 / 待续期 5s 节奏。
    if (count === null || count === 0) {
      return;
    }
    const intervalMs = silentQuotaIntervalMs(host.accounts());
    let due: number;
    if (
      intervalMs === SILENT_QUOTA_INTERVAL_MS ||
      intervalMs === SILENT_QUOTA_PENDING_RENEWAL_INTERVAL_MS
    ) {
      // 常态与待续期：单一全量节奏（待续期 5s 优先于适应性档位）。
      armedScope = "all";
      due = lastFullAt + intervalMs;
    } else {
      // 加密档（30s / 10s，issue #161）：当前账号按档位节奏加密重查，
      // 停泊账号维持 90s 全量原节奏，两者取更早到期的一档；
      // 同时到期时优先全量（顺带刷新当前账号）。
      const fullDue = lastFullAt + SILENT_QUOTA_INTERVAL_MS;
      const currentDue = lastCurrentAt + intervalMs;
      if (fullDue <= currentDue) {
        armedScope = "all";
        due = fullDue;
      } else {
        armedScope = "current";
        due = currentDue;
      }
    }
    const delay = Math.max(0, due - clock.now());
    timer = clock.setTimeout(() => {
      void run();
    }, delay);
  }

  async function run(): Promise<void> {
    // 界面可见性不参与执行决策（issue #160）。
    if (stopped) {
      return;
    }
    if (host.isBusy() || inFlight) {
      // fire 时撞上用户操作在途 / 静默在飞（reviewer P1-1）：裸早退会让调度器
      // 永久停摆——用户操作失败路径只 toast 不 arm，#160 又移除了可见性
      // show→arm 的兜底复活源。短退避后重 arm 保证存活：busy/inFlight 期间
      // 不发请求；stopped 时 arm() 自行拒绝，stop() 也会清掉本退避定时器，
      // 不会复活。退避到期由 arm() 重算正常到期时间（已过期即尽快执行）。
      stopTimer();
      timer = clock.setTimeout(() => {
        arm();
      }, SILENT_QUOTA_BUSY_BACKOFF_MS);
      return;
    }
    const count = host.accountCount();
    if (count === null || count === 0) {
      return;
    }
    inFlight = true;
    const snapshotGeneration = generation.next();
    // 排程时的范围决定本轮收集范围与完成后的节奏簿记；await 期间被重新 arm
    // 也不影响本轮已发出的请求语义。
    const scope = armedScope;
    try {
      const next = await host.listAccounts(scope);
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
      // 失败也记账（与旧 lastSnapshotAt 口径一致）：按本轮范围把对应节奏从现在
      // 起算，避免失败后 0 延迟重试打转。
      const completedAt = clock.now();
      lastCurrentAt = completedAt;
      if (scope === "all") {
        lastFullAt = completedAt;
      }
      arm();
    }
  }

  return {
    generation,
    arm,
    noteApplied() {
      // 用户发起的快照（刷新 / 切换 / 导入等）都是全量列表：两种节奏都从现在起算。
      const appliedAt = clock.now();
      lastFullAt = appliedAt;
      lastCurrentAt = appliedAt;
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
