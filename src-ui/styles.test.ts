import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "styles.css"), "utf8").replace(
  /\r\n/g,
  "\n",
);

function ruleBody(selector: string): string {
  const idx = css.indexOf(`${selector} {`);
  if (idx < 0) {
    throw new Error(`缺少规则: ${selector}`);
  }
  const start = css.indexOf("{", idx);
  const end = css.indexOf("}", start);
  return css.slice(start + 1, end);
}

describe("中文标题排印", () => {
  it("展示字体链在 Cormorant 之后回退 Noto Sans SC", () => {
    expect(css).toMatch(
      /--font-display:\s*"Cormorant Garamond Custom",\s*"Noto Sans SC Custom"/,
    );
  });

  it("标题行高 ≥ 1.15 且字距不小于 0，并合并近似标题规则", () => {
    const title = ruleBody(".section-title");
    expect(title).toMatch(/line-height:\s*1\.15\s*;/);
    expect(title).toMatch(/letter-spacing:\s*0\s*;/);
    expect(css).not.toMatch(/\.section-title-rail\s*\{/);
    const dialog = ruleBody(".app-dialog h3");
    expect(dialog).toMatch(/line-height:\s*1\.15\s*;/);
    expect(dialog).toMatch(/letter-spacing:\s*0\s*;/);
  });
});

describe("额度条字号与样式收尾", () => {
  it("条上用量文字 ≥ 12px，标签与重置时间为 0.75rem", () => {
    const value = ruleBody(".quota-value");
    expect(value).toMatch(/font-size:\s*12px\s*;/);
    const label = ruleBody(".quota-label,\n.quota-reset");
    expect(label).toMatch(/font-size:\s*0\.75rem\s*;/);
  });

  it("减弱动效时停 pulse 与 transform", () => {
    expect(css).toMatch(/prefers-reduced-motion:\s*reduce/);
    const start = css.indexOf("@media (prefers-reduced-motion: reduce)");
    const body = css.slice(start, start + 900);
    expect(body).toMatch(/animation:\s*none/);
    expect(body).toMatch(/transform:\s*none/);
  });

  it("最大化使用双方块还原字形，并删除死 token", () => {
    expect(css).toContain(
      ".window-control-button.is-maximized .window-control-icon--maximize::after",
    );
    expect(css).not.toMatch(/--radius-shell:/);
  });
});

describe("全局忙碌指示布局", () => {
  it("工具条三列：导航居中、忙碌指示靠右，各页都能看见", () => {
    const toolbar = ruleBody(".app-toolbar");
    expect(toolbar).toMatch(/display:\s*grid\s*;/);
    expect(toolbar).toMatch(/grid-template-columns:\s*1fr\s+auto\s+1fr\s*;/);
    const pill = ruleBody(".app-toolbar .quota-monitor-pill");
    expect(pill).toMatch(/grid-column:\s*3\s*;/);
    expect(pill).toMatch(/justify-self:\s*end\s*;/);
  });
});

describe("主题化焦点环", () => {
  it("textarea 与 button/input 共用同一 :focus-visible 规则", () => {
    expect(css).toMatch(
      /button:focus-visible,\s*input:focus-visible,\s*textarea:focus-visible,\s*a:focus-visible \{/
    );
    const start = css.indexOf("button:focus-visible");
    const body = css.slice(start, start + 220);
    expect(body).toContain("outline: 2px solid var(--focus-ring)");
    expect(body).toContain("outline-offset: 2px");
  });
});

describe("账号卡片日期列", () => {
  it("为不同数量的操作按钮保留统一的右侧列宽", () => {
    const card = ruleBody(".profile-card");
    expect(card).toMatch(/--profile-actions-column:\s*264px\s*;/);
    expect(card).toMatch(
      /grid-template-columns:\s*minmax\(0,\s*1fr\)\s+var\(--profile-actions-column\)\s*;/,
    );
  });
});
