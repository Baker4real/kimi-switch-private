import type { AccountRow } from "./api";

/** #169 桌面验收夹具：Tab 序与菜单焦点。不读用户账号，不改系统缩放。 */

export const FIXTURE_ACCOUNT_ID = "fixture-parked";

export const LONG_DISPLAY_NAME = "缩放验收用超长显示名不会把操作列挤出窗口";

export const ACCEPT_WINDOWS = [
  { name: "1080×760", width: 1080, height: 760 },
  { name: "1280×880", width: 1280, height: 880 },
] as const;

/** 1 是逻辑窗口；1.25/1.5/2 由页面 zoom 实测，不是系统 DPI。 */
export const ACCEPT_ZOOMS = [1, 1.25, 1.5, 2] as const;

export function parkedRow(partial: Partial<AccountRow> = {}): AccountRow {
  return {
    id: FIXTURE_ACCOUNT_ID,
    label: FIXTURE_ACCOUNT_ID,
    active: false,
    membership: "Allegro",
    membership_expiry: null,
    membership_remaining_days: null,
    membership_period_days: null,
    membership_expiry_hint: false,
    quotas: [],
    error: null,
    credential_expired: false,
    cached_at: null,
    display_name: LONG_DISPLAY_NAME,
    official_nickname_known: false,
    auto_swap_opt_out: false,
    ...partial,
  };
}

export function showLibrary(html: string): string {
  const overview = 'class="page active" id="page-overview"';
  const library = 'class="page" id="page-library"';
  if (!html.includes(overview) || !html.includes(library)) {
    throw new Error("路由 class 与 index.html 不一致");
  }
  return html
    .replace(overview, 'class="page" id="page-overview"')
    .replace(library, 'class="page active" id="page-library"');
}

export function insertAccountCard(html: string, cardHtml: string): string {
  const slot = '<div id="accounts-list" class="profiles-grid"></div>';
  if (!html.includes(slot)) {
    throw new Error("账号列表槽位不见了");
  }
  return html.replace(slot, `<div id="accounts-list" class="profiles-grid">${cardHtml}</div>`);
}

export function expandBehaviorPanel(html: string): string {
  const from = '<div id="behavior-panel-body" class="hidden">';
  if (!html.includes(from)) {
    throw new Error("行为面板折叠标记不见了");
  }
  return html.replace(from, '<div id="behavior-panel-body">');
}

export function showEmptyLibrary(html: string): string {
  const from = '<div id="accounts-empty" class="empty-state library-empty hidden">';
  if (!html.includes(from)) {
    throw new Error("空态标记不见了");
  }
  return html.replace(from, '<div id="accounts-empty" class="empty-state library-empty">');
}

export function setButtonDisabled(html: string, id: string, disabled: boolean): string {
  const start = html.indexOf(`<button id="${id}"`);
  if (start < 0) {
    throw new Error(`找不到按钮 ${id}`);
  }
  const end = html.indexOf(">", start);
  let tag = html.slice(start, end);
  const has = /\sdisabled\b/.test(tag);
  if (disabled && !has) {
    tag += " disabled";
  }
  if (!disabled && has) {
    tag = tag.replace(/\sdisabled\b/, "");
  }
  return html.slice(0, start) + tag + html.slice(end);
}

export function openOverflowMenu(html: string): string {
  const from = '<div class="profile-overflow-menu" hidden>';
  if (!html.includes(from)) {
    throw new Error("更多操作菜单未处于收起");
  }
  return html.replace(from, '<div class="profile-overflow-menu">');
}

export function openModalDialog(html: string, id: string): string {
  const from = `<dialog id="${id}"`;
  if (!html.includes(from)) {
    throw new Error(`找不到对话框 ${id}`);
  }
  return html.replace(from, `<dialog id="${id}" open`);
}

export interface Focusable {
  tag: string;
  id: string;
  type: string;
  attrs: Record<string, string>;
}

export function focusKey(el: Focusable): string {
  if (el.id) {
    return el.id;
  }
  for (const name of [
    "data-overflow",
    "data-rename",
    "data-swap",
    "data-reauth",
    "data-delete",
    "data-bind",
    "data-optout",
  ]) {
    const value = el.attrs[name];
    if (value) {
      return `${name}:${value}`;
    }
  }
  if (el.attrs.href) {
    return `href:${el.attrs.href}`;
  }
  if (el.attrs["aria-label"]) {
    return `label:${el.attrs["aria-label"]}`;
  }
  return el.tag;
}

interface TabFrame {
  hidesChildren: boolean;
  openDialog: boolean;
}

const VOID_TAGS = new Set([
  "area",
  "base",
  "br",
  "col",
  "embed",
  "hr",
  "img",
  "input",
  "link",
  "meta",
  "source",
  "track",
  "wbr",
]);

function parseAttrs(raw: string): Record<string, string> {
  const attrs: Record<string, string> = {};
  const attrRe = /([:@\w-]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+)))?/g;
  let match: RegExpExecArray | null;
  while ((match = attrRe.exec(raw)) !== null) {
    attrs[match[1]] = match[2] ?? match[3] ?? match[4] ?? "";
  }
  return attrs;
}

function selfHides(tag: string, classes: string[], attrs: Record<string, string>): boolean {
  if (classes.includes("hidden") || Object.hasOwn(attrs, "hidden")) {
    return true;
  }
  if (tag === "dialog" && !Object.hasOwn(attrs, "open")) {
    return true;
  }
  return tag === "section" && classes.includes("page") && !classes.includes("active");
}

function isFocusable(tag: string, attrs: Record<string, string>): boolean {
  if (attrs.tabindex === "-1" || Object.hasOwn(attrs, "disabled")) {
    return false;
  }
  if (tag === "a") {
    return Object.hasOwn(attrs, "href");
  }
  if (tag === "button" || tag === "textarea" || tag === "select") {
    return true;
  }
  return tag === "input" && attrs.type !== "hidden";
}

/** 文档顺序即 Tab 序。`modal` 模拟 showModal：只留下打开的对话框内部。 */
export function tabOrder(html: string, options?: { modal?: boolean }): Focusable[] {
  const source = html.replace(/<!--[\s\S]*?-->/g, "");
  const order: Focusable[] = [];
  const stack: TabFrame[] = [];
  const tagRe = /<[^>]+>/g;
  let match: RegExpExecArray | null;
  while ((match = tagRe.exec(source)) !== null) {
    const raw = match[0];
    if (raw.startsWith("<!") || raw.startsWith("<?")) {
      continue;
    }
    const name = raw.match(/^<\/?\s*([^\s/>]+)/)?.[1]?.toLowerCase();
    if (!name) {
      continue;
    }
    if (raw.startsWith("</")) {
      if (stack.length === 0) {
        throw new Error(`多余的闭合标签 ${name}`);
      }
      stack.pop();
      continue;
    }
    const attrSource = raw
      .replace(/^<\s*[^\s/>]+/, "")
      .replace(/\/?>$/, "");
    const attrs = parseAttrs(attrSource);
    const classes = (attrs.class ?? "").split(/\s+/).filter(Boolean);
    const parentHides = stack.at(-1)?.hidesChildren ?? false;
    const hidden = parentHides || selfHides(name, classes, attrs);
    const insideOpenDialog = stack.some((frame) => frame.openDialog);
    if (isFocusable(name, attrs) && !hidden && (!options?.modal || insideOpenDialog)) {
      order.push({
        tag: name,
        id: attrs.id ?? "",
        type: attrs.type ?? (name === "button" ? "submit" : ""),
        attrs,
      });
    }
    const selfClosing = raw.endsWith("/>") || VOID_TAGS.has(name);
    if (!selfClosing) {
      stack.push({
        hidesChildren: hidden,
        openDialog: name === "dialog" && Object.hasOwn(attrs, "open"),
      });
    }
  }
  if (stack.length !== 0) {
    throw new Error(`HTML 标签未闭合，剩余 ${stack.length}`);
  }
  return order;
}

export function createTabSession(html: string, options?: { modal?: boolean }) {
  const order = tabOrder(html, options);
  let index = -1;
  return {
    keys(): string[] {
      return order.map(focusKey);
    },
    current(): Focusable | null {
      return index >= 0 ? order[index] : null;
    },
    tab(shift = false): string {
      index += shift ? -1 : 1;
      if (index < 0 || index >= order.length) {
        throw new Error(`Tab 越界 ${index} / ${order.length}`);
      }
      return focusKey(order[index]);
    },
    jumpTo(key: string): Focusable {
      const found = order.findIndex((el) => focusKey(el) === key);
      if (found < 0) {
        throw new Error(`焦点目标不在 Tab 序：${key}`);
      }
      index = found;
      return order[index];
    },
  };
}

export function clickListener(source: string, id: string): string {
  const marker = `"${id}"`;
  let from = 0;
  while (from < source.length) {
    const start = source.indexOf(marker, from);
    if (start < 0) {
      break;
    }
    const nearby = source.slice(start, start + 240);
    const listenAt = nearby.indexOf("addEventListener(");
    const clickAt = nearby.indexOf('"click"');
    if (listenAt >= 0 && clickAt > listenAt && clickAt - listenAt < 40) {
      return braceBlock(source, source.indexOf("{", start + listenAt));
    }
    from = start + marker.length;
  }
  throw new Error(`没有 ${id} 的 click 监听`);
}

export function functionBlock(source: string, name: string): string {
  const marker = `function ${name}`;
  const start = source.indexOf(marker);
  if (start < 0) {
    throw new Error(`没有函数 ${name}`);
  }
  return braceBlock(source, source.indexOf("{", start));
}

function braceBlock(source: string, open: number): string {
  if (open < 0) {
    throw new Error("代码块没有起始括号");
  }
  let depth = 0;
  for (let i = open; i < source.length; i += 1) {
    const ch = source[i];
    if (ch === "{") {
      depth += 1;
    } else if (ch === "}") {
      depth -= 1;
      if (depth === 0) {
        return source.slice(open, i + 1);
      }
    }
  }
  throw new Error("代码块没有闭合");
}

/** 供「更多操作」焦点归还使用的最小元素。 */
export class OverflowEl {
  hidden = false;
  parent: OverflowEl | null = null;
  children: OverflowEl[] = [];
  focusCount = 0;
  private attrs = new Map<string, string>();

  constructor(initial: Record<string, string> = {}) {
    for (const [key, value] of Object.entries(initial)) {
      if (key === "hidden") {
        this.hidden = true;
      } else {
        this.attrs.set(key, value);
      }
    }
  }

  getAttribute(name: string): string | null {
    return this.attrs.get(name) ?? null;
  }

  setAttribute(name: string, value: string): void {
    this.attrs.set(name, value);
  }

  get parentElement(): OverflowEl | null {
    return this.parent;
  }

  querySelector(selector: string): OverflowEl | null {
    if (selector !== ".profile-overflow-menu") {
      return null;
    }
    return this.children.find((child) => child.getAttribute("class") === "profile-overflow-menu") ?? null;
  }

  contains(node: OverflowEl | null): boolean {
    let current = node;
    while (current) {
      if (current === this) {
        return true;
      }
      current = current.parent;
    }
    return false;
  }

  focus(): void {
    this.focusCount += 1;
  }
}

export function parkedOverflow(accountId: string): {
  toggle: OverflowEl;
  menu: OverflowEl;
  rename: OverflowEl;
} {
  const menu = new OverflowEl({ class: "profile-overflow-menu", hidden: "" });
  const rename = new OverflowEl({
    class: "profile-overflow-item",
    type: "button",
    "data-rename": accountId,
    "aria-label": "重命名",
  });
  rename.parent = menu;
  menu.children.push(rename);
  const toggle = new OverflowEl({
    class: "profile-overflow-toggle",
    type: "button",
    "data-overflow": accountId,
    "aria-expanded": "false",
    "aria-label": "更多操作",
  });
  const wrap = new OverflowEl({ class: "profile-overflow" });
  toggle.parent = wrap;
  menu.parent = wrap;
  wrap.children.push(toggle, menu);
  return { toggle, menu, rename };
}
