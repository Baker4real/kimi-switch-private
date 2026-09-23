import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  autoSwapPacePreSwapSwitchView,
  autoSwapPreResetBurnSwitchView,
  autoSwapPreferLowerTierSwitchView,
  autoSwapResidentLowerTierSwitchView,
} from "./auto-swap";
import {
  FIXTURE_ACCOUNT_ID,
  clickListener,
  createTabSession,
  expandBehaviorPanel,
  focusKey,
  functionBlock,
  insertAccountCard,
  openModalDialog,
  openOverflowMenu,
  parkedOverflow,
  parkedRow,
  setButtonDisabled,
  showEmptyLibrary,
  showLibrary,
  tabOrder,
} from "./desktop-accept.fixture";
import { measureDesktopZoom } from "./desktop-zoom-measure";
import { accountCardHtml } from "./library-card";
import {
  applyOverflowMenuState,
  closeOverflowToggle,
  nextOverflowOpenId,
  type OverflowCloseToggleLike,
  type OverflowToggleLike,
} from "./overflow-menu";

const dir = dirname(fileURLToPath(import.meta.url));
const html = readFileSync(join(dir, "index.html"), "utf8").replace(/\r\n/g, "\n");
const mainSrc = readFileSync(join(dir, "main.ts"), "utf8").replace(/\r\n/g, "\n");

const SUBSWITCHES = [
  "auto-swap-prefer-lower-tier-toggle",
  "auto-swap-resident-lower-tier-toggle",
  "auto-swap-pace-pre-swap-toggle",
  "auto-swap-pre-reset-burn-toggle",
] as const;

function walk(source: string, options?: { modal?: boolean }): string[] {
  const session = createTabSession(source, options);
  const seen: string[] = [];
  for (let i = 0; i < session.keys().length; i += 1) {
    seen.push(session.tab());
  }
  expect(seen).toEqual(session.keys());
  return seen;
}

function pinActivation(el: NonNullable<ReturnType<ReturnType<typeof createTabSession>["current"]>>): string {
  expect(el.tag).toBe("button");
  expect(el.type).toBe("button");
  const id = el.id;
  if (id === "bind-submit") {
    expect(clickListener(mainSrc, id)).toContain("submitBind()");
    expect(functionBlock(mainSrc, "submitBind")).toContain("api.bindWebAccount(id, token)");
    return "api.bindWebAccount";
  }
  const pins: Record<string, string> = {
    "add-account-button": 'startDeviceAuth(null, "")',
    "library-add-button": 'startDeviceAuth(null, "")',
    "library-empty-add-button": 'startDeviceAuth(null, "")',
    "import-button": "api.importCurrent()",
    "renewal-toggle": "api.setProactiveRenewal(",
    "auto-swap-toggle": "api.setAutoSwap(",
    "auto-swap-prefer-lower-tier-toggle": "api.setAutoSwapPreferLowerTier(",
    "auto-swap-resident-lower-tier-toggle": "api.setAutoSwapResidentLowerTier(",
    "auto-swap-pace-pre-swap-toggle": "api.setAutoSwapPacePreSwap(",
    "auto-swap-pre-reset-burn-toggle": "api.setAutoSwapPreResetBurn(",
    "behavior-panel-toggle": "behaviorPanelExpanded = !behaviorPanelExpanded",
    "bind-cancel": 'required<HTMLDialogElement>("bind-dialog").close()',
  };
  if (id && pins[id]) {
    expect(clickListener(mainSrc, id)).toContain(pins[id]);
    return pins[id];
  }
  const list = functionBlock(mainSrc, "wireList");
  if (el.attrs["data-swap"]) {
    expect(list).toContain("runSwap(id)");
    return `runSwap:${el.attrs["data-swap"]}`;
  }
  if (el.attrs["data-overflow"]) {
    expect(list).toContain("toggleOverflowMenu(overflow)");
    return `toggleOverflow:${el.attrs["data-overflow"]}`;
  }
  if (el.attrs["data-optout"]) {
    expect(list).toContain("api.setAutoSwapOptOut(id, next)");
    return `setAutoSwapOptOut:${el.attrs["data-optout"]}`;
  }
  throw new Error(`没有钉住的动作 ${focusKey(el)}`);
}

function press(el: NonNullable<ReturnType<ReturnType<typeof createTabSession>["current"]>>): string[] {
  return (["Enter", "Space"] as const).map((key) => `${key}:${pinActivation(el)}`);
}

describe("桌面窗口与缩放（issue #169）", () => {
  it("1080×760 与 1280×880 在 100%、125%、150%、200% 页面缩放下测量溢出和可命中", async () => {
    const frames = await measureDesktopZoom();
    expect(frames.map((frame) => `${frame.window}@${frame.zoom}`)).toEqual([
      "1080×760@1",
      "1080×760@1.25",
      "1080×760@1.5",
      "1080×760@2",
      "1280×880@1",
      "1280×880@1.25",
      "1280×880@1.5",
      "1280×880@2",
    ]);
    for (const frame of frames) {
      expect(frame.innerWidth).toBe(frame.width);
      expect(frame.innerHeight).toBe(frame.height);
      expect(frame.appliedZoom).toBe(frame.zoom);
      expect(frame.pageOverflow).toBe(false);
      for (const name of ["actions", "title", "status", "swap", "submit", "open"] as const) {
        const hit = frame[name];
        expect(hit.horizontallyIn, `${frame.window} ${frame.zoom} ${name} ${hit.via}`).toBe(true);
        expect(hit.ok, `${frame.window} ${frame.zoom} ${name} ${hit.via}`).toBe(true);
        expect(hit.w).toBeGreaterThanOrEqual(8);
        expect(hit.h).toBeGreaterThanOrEqual(8);
      }
    }
  }, 30_000);
});

describe("桌面键盘（issue #169）", () => {
  const library = insertAccountCard(showLibrary(html), accountCardHtml(parkedRow(), false));

  it("总览 Tab 序到达添加与导入，Enter/Space 钉住对应命令", () => {
    expect(walk(html)).toEqual([
      "window-minimize-button",
      "window-maximize-button",
      "window-close-button",
      "href:#overview",
      "href:#library",
      "add-account-button",
      "import-button",
      "refresh-button",
    ]);
    const session = createTabSession(html);
    while (session.tab() !== "import-button") {
      // 走到导入。
    }
    expect(session.tab(true)).toBe("add-account-button");
    const add = session.current();
    expect(add).not.toBeNull();
    expect(press(add!)).toEqual([
      'Enter:startDeviceAuth(null, "")',
      'Space:startDeviceAuth(null, "")',
    ]);
    const imported = session.jumpTo("import-button");
    expect(press(imported)).toEqual([
      "Enter:api.importCurrent()",
      "Space:api.importCurrent()",
    ]);
    expect(walk(html).includes("library-add-button")).toBe(false);
  });

  it("账号库 Tab 序到达切换与行为开关，官方昵称已知时不出现更多操作", () => {
    expect(walk(library)).toEqual([
      "window-minimize-button",
      "window-maximize-button",
      "window-close-button",
      "href:#overview",
      "href:#library",
      "library-refresh-button",
      "library-add-button",
      "behavior-panel-toggle",
      `data-overflow:${FIXTURE_ACCOUNT_ID}`,
      `data-optout:${FIXTURE_ACCOUNT_ID}`,
      `data-swap:${FIXTURE_ACCOUNT_ID}`,
      `data-reauth:${FIXTURE_ACCOUNT_ID}`,
      `data-delete:${FIXTURE_ACCOUNT_ID}`,
    ]);
    expect(walk(library).includes("import-button")).toBe(false);
    const known = insertAccountCard(
      showLibrary(html),
      accountCardHtml(
        parkedRow({ official_nickname_known: true, display_name: "官方昵称" }),
        false,
      ),
    );
    expect(known).not.toContain("data-overflow");
    expect(walk(known).some((key) => key.startsWith("data-overflow:"))).toBe(false);

    const session = createTabSession(library);
    const add = session.jumpTo("library-add-button");
    expect(press(add)).toEqual([
      'Enter:startDeviceAuth(null, "")',
      'Space:startDeviceAuth(null, "")',
    ]);
    const swap = session.jumpTo(`data-swap:${FIXTURE_ACCOUNT_ID}`);
    expect(press(swap)).toEqual([
      `Enter:runSwap:${FIXTURE_ACCOUNT_ID}`,
      `Space:runSwap:${FIXTURE_ACCOUNT_ID}`,
    ]);

    const expanded = expandBehaviorPanel(library);
    expect(walk(expanded)).toEqual([
      "window-minimize-button",
      "window-maximize-button",
      "window-close-button",
      "href:#overview",
      "href:#library",
      "library-refresh-button",
      "library-add-button",
      "behavior-panel-toggle",
      "renewal-toggle",
      "auto-swap-toggle",
      `data-overflow:${FIXTURE_ACCOUNT_ID}`,
      `data-optout:${FIXTURE_ACCOUNT_ID}`,
      `data-swap:${FIXTURE_ACCOUNT_ID}`,
      `data-reauth:${FIXTURE_ACCOUNT_ID}`,
      `data-delete:${FIXTURE_ACCOUNT_ID}`,
    ]);
    expect(walk(expanded).some((key) => SUBSWITCHES.includes(key as (typeof SUBSWITCHES)[number]))).toBe(
      false,
    );
    const panel = createTabSession(expanded);
    expect(press(panel.jumpTo("behavior-panel-toggle"))).toEqual([
      "Enter:behaviorPanelExpanded = !behaviorPanelExpanded",
      "Space:behaviorPanelExpanded = !behaviorPanelExpanded",
    ]);
    expect(press(panel.jumpTo("renewal-toggle"))).toEqual([
      "Enter:api.setProactiveRenewal(",
      "Space:api.setProactiveRenewal(",
    ]);
    expect(press(panel.jumpTo("auto-swap-toggle"))).toEqual([
      "Enter:api.setAutoSwap(",
      "Space:api.setAutoSwap(",
    ]);
    expect(press(panel.jumpTo(`data-optout:${FIXTURE_ACCOUNT_ID}`))).toEqual([
      `Enter:setAutoSwapOptOut:${FIXTURE_ACCOUNT_ID}`,
      `Space:setAutoSwapOptOut:${FIXTURE_ACCOUNT_ID}`,
    ]);

    expect(autoSwapPreferLowerTierSwitchView(false, true).disabled).toBe(true);
    expect(autoSwapPreferLowerTierSwitchView(true, false).disabled).toBe(false);
    expect(autoSwapResidentLowerTierSwitchView(true, false).disabled).toBe(false);
    expect(autoSwapPreResetBurnSwitchView(true, false).disabled).toBe(false);
    expect(autoSwapPacePreSwapSwitchView(true, false).disabled).toBe(false);
    let enabled = expanded;
    for (const id of SUBSWITCHES) {
      enabled = setButtonDisabled(enabled, id, false);
    }
    const enabledKeys = walk(enabled);
    expect(enabledKeys.slice(enabledKeys.indexOf("auto-swap-toggle") + 1, enabledKeys.indexOf("auto-swap-toggle") + 5)).toEqual(
      [...SUBSWITCHES],
    );
    const sub = createTabSession(enabled);
    expect(clickListener(mainSrc, "auto-swap-prefer-lower-tier-toggle")).toContain("if (toggle.disabled)");
    expect(press(sub.jumpTo("auto-swap-prefer-lower-tier-toggle"))).toEqual([
      "Enter:api.setAutoSwapPreferLowerTier(",
      "Space:api.setAutoSwapPreferLowerTier(",
    ]);
  });

  it("空态添加入口在 Tab 序里，Enter/Space 仍是添加", () => {
    expect(mainSrc).toMatch(/classList\.toggle\(\s*"hidden",\s*next\.accounts\.length > 0\s*\)/);
    const empty = showEmptyLibrary(showLibrary(html));
    expect(walk(empty)).toContain("library-empty-add-button");
    const session = createTabSession(empty);
    expect(press(session.jumpTo("library-empty-add-button"))).toEqual([
      'Enter:startDeviceAuth(null, "")',
      'Space:startDeviceAuth(null, "")',
    ]);
  });

  it("更多操作打开后下一项是重命名，Escape 把焦点送回开关", () => {
    const opened = openOverflowMenu(library);
    const keys = walk(opened);
    const at = keys.indexOf(`data-overflow:${FIXTURE_ACCOUNT_ID}`);
    expect(keys[at + 1]).toBe(`data-rename:${FIXTURE_ACCOUNT_ID}`);
    expect(tabOrder(library).some((el) => el.attrs["data-rename"])).toBe(false);

    const session = createTabSession(opened);
    expect(press(session.jumpTo(`data-overflow:${FIXTURE_ACCOUNT_ID}`))).toEqual([
      `Enter:toggleOverflow:${FIXTURE_ACCOUNT_ID}`,
      `Space:toggleOverflow:${FIXTURE_ACCOUNT_ID}`,
    ]);
    expect(session.tab()).toBe(`data-rename:${FIXTURE_ACCOUNT_ID}`);

    const dismiss = functionBlock(mainSrc, "wireOverflowDismiss");
    expect(dismiss).toContain('event.key === "Escape"');
    expect(dismiss).toContain("closeOverflowMenus()");

    const { toggle, menu, rename } = parkedOverflow(FIXTURE_ACCOUNT_ID);
    expect(nextOverflowOpenId("toggle", FIXTURE_ACCOUNT_ID, null)).toBe(FIXTURE_ACCOUNT_ID);
    applyOverflowMenuState(toggle as unknown as OverflowToggleLike, true);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(menu.hidden).toBe(false);
    closeOverflowToggle(toggle as unknown as OverflowCloseToggleLike, rename as unknown as Element);
    expect(menu.hidden).toBe(true);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(toggle.focusCount).toBe(1);

    applyOverflowMenuState(toggle as unknown as OverflowToggleLike, true);
    const before = toggle.focusCount;
    closeOverflowToggle(toggle as unknown as OverflowCloseToggleLike, toggle as unknown as Element);
    expect(toggle.focusCount).toBe(before);
    expect(menu.hidden).toBe(true);
    expect(nextOverflowOpenId("dismiss", FIXTURE_ACCOUNT_ID, FIXTURE_ACCOUNT_ID)).toBeNull();
  });

  it("绑定对话框 Escape 走原生关闭，手动提交按钮可聚焦", () => {
    expect(mainSrc).toContain('required<HTMLDialogElement>("bind-dialog").showModal()');
    expect(mainSrc).not.toMatch(/bind-dialog"\)\.addEventListener\("cancel"/);
    expect(mainSrc).not.toMatch(/bind-dialog"\)\.addEventListener\("keydown"/);
    const close = functionBlock(mainSrc, "wireDialogs");
    expect(close).toContain('required<HTMLDialogElement>("bind-dialog").addEventListener("close"');
    expect(close).toContain('input.value = ""');

    const dialog = openModalDialog(html, "bind-dialog");
    expect(walk(dialog, { modal: true })).toEqual([
      "bind-open",
      "bind-open-console",
      "bind-token",
      "bind-cancel",
      "bind-submit",
    ]);
    const session = createTabSession(dialog, { modal: true });
    expect(session.jumpTo("bind-token").tag).toBe("textarea");
    expect(press(session.jumpTo("bind-cancel"))).toEqual([
      'Enter:required<HTMLDialogElement>("bind-dialog").close()',
      'Space:required<HTMLDialogElement>("bind-dialog").close()',
    ]);
    expect(press(session.jumpTo("bind-submit"))).toEqual([
      "Enter:api.bindWebAccount",
      "Space:api.bindWebAccount",
    ]);
  });
});
