import { describe, expect, it } from "vitest";

import {
  applyOverflowMenuState,
  closeOverflowToggle,
  linkOverflowPanels,
  nextOverflowOpenId,
  overflowPanelIdBase,
  shouldReturnFocusOnClose,
  type OverflowCloseToggleLike,
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

/** 收起/关联路径的菜单桩：记录 hidden 与 id，contains 只认传入的焦点元素。 */
function stubMenu(inside: Element | null): {
  hidden: boolean;
  id: string;
  contains(node: Node | null): boolean;
} {
  return {
    hidden: true,
    id: "",
    contains: (node) => node !== null && node === inside,
  };
}

/** 收起路径的开关桩：真实 HTMLButtonElement 的最小结构面。 */
function fakeToggle(
  accountId: string | null,
  menu: { hidden: boolean; id: string; contains(node: Node | null): boolean } | null,
): { toggle: OverflowCloseToggleLike; attrs: Map<string, string>; focusCount: () => number } {
  const attrs = new Map<string, string>();
  if (accountId !== null) {
    attrs.set("data-overflow", accountId);
  }
  let focused = 0;
  const toggle: OverflowCloseToggleLike = {
    getAttribute: (name) => attrs.get(name) ?? null,
    setAttribute: (name, value) => {
      attrs.set(name, value);
    },
    focus: () => {
      focused += 1;
    },
    parentElement: {
      querySelector: <E extends Element = Element>(selectors: string): E | null =>
        selectors === ".profile-overflow-menu" ? (menu as unknown as E | null) : null,
    },
  };
  return { toggle, attrs, focusCount: () => focused };
}

describe("overflowPanelIdBase（issue #167 面板 id 派生）", () => {
  it("账号 id 折叠非法字符并加前缀，得到可用 DOM id", () => {
    expect(overflowPanelIdBase("acc-1")).toBe("overflow-menu-acc-1");
    expect(overflowPanelIdBase("a.b/c")).toBe("overflow-menu-a_b_c");
    expect(overflowPanelIdBase('怪"id')).toBe("overflow-menu-__id");
  });

  it("空账号 id 仍得到带前缀的合法 id，不抛错", () => {
    expect(overflowPanelIdBase("")).toBe("overflow-menu-");
  });
});

describe("linkOverflowPanels（aria-controls 指向唯一面板）", () => {
  it("开关补 aria-controls，指向同卡唯一菜单面板的新 id", () => {
    const menu = stubMenu(null);
    const { toggle, attrs } = fakeToggle("acc-1", menu);
    linkOverflowPanels([toggle]);
    expect(menu.id).toBe("overflow-menu-acc-1");
    expect(attrs.get("aria-controls")).toBe("overflow-menu-acc-1");
  });

  it("两个账号 id 折叠后撞名时加序号去重，各自指向各自面板", () => {
    const menuA = stubMenu(null);
    const menuB = stubMenu(null);
    const a = fakeToggle("a b", menuA);
    const b = fakeToggle("a_b", menuB);
    linkOverflowPanels([a.toggle, b.toggle]);
    expect(menuA.id).toBe("overflow-menu-a_b");
    expect(menuB.id).toBe("overflow-menu-a_b-2");
    expect(a.attrs.get("aria-controls")).toBe("overflow-menu-a_b");
    expect(b.attrs.get("aria-controls")).toBe("overflow-menu-a_b-2");
  });

  it("菜单缺失时跳过该开关：不写 aria-controls、不抛错", () => {
    const { toggle, attrs } = fakeToggle("acc-1", null);
    expect(() => linkOverflowPanels([toggle])).not.toThrow();
    expect(attrs.get("aria-controls")).toBeUndefined();
  });

  it("只补控制关系，不改写 aria-expanded 的开合事实", () => {
    const menu = stubMenu(null);
    const { toggle, attrs } = fakeToggle("acc-1", menu);
    attrs.set("aria-expanded", "true");
    linkOverflowPanels([toggle]);
    expect(attrs.get("aria-expanded")).toBe("true");
  });
});

describe("shouldReturnFocusOnClose（收起焦点去向裁决，issue #167）", () => {
  it("焦点在被隐藏的菜单内容里才送回开关", () => {
    const item = {} as Element;
    const menu = stubMenu(item);
    expect(
      shouldReturnFocusOnClose({ expanded: true, menu, activeElement: item }),
    ).toBe(true);
  });

  it("焦点在菜单外（其他元素 / body / 无焦点）不送回", () => {
    const item = {} as Element;
    const outside = {} as Element;
    const menu = stubMenu(item);
    expect(
      shouldReturnFocusOnClose({ expanded: true, menu, activeElement: outside }),
    ).toBe(false);
    expect(
      shouldReturnFocusOnClose({ expanded: true, menu, activeElement: null }),
    ).toBe(false);
  });

  it("菜单缺失或本就未展开时不送回", () => {
    expect(
      shouldReturnFocusOnClose({ expanded: true, menu: null, activeElement: {} as Element }),
    ).toBe(false);
    expect(
      shouldReturnFocusOnClose({
        expanded: false,
        menu: stubMenu(null),
        activeElement: {} as Element,
      }),
    ).toBe(false);
  });
});

describe("closeOverflowToggle（收起 + 条件归还焦点）", () => {
  it("开合翻转不动 aria-controls 的控制指向", () => {
    const { toggle, attrs } = fakeToggle("acc-1", stubMenu(null));
    attrs.set("aria-controls", "overflow-menu-acc-1");
    applyOverflowMenuState(toggle, true);
    applyOverflowMenuState(toggle, false);
    expect(attrs.get("aria-controls")).toBe("overflow-menu-acc-1");
  });

  it("焦点在菜单项上 Escape：状态归位并把焦点送回开关", () => {
    const item = {} as Element;
    const menu = stubMenu(item);
    const { toggle, attrs, focusCount } = fakeToggle("acc-1", menu);
    attrs.set("aria-expanded", "true");
    menu.hidden = false;
    closeOverflowToggle(toggle, item);
    expect(attrs.get("aria-expanded")).toBe("false");
    expect(menu.hidden).toBe(true);
    expect(focusCount()).toBe(1);
  });

  it("焦点在菜单外收起（外点 / 再点开关）：不动焦点", () => {
    const menu = stubMenu({} as Element);
    const { toggle, attrs, focusCount } = fakeToggle("acc-1", menu);
    attrs.set("aria-expanded", "true");
    closeOverflowToggle(toggle, null);
    expect(attrs.get("aria-expanded")).toBe("false");
    expect(menu.hidden).toBe(true);
    expect(focusCount()).toBe(0);
  });

  it("未展开的开关安全归位，不抢焦点", () => {
    const { toggle, attrs, focusCount } = fakeToggle("acc-1", stubMenu(null));
    closeOverflowToggle(toggle, null);
    expect(attrs.get("aria-expanded")).toBe("false");
    expect(focusCount()).toBe(0);
  });
});
