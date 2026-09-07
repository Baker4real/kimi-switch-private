import { describe, expect, it } from "vitest";

import {
  accountFocusAttr,
  listFocusKey,
  listedActionSelector,
  pickRestoredFocus,
  setListedActionsBusy,
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

  it("忙碌选择器把每个 data-* 都限定在列表根下", () => {
    const selector = listedActionSelector(["#accounts-list", "#current-plan"]);
    const parts = selector.split(",").map((part) => part.trim());
    expect(parts.every((part) => part.startsWith("#accounts-list ") || part.startsWith("#current-plan "))).toBe(
      true,
    );
    expect(parts.some((part) => part === "#accounts-list [data-swap]")).toBe(true);
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
