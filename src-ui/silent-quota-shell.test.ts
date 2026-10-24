import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { JSDOM } from "jsdom";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { Snapshot } from "./api";

// GUI 外壳真实接线（issue #183）：真实页面模板 + 真实外壳 + 真实静默调度器，
// 只 mock 后端命令边界（api 模块）；断言外壳向后端发出的列出请求与时间点。
vi.mock("./api", async () => {
  const actual = await vi.importActual<typeof import("./api")>("./api");
  return {
    ...actual,
    api: {
      ...actual.api,
      listAccounts: vi.fn(),
    },
  };
});

const dir = dirname(fileURLToPath(import.meta.url));
const html = readFileSync(join(dir, "index.html"), "utf8").replace(/\r\n/g, "\n");

const TEN_MINUTES_MS = 600_000;

type AccountRow = Snapshot["accounts"][number];

/** 外壳发出的一次列出：相对首载的发出时刻、收集范围、是否用户发起。 */
interface Listing {
  at: number;
  /** "all" 全量（停泊账号也查）；"current" 只查当前账号。 */
  scope: "all" | "current";
  userInitiated: boolean;
}

function accountRow(id: string, active: boolean, ratio: number): AccountRow {
  return {
    id,
    label: id,
    active,
    membership: null,
    membership_expiry: null,
    membership_remaining_days: null,
    membership_period_days: null,
    membership_expiry_hint: false,
    quotas: [
      {
        window: "5 小时窗口",
        ratio,
        text: `${Math.round(ratio * 100)}%`,
        reset: null,
        tone: "high",
      },
    ],
    error: null,
    credential_expired: false,
    cached_at: null,
    display_name: id,
    official_nickname_known: false,
    auto_swap_opt_out: false,
  };
}

/** 「当前账号 + 一个停泊账号」快照，开着自动切换（与 #182 故障现场一致）。 */
function snapshotWithCurrentAt(currentRatio: number): Snapshot {
  const accounts = [accountRow("current", true, currentRatio), accountRow("parked", false, 0.26)];
  return {
    accounts,
    current_label: "current",
    account_count: accounts.length,
    urgent_count: 1,
    proactive_renewal: false,
    auto_swap: true,
    auto_swap_prefer_lower_tier: false,
    auto_swap_resident_lower_tier: false,
    auto_swap_pre_reset_burn: false,
    auto_swap_pace_pre_swap: false,
    message: null,
  };
}

/** 时刻序列（补上观察起点 0 与终点 endAt）里相邻两点的最大间隔。 */
function longestGapMs(times: readonly number[], endAt: number): number {
  const points = [0, ...times, endAt];
  let longest = 0;
  for (let i = 1; i < points.length; i += 1) {
    longest = Math.max(longest, points[i] - points[i - 1]);
  }
  return longest;
}

let teardown: (() => void) | null = null;

afterEach(() => {
  teardown?.();
  teardown = null;
});

/** 在 JSDOM 里启动真实外壳：首载返回「当前账号 currentRatio + 一个停泊账号」，之后每次列出同样返回。 */
async function bootShell(currentRatio: number) {
  const dom = new JSDOM(html, { url: "http://127.0.0.1/" });
  const win = dom.window;
  const previous = {
    document: globalThis.document,
    window: globalThis.window,
    HTMLElement: globalThis.HTMLElement,
    HTMLButtonElement: globalThis.HTMLButtonElement,
    HTMLInputElement: globalThis.HTMLInputElement,
    Element: globalThis.Element,
  };
  globalThis.document = win.document;
  globalThis.window = win as unknown as Window & typeof globalThis;
  globalThis.HTMLElement = win.HTMLElement;
  globalThis.HTMLButtonElement = win.HTMLButtonElement;
  globalThis.HTMLInputElement = win.HTMLInputElement;
  globalThis.Element = win.Element;
  // 外壳经 window.setTimeout 排静默定时器，而 vi 假时钟只伪造全局计时器：
  // 把 JSDOM window 的计时器转给（调用时的）全局实现，推进假时钟才驱动调度器。
  win.setTimeout = ((handler: () => void, delayMs?: number) =>
    globalThis.setTimeout(handler, delayMs)) as unknown as typeof win.setTimeout;
  win.clearTimeout = ((id?: number) =>
    globalThis.clearTimeout(id)) as unknown as typeof win.clearTimeout;
  teardown = () => {
    // 走外壳真实的 pagehide 停止路径，再拆假时钟与全局。
    win.dispatchEvent(new win.Event("pagehide"));
    vi.useRealTimers();
    Object.assign(globalThis, previous);
    dom.window.close();
  };

  // 每条用例一份全新的外壳模块（调度器、store 都是模块级状态）；先导入再装假时钟。
  vi.resetModules();
  const { api } = await import("./api");
  const { init } = await import("./main");
  vi.useFakeTimers();

  const startedAt = Date.now();
  const listings: Listing[] = [];
  vi.mocked(api.listAccounts).mockImplementation((userInitiated = false, currentOnly = false) => {
    listings.push({
      at: Date.now() - startedAt,
      scope: currentOnly ? "current" : "all",
      userInitiated,
    });
    return Promise.resolve(snapshotWithCurrentAt(currentRatio));
  });
  await init();

  return {
    listings,
    advance: (ms: number) => vi.advanceTimersByTimeAsync(ms),
    click(id: string) {
      const button = win.document.getElementById(id);
      if (!(button instanceof win.HTMLButtonElement)) {
        throw new Error(`缺少按钮: ${id}`);
      }
      button.click();
    },
  };
}

describe("GUI 外壳静默轮询双节奏（issue #183）", () => {
  it("当前账号 97%（10s 档）：每个 90s 周期至少一次全量列出，其余只查当前账号", async () => {
    const shell = await bootShell(0.97);
    await shell.advance(TEN_MINUTES_MS);

    const [initial, ...silent] = shell.listings;
    expect(initial).toEqual({ at: 0, scope: "all", userInitiated: false });
    const fullAt = silent.filter((listing) => listing.scope === "all").map((listing) => listing.at);
    // 第 90、180 … 540 秒各一轮全量：次数与 10 分钟时长相符，不多查停泊账号。
    expect(fullAt).toHaveLength(6);
    // 每个 90s 周期内（允许一轮 10s 执行误差）至少一次全量列出。
    expect(longestGapMs(fullAt, TEN_MINUTES_MS)).toBeLessThanOrEqual(90_000 + 10_000);
    // 其余列出都只查当前账号，当前账号维持 10s 档节奏。
    expect(longestGapMs(silent.map((listing) => listing.at), TEN_MINUTES_MS)).toBeLessThanOrEqual(
      10_000,
    );
  });

  it("当前账号 85%（30s 档）：每个 90s 周期至少一次全量列出，其余只查当前账号", async () => {
    const shell = await bootShell(0.85);
    await shell.advance(TEN_MINUTES_MS);

    const [initial, ...silent] = shell.listings;
    expect(initial).toEqual({ at: 0, scope: "all", userInitiated: false });
    const fullAt = silent.filter((listing) => listing.scope === "all").map((listing) => listing.at);
    // 第 90、180 … 540 秒各一轮全量：次数与 10 分钟时长相符，不多查停泊账号。
    expect(fullAt).toHaveLength(6);
    // 每个 90s 周期内（允许一轮 30s 执行误差）至少一次全量列出。
    expect(longestGapMs(fullAt, TEN_MINUTES_MS)).toBeLessThanOrEqual(90_000 + 30_000);
    // 其余列出都只查当前账号，当前账号维持 30s 档节奏。
    expect(longestGapMs(silent.map((listing) => listing.at), TEN_MINUTES_MS)).toBeLessThanOrEqual(
      30_000,
    );
  });

  it("点「刷新」后两条节奏从此刻起算：下一次全量约在 90s 后，不多查停泊账号", async () => {
    const shell = await bootShell(0.97);
    // 首载后 45s 点「刷新」：此时刷新前的旧全量轮（首载后第 90s）还没到期。
    const refreshAt = 45_000;
    await shell.advance(refreshAt);
    shell.click("refresh-button");
    await shell.advance(0);
    expect(shell.listings.filter((listing) => listing.userInitiated)).toEqual([
      { at: refreshAt, scope: "all", userInitiated: true },
    ]);

    await shell.advance(120_000);
    const sinceRefresh = shell.listings
      .filter((listing) => listing.at > refreshAt)
      .map((listing) => ({ ...listing, at: listing.at - refreshAt }));
    // 当前账号的 10s 档从刷新时刻起算。
    expect(sinceRefresh[0]).toEqual({ at: 10_000, scope: "current", userInitiated: false });
    // 停泊账号的 90s 全量也从刷新时刻起算：不再按旧节奏在首载后第 90s 多查一次，
    // 下一次全量在刷新后约 90s（允许一轮 10s 执行误差）。
    const fullSinceRefresh = sinceRefresh
      .filter((listing) => listing.scope === "all")
      .map((listing) => listing.at);
    expect(fullSinceRefresh[0]).toBeGreaterThanOrEqual(90_000);
    expect(fullSinceRefresh[0]).toBeLessThanOrEqual(90_000 + 10_000);
  });
});
