/** 账号卡「…」菜单的开合状态机与就地应用（issue #143 评审修订）。
 * 语义是 disclosure：只有 aria-expanded 开合与 aria-label 名称，
 * 菜单项是普通按钮，不声明 role="menu" 键盘契约。 */

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
