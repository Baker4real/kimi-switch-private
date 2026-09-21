import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

const dir = dirname(fileURLToPath(import.meta.url));

const html = readFileSync(join(dir, "index.html"), "utf8").replace(/\r\n/g, "\n");
const css = readFileSync(join(dir, "styles.css"), "utf8").replace(/\r\n/g, "\n");
const main = readFileSync(join(dir, "main.ts"), "utf8");

/** 账号库空态整块（#accounts-empty 容器内部只含 p / button，无嵌套 div）。 */
function emptyBlock(): string {
  const start = html.indexOf('id="accounts-empty"');
  expect(start).toBeGreaterThanOrEqual(0);
  const open = html.lastIndexOf("<div", start);
  const end = html.indexOf("</div>", start) + "</div>".length;
  return html.slice(open, end);
}

describe("账号库空态入口（issue #165）", () => {
  it("空态自带可直接执行的「添加账号」主按钮", () => {
    const block = emptyBlock();
    expect(block).toMatch(
      /id="library-empty-add-button"[^>]*class="[^"]*action-button action-button-primary/,
    );
    expect(block).toContain(">添加账号</button>");
  });

  it("空态说明总览导入路径及其适用场景（已在 Kimi Code 登录）", () => {
    const block = emptyBlock();
    expect(block).toContain("已在 Kimi Code 登录");
    expect(block).toContain("总览");
    expect(block).toContain("导入当前账号");
    // 先指路（总览）再给动作（导入），沿用 issue #127 的顺序约定。
    expect(block.indexOf("总览")).toBeLessThan(block.indexOf("导入"));
  });

  it("两路径差异一句话说清：添加走浏览器授权，导入无需重新授权", () => {
    const block = emptyBlock();
    expect(block).toContain("浏览器");
    expect(block).toContain("重新授权");
  });

  it("空态按钮触发既有添加流程：与页眉「添加账号」同一 startDeviceAuth(null, \"\")", () => {
    for (const id of ["library-add-button", "library-empty-add-button"]) {
      const idx = main.indexOf(`required<HTMLButtonElement>("${id}")`);
      expect(idx, `${id} 应有接线`).toBeGreaterThanOrEqual(0);
      const wiring = main.slice(idx, idx + 200);
      expect(wiring, `${id} 应触发既有设备码添加流`).toContain('startDeviceAuth(null, "")');
    }
  });

  it("空态按钮与页眉添加一致进入忙碌禁用清单", () => {
    const listStart = main.indexOf("const BUSY_BUTTON_IDS = [");
    expect(listStart).toBeGreaterThanOrEqual(0);
    const list = main.slice(listStart, main.indexOf("] as const;", listStart));
    expect(list).toContain('"library-add-button"');
    expect(list).toContain('"library-empty-add-button"');
  });

  it("账号非空时不出现空态：容器默认隐藏，paintAccountCards 仍按账号数收起", () => {
    const tagStart = html.lastIndexOf("<div", html.indexOf('id="accounts-empty"'));
    const tag = html.slice(tagStart, html.indexOf(">", tagStart) + 1);
    expect(tag).toMatch(/class="[^"]*\bhidden\b/);
    expect(main).toContain('empty.classList.toggle("hidden", next.accounts.length > 0)');
  });

  it("空态沿用既有卡片与按钮样式，布局只做竖排收拢", () => {
    const block = emptyBlock();
    expect(block).toMatch(/class="[^"]*\bempty-state\b/);
    expect(block).not.toMatch(/lucide|heroicons/i);
    expect(css).toMatch(/\.library-empty\s*\{/);
  });
});
