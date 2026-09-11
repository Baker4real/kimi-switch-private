import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  accountFocusAttr,
  controlDisabledByBusy,
  createAppStore,
  listFocusKey,
  listedActionSelector,
  overviewCurrentEmptyCopy,
  parkedAccountCount,
  pickRestoredFocus,
  setListedActionsBusy,
  shouldArmToastDismiss,
} from "./store";

describe("列表保焦", () => {
  it("账号卡带 data-account，供重建后按账号找回", () => {
    expect(accountFocusAttr("bob")).toBe('data-account="bob"');
    expect(accountFocusAttr(`a"b`)).toBe('data-account="a&quot;b"');
  });

  it("从切换钮读出账号 id", () => {
    expect(listFocusKey({ swap: "bob" })).toEqual({
      action: "swap",
      accountId: "bob",
    });
  });

  it("重建后优先同一 data-swap 控件", () => {
    const key = listFocusKey({ swap: "bob" });
    expect(key).not.toBeNull();
    const found = pickRestoredFocus(key!, (selector) =>
      selector.includes('data-swap="bob"') ? "swap-btn" : null,
    );
    expect(found).toBe("swap-btn");
  });

  it("切换成功后焦点落到同账号操作列按钮", () => {
    const key = listFocusKey({ swap: "bob" });
    expect(key).not.toBeNull();
    const found = pickRestoredFocus(key!, (selector) => {
      if (selector.includes("data-swap")) {
        return null;
      }
      if (
        selector.includes('data-account="bob"') &&
        selector.includes("profile-card-actions")
      ) {
        return "reauth-btn";
      }
      return null;
    });
    expect(found).toBe("reauth-btn");
  });

  it("从「…」开关读出账号 id（issue #143）", () => {
    expect(listFocusKey({ overflow: "bob" })).toEqual({
      action: "overflow",
      accountId: "bob",
    });
  });

  it("重建后优先同一 data-overflow 控件，缺失时回落同卡按钮（issue #143）", () => {
    const key = listFocusKey({ overflow: "bob" });
    expect(key).not.toBeNull();
    const found = pickRestoredFocus(key!, (selector) =>
      selector.includes('data-overflow="bob"') ? "overflow-btn" : null,
    );
    expect(found).toBe("overflow-btn");
    // 官方昵称补全后「…」整颗消失，焦点仍能回到这张卡的等价控件。
    const fallback = pickRestoredFocus(key!, (selector) =>
      selector.includes('data-account="bob"') && selector.includes("button") ? "card-btn" : null,
    );
    expect(fallback).toBe("card-btn");
  });

  it("忙碌选择器把每个 data-* 都限定在列表根下", () => {
    const selector = listedActionSelector(["#accounts-list", "#current-plan"]);
    const parts = selector.split(",").map((part) => part.trim());
    expect(parts.every((part) => part.startsWith("#accounts-list ") || part.startsWith("#current-plan "))).toBe(
      true,
    );
    expect(parts.some((part) => part === "#accounts-list [data-swap]")).toBe(true);
    expect(parts.some((part) => part === "#accounts-list [data-optout]")).toBe(true);
    expect(parts.some((part) => part === "#accounts-list [data-overflow]")).toBe(true);
    expect(parts.some((part) => part === "#current-plan [data-bind]")).toBe(true);
  });

  it("忙碌时只切换 disabled，不替换按钮对象", () => {
    const swap = { disabled: false };
    const buttons = [swap];
    setListedActionsBusy(buttons, true);
    expect(buttons[0]).toBe(swap);
    expect(swap.disabled).toBe(true);
    setListedActionsBusy(buttons, false);
    expect(swap.disabled).toBe(false);
  });
});

describe("空闲 pill 与总览空态（issue #127）", () => {
  it("空闲 busyLabel 是「就绪」，忙完后回到同一文案", () => {
    const store = createAppStore();
    expect(store.get().busyLabel).toBe("就绪");
    store.setBusy(true, "加载中…");
    expect(store.get().busyLabel).toBe("加载中…");
    store.setBusy(false);
    expect(store.get().busyLabel).toBe("就绪");
  });

  it("停泊账号数 = 总数减去当前账号（若有）", () => {
    expect(parkedAccountCount(0, false)).toBe(0);
    expect(parkedAccountCount(0, true)).toBe(0);
    expect(parkedAccountCount(1, true)).toBe(0);
    expect(parkedAccountCount(2, false)).toBe(2);
    expect(parkedAccountCount(3, true)).toBe(2);
  });

  it("总览无当前账号时按账号数分支空态文案", () => {
    expect(overviewCurrentEmptyCopy(0)).toBe("请先添加或导入");
    expect(overviewCurrentEmptyCopy(1)).toBe("请到「账号库」切换");
    expect(overviewCurrentEmptyCopy(4)).toBe("请到「账号库」切换");
  });
});

describe("忙碌闸门与错误 toast 计时（issue #126）", () => {
  it("忙碌或视图禁用任一为真则控件禁用", () => {
    expect(controlDisabledByBusy(true, false)).toBe(true);
    expect(controlDisabledByBusy(true, true)).toBe(true);
    expect(controlDisabledByBusy(false, true)).toBe(true);
    expect(controlDisabledByBusy(false, false)).toBe(false);
  });

  it("对话框打开时错误 toast 不排程；成功 toast 仍排程", () => {
    expect(shouldArmToastDismiss(true, true)).toBe(false);
    expect(shouldArmToastDismiss(true, false)).toBe(true);
    expect(shouldArmToastDismiss(false, true)).toBe(true);
    expect(shouldArmToastDismiss(false, false)).toBe(true);
  });

  it("忙碌列表含四个设置开关；子开关 paint 与忙碌取或", () => {
    const main = readFileSync(
      join(dirname(fileURLToPath(import.meta.url)), "main.ts"),
      "utf8",
    );
    const busyList = main.slice(
      main.indexOf("const BUSY_BUTTON_IDS = ["),
      main.indexOf("] as const;", main.indexOf("const BUSY_BUTTON_IDS = [")),
    );
    expect(busyList).toContain('"renewal-toggle"');
    expect(busyList).toContain('"auto-swap-toggle"');
    expect(busyList).toContain('"auto-swap-prefer-lower-tier-toggle"');
    expect(busyList).toContain('"auto-swap-resident-lower-tier-toggle"');
    expect(busyList).not.toContain("behavior-panel-toggle");

    const functionSrc = (name: string): string => {
      const start = main.indexOf(`function ${name}(`);
      expect(start, `缺少函数: ${name}`).toBeGreaterThan(-1);
      const next = main.indexOf("\nfunction ", start + 1);
      return main.slice(start, next < 0 ? main.length : next);
    };
    const prefer = functionSrc("paintAutoSwapPreferLowerTier");
    const resident = functionSrc("paintAutoSwapResidentLowerTier");
    expect(prefer).toContain("controlDisabledByBusy");
    expect(resident).toContain("controlDisabledByBusy");
    expect(prefer).not.toMatch(/toggle\.disabled\s*=\s*view\.disabled\s*;/);
    expect(resident).not.toMatch(/toggle\.disabled\s*=\s*view\.disabled\s*;/);
    expect(functionSrc("syncBusyChrome")).toContain("controlDisabledByBusy");
  });
});
