import { describe, expect, it } from "vitest";

import {
  applyOverflowMenuState,
  nextOverflowOpenId,
  type OverflowToggleLike,
} from "./overflow-menu";

describe("nextOverflowOpenId（issue #143 评审修订：互斥与全收）", () => {
  it("点开关展开：互斥——B 已展开时点 A，只留 A", () => {
    expect(nextOverflowOpenId("toggle", "acc-a", "acc-b")).toBe("acc-a");
  });

  it("再点同一开关收起", () => {
    expect(nextOverflowOpenId("toggle", "acc-a", "acc-a")).toBeNull();
  });

  it("没有展开的菜单时点开关直接展开", () => {
    expect(nextOverflowOpenId("toggle", "acc-a", null)).toBe("acc-a");
  });

  it("点菜单项、点外部或按 Escape 一律全收起", () => {
    expect(nextOverflowOpenId("item-activate", "acc-a", "acc-a")).toBeNull();
    expect(nextOverflowOpenId("dismiss", "acc-a", "acc-a")).toBeNull();
    expect(nextOverflowOpenId("dismiss", "acc-a", null)).toBeNull();
  });
});

describe("applyOverflowMenuState（aria-expanded 翻转与菜单 hidden 同步）", () => {
  function fakeToggle(): { toggle: OverflowToggleLike; menu: { hidden: boolean } } {
    const menu = { hidden: true };
    const attrs = new Map<string, string>();
    const toggle: OverflowToggleLike = {
      getAttribute: (name) => attrs.get(name) ?? null,
      setAttribute: (name, value) => {
        attrs.set(name, value);
      },
      parentElement: {
        querySelector: <E extends Element = Element>(selectors: string): E | null =>
          selectors === ".profile-overflow-menu" ? (menu as unknown as E) : null,
      },
    };
    return { toggle, menu };
  }

  it("展开：aria-expanded 翻转为 true，菜单取消 hidden", () => {
    const { toggle, menu } = fakeToggle();
    applyOverflowMenuState(toggle, true);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(menu.hidden).toBe(false);
  });

  it("收起：aria-expanded 归 false，菜单重新 hidden（Escape / 外点同路径）", () => {
    const { toggle, menu } = fakeToggle();
    applyOverflowMenuState(toggle, true);
    applyOverflowMenuState(toggle, false);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(menu.hidden).toBe(true);
  });

  it("菜单节点缺失时只翻转开关状态，不抛错", () => {
    const attrs = new Map<string, string>();
    const toggle: OverflowToggleLike = {
      getAttribute: (name) => attrs.get(name) ?? null,
      setAttribute: (name, value) => {
        attrs.set(name, value);
      },
      parentElement: {
        querySelector: <E extends Element = Element>(): E | null => null,
      },
    };
    expect(() => applyOverflowMenuState(toggle, true)).not.toThrow();
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
  });
});
