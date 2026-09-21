/** 账号卡「…」菜单的开合状态机与就地应用（issue #143 评审修订）。
 * 语义是 disclosure：只有 aria-expanded 开合与 aria-label 名称，
 * 菜单项是普通按钮，不声明 role="menu" 键盘契约。
 * issue #167 增补：开关与唯一面板的 aria-controls 控制关系（linkOverflowPanels），
 * 以及收起时的焦点去向裁决（closeOverflowToggle，Escape 归还 / 外点不抢）。 */

/** 驱动开合的事件来源。 */
export type OverflowMenuEvent =
  | "toggle" // 点「…」开关
  | "item-activate" // 点菜单项（重命名）
  | "dismiss"; // 点卡片外或按 Escape

/**
 * 计算下一个展开的「…」开关账号 id，null 表示全部收起。
 * 互斥：任意时刻至多一个菜单展开；再点同一开关收起；
 * 点菜单项、点外部与 Escape 一律全收。
 */
export function nextOverflowOpenId(
  event: OverflowMenuEvent,
  toggleId: string,
  currentOpenId: string | null,
): string | null {
  if (event === "toggle") {
    return currentOpenId === toggleId ? null : toggleId;
  }
  return null;
}

/** 开关的最小结构面：真实 HTMLButtonElement 与测试桩都满足。 */
export interface OverflowToggleLike {
  getAttribute(name: string): string | null;
  setAttribute(name: string, value: string): void;
  parentElement: {
    querySelector<E extends Element = Element>(selectors: string): E | null;
  } | null;
}

/** 把开合状态就地写到一个开关上：aria-expanded 翻转，菜单 hidden 同步。 */
export function applyOverflowMenuState(toggle: OverflowToggleLike, open: boolean): void {
  toggle.setAttribute("aria-expanded", open ? "true" : "false");
  const menu = toggle.parentElement?.querySelector<HTMLElement>(".profile-overflow-menu");
  if (menu) {
    menu.hidden = !open;
  }
}

/** 「…」菜单面板的 DOM id 前缀（issue #167）：账号 id 折叠掉非 [A-Za-z0-9_-] 字符。
 * 同批两个账号折叠后可能撞名，由 linkOverflowPanels 加序号去重。 */
export function overflowPanelIdBase(accountId: string): string {
  return `overflow-menu-${accountId.replace(/[^A-Za-z0-9_-]/g, "_")}`;
}

/**
 * 给一批「…」开关与各自唯一菜单面板建立可读控制关系（issue #167）：
 * 面板取账号 id 折叠派生的 DOM id（撞名加序号保证同批唯一），
 * 开关补 aria-controls 指向。只补属性，开合事实来源仍是 aria-expanded。
 */
export function linkOverflowPanels(toggles: Iterable<OverflowToggleLike>): void {
  const used = new Set<string>();
  for (const toggle of toggles) {
    const menu = toggle.parentElement?.querySelector<HTMLElement>(".profile-overflow-menu");
    if (!menu) {
      continue;
    }
    const base = overflowPanelIdBase(toggle.getAttribute("data-overflow") ?? "");
    let id = base;
    let suffix = 2;
    while (used.has(id)) {
      id = `${base}-${suffix}`;
      suffix += 1;
    }
    used.add(id);
    menu.id = id;
    toggle.setAttribute("aria-controls", id);
  }
}

/** 焦点去向裁决的最小面：即将隐藏的菜单与当前焦点元素。 */
export interface OverflowCloseFocusQuery {
  expanded: boolean;
  menu: { contains(node: Node | null): boolean } | null;
  activeElement: Element | null;
}

/**
 * 收起时是否把焦点送回「…」开关（issue #167，#164 spec D 行）：
 * 只有当前焦点就落在即将隐藏的菜单内容里才送回——菜单 hidden 后浏览器会把
 * 焦点甩到 body，必须显式归还；焦点不在菜单里时一律不动（外点关闭不抢焦点）。
 */
export function shouldReturnFocusOnClose(query: OverflowCloseFocusQuery): boolean {
  if (!query.expanded || query.menu === null || query.activeElement === null) {
    return false;
  }
  return query.menu.contains(query.activeElement);
}

/** 收起路径的开关最小结构面：在开关面之上加 focus。 */
export interface OverflowCloseToggleLike extends OverflowToggleLike {
  focus(): void;
}

/**
 * 收起单个「…」菜单：状态归位（aria-expanded 归 false、菜单 hidden）加焦点裁决。
 * Escape / 外点 / 菜单项触发共用；仅当焦点在被隐藏的菜单内容里才把焦点送回开关。
 */
export function closeOverflowToggle(
  toggle: OverflowCloseToggleLike,
  activeElement: Element | null,
): void {
  const expanded = toggle.getAttribute("aria-expanded") === "true";
  const menu = toggle.parentElement?.querySelector<HTMLElement>(".profile-overflow-menu") ?? null;
  const returnFocus = shouldReturnFocusOnClose({ expanded, menu, activeElement });
  applyOverflowMenuState(toggle, false);
  if (returnFocus) {
    toggle.focus();
  }
}
