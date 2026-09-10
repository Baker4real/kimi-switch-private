import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  applyMaximizePresentation,
  maximizeButtonLabel,
} from "./window-controls";

const dir = dirname(fileURLToPath(import.meta.url));

const html = readFileSync(join(dir, "index.html"), "utf8").replace(/\r\n/g, "\n");
const css = readFileSync(join(dir, "styles.css"), "utf8").replace(/\r\n/g, "\n");
const windowControlsSrc = readFileSync(join(dir, "window-controls.ts"), "utf8");

function ruleBody(selector: string): string {
  const idx = css.indexOf(`${selector} {`);
  if (idx < 0) {
    throw new Error(`缺少规则: ${selector}`);
  }
  const start = css.indexOf("{", idx);
  const end = css.indexOf("}", start);
  return css.slice(start + 1, end);
}

function declaredMinHeightPx(selector: string): number {
  const body = ruleBody(selector);
  const px = /min-height:\s*(\d+)px/.exec(body);
  if (px) {
    return Number(px[1]);
  }
  const unitless = /min-height:\s*(\d+(?:\.\d+)?)\s*;/.exec(body);
  if (unitless) {
    return Number(unitless[1]);
  }
  throw new Error(`缺少 min-height: ${selector}`);
}

function tagWithId(fragment: string, id: string): string {
  const idx = fragment.indexOf(`id="${id}"`);
  expect(idx).toBeGreaterThanOrEqual(0);
  const start = fragment.lastIndexOf("<", idx);
  const end = fragment.indexOf(">", idx);
  return fragment.slice(start, end + 1);
}

function attr(tag: string, name: string): string | null {
  const match = new RegExp(`${name}="([^"]*)"`).exec(tag);
  return match?.[1] ?? null;
}

function dialogAround(dialogId: string): string {
  const start = html.indexOf(`id="${dialogId}"`);
  expect(start).toBeGreaterThanOrEqual(0);
  const open = html.lastIndexOf("<dialog", start);
  const end = html.indexOf("</dialog>", start);
  return html.slice(open, end + "</dialog>".length);
}

describe("小按钮可点尺寸（issue #85）", () => {
  it("绑定/重新绑定按钮 min-height 至少 24px，不再被压成约 20px", () => {
    expect(declaredMinHeightPx(".plan-bind")).toBeGreaterThanOrEqual(24);
  });
});

describe("设置开关可点尺寸（issue #126）", () => {
  it("开关轨道 min-height 至少 24px，可视尺寸 44×24", () => {
    expect(declaredMinHeightPx(".switch")).toBeGreaterThanOrEqual(24);
    const body = ruleBody(".switch");
    expect(body).toMatch(/width:\s*44px/);
    expect(body).toMatch(/height:\s*24px/);
  });
});

describe("窗口控制语义（issue #85）", () => {
  it("窗口控制容器用语义化 role 承载分组名，不只把 aria-label 挂在无语义 div 上", () => {
    const tag = tagWithId(html, "window-minimize-button");
    const open = html.lastIndexOf("<div", html.indexOf('class="window-controls"'));
    const controlsTag = html.slice(open, html.indexOf(">", open) + 1);
    expect(controlsTag).toContain('class="window-controls"');
    expect(attr(controlsTag, "role")).toBe("group");
    expect(attr(controlsTag, "aria-label")).toBe("窗口控制");
    expect(tag).toContain("window-minimize-button");
  });

  it("最大化按钮名称随状态在「最大化」与「还原」间切换", () => {
    expect(maximizeButtonLabel(false)).toBe("最大化");
    expect(maximizeButtonLabel(true)).toBe("还原");
  });

  it("同步最大化态时同时改 class 与 aria-label", () => {
    const classes = new Set<string>();
    const attrs: Record<string, string> = { "aria-label": "最大化" };
    const button = {
      classList: {
        toggle(token: string, force?: boolean) {
          const on = force ?? !classes.has(token);
          if (on) {
            classes.add(token);
          } else {
            classes.delete(token);
          }
        },
        remove(token: string) {
          classes.delete(token);
        },
      },
      setAttribute(name: string, value: string) {
        attrs[name] = value;
      },
    };
    applyMaximizePresentation(button, true);
    expect(classes.has("is-maximized")).toBe(true);
    expect(attrs["aria-label"]).toBe("还原");
    applyMaximizePresentation(button, false);
    expect(classes.has("is-maximized")).toBe(false);
    expect(attrs["aria-label"]).toBe("最大化");
  });

  it("setupWindowControls 经 applyMaximizePresentation 同步读屏名称", () => {
    expect(windowControlsSrc).toContain("applyMaximizePresentation(");
    expect(windowControlsSrc).not.toMatch(
      /maximizeButton\.classList\.toggle\("is-maximized"/,
    );
  });

  it("标题栏 K 徽标是纯装饰内联 SVG，读屏只播报 h1 标题", () => {
    const mark = html.slice(
      html.indexOf('<svg class="window-app-mark"'),
      html.indexOf("</svg>"),
    );
    expect(mark).toContain("<svg");
    expect(mark).toContain('aria-hidden="true"');
    expect(mark).toContain('focusable="false"');
    // 徽标内不重新出现「Kimi 换号器」文本，避免读屏重复播报。
    expect(mark).not.toContain("Kimi 换号器");
  });

  it("拖动 / 双击最大化用 Element 判定，SVG 子元素上也能触发", () => {
    expect(windowControlsSrc).toContain("target instanceof Element");
    expect(windowControlsSrc).not.toContain("target instanceof HTMLElement");
    expect(windowControlsSrc).toContain("event.detail === 2");
    expect(windowControlsSrc).toContain("appWindow.startDragging()");
  });
});

describe("对话框标题关联（issue #85）", () => {
  it("每个对话框 aria-labelledby 指向内部标题", () => {
    const dialogs: [string, string, string][] = [
      ["auth-dialog", "auth-dialog-title", "添加账号 · 浏览器授权"],
      ["rename-dialog", "rename-dialog-title", "重命名账号"],
      ["bind-dialog", "bind-dialog-title", "绑定网页登录"],
      ["delete-dialog", "delete-dialog-title", "确认删除"],
    ];
    for (const [dialogId, titleId, title] of dialogs) {
      const dialog = dialogAround(dialogId);
      const openTag = dialog.slice(0, dialog.indexOf(">") + 1);
      expect(attr(openTag, "aria-labelledby"), dialogId).toBe(titleId);
      expect(dialog).toMatch(new RegExp(`<h3[^>]*id="${titleId}"`));
      expect(dialog).toContain(title);
    }
  });
});
