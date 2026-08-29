import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

type Rgba = { r: number; g: number; b: number; a: number };
type ThemeVars = Record<string, string>;

const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "styles.css"), "utf8");

function blockVars(marker: string): ThemeVars {
  const idx = css.indexOf(marker);
  if (idx < 0) {
    throw new Error(`缺少选择器: ${marker}`);
  }
  const start = css.indexOf("{", idx);
  let depth = 0;
  let end = start;
  for (let i = start; i < css.length; i += 1) {
    const ch = css[i];
    if (ch === "{") {
      depth += 1;
    } else if (ch === "}") {
      depth -= 1;
      if (depth === 0) {
        end = i;
        break;
      }
    }
  }
  const body = css.slice(start + 1, end);
  const vars: ThemeVars = {};
  const re = /--([a-z0-9-]+)\s*:\s*([^;]+);/gi;
  for (const match of body.matchAll(re)) {
    vars[match[1]] = match[2].trim().replace(/\s+/g, " ");
  }
  return vars;
}

function parseColor(value: string): Rgba {
  const hex = /^#([0-9a-f]{6})$/i.exec(value);
  if (hex) {
    const n = Number.parseInt(hex[1], 16);
    return { r: (n >> 16) & 255, g: (n >> 8) & 255, b: n & 255, a: 1 };
  }
  const rgba =
    /^rgba\(\s*([\d.]+)\s*,\s*([\d.]+)\s*,\s*([\d.]+)\s*,\s*([\d.]+)\s*\)$/i.exec(value);
  if (rgba) {
    return {
      r: Number(rgba[1]),
      g: Number(rgba[2]),
      b: Number(rgba[3]),
      a: Number(rgba[4]),
    };
  }
  const rgb = /^rgb\(\s*([\d.]+)\s*,\s*([\d.]+)\s*,\s*([\d.]+)\s*\)$/i.exec(value);
  if (rgb) {
    return { r: Number(rgb[1]), g: Number(rgb[2]), b: Number(rgb[3]), a: 1 };
  }
  throw new Error(`不支持的颜色值: ${value}`);
}

function composite(fg: Rgba, bg: Rgba): Rgba {
  return {
    r: Math.round(fg.r * fg.a + bg.r * (1 - fg.a)),
    g: Math.round(fg.g * fg.a + bg.g * (1 - fg.a)),
    b: Math.round(fg.b * fg.a + bg.b * (1 - fg.a)),
    a: 1,
  };
}

function linear(channel: number): number {
  const c = channel / 255;
  return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
}

function relativeLuminance(color: Rgba): number {
  return 0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b);
}

function contrastRatio(fg: Rgba, bg: Rgba): number {
  const painted = fg.a < 1 ? composite(fg, bg) : fg;
  const l1 = relativeLuminance(painted);
  const l2 = relativeLuminance(bg);
  const [hi, lo] = l1 > l2 ? [l1, l2] : [l2, l1];
  return (hi + 0.05) / (lo + 0.05);
}

const classic = blockVars(":root {");
const jade = blockVars(':root[data-theme="jade-night"] {');

const pairs: { theme: string; vars: ThemeVars; fg: string; bg: string; label: string }[] = [
  {
    theme: "classic",
    vars: classic,
    fg: "text-inverse",
    bg: "primary-background",
    label: "主按钮白字",
  },
  {
    theme: "classic",
    vars: classic,
    fg: "text-inverse",
    bg: "surface-pressed",
    label: "激活 Tab / 主题钮白字",
  },
  { theme: "classic", vars: classic, fg: "text-body", bg: "canvas-bg", label: "body 文字" },
  { theme: "classic", vars: classic, fg: "text-muted", bg: "canvas-bg", label: "muted 文字" },
  {
    theme: "jade-night",
    vars: jade,
    fg: "text-inverse",
    bg: "primary-background",
    label: "主按钮深字",
  },
  {
    theme: "jade-night",
    vars: jade,
    fg: "text-inverse",
    bg: "surface-pressed",
    label: "激活 Tab / 主题钮深字",
  },
  { theme: "jade-night", vars: jade, fg: "text-body", bg: "canvas-bg", label: "body 文字" },
  { theme: "jade-night", vars: jade, fg: "text-muted", bg: "canvas-bg", label: "muted 文字" },
];

describe("主题对比度守卫", () => {
  it("两主题关键前景/背景对均 ≥ 4.5:1", () => {
    const report: string[] = [];
    for (const pair of pairs) {
      const canvas = parseColor(pair.vars["canvas-bg"]);
      const fg = parseColor(pair.vars[pair.fg]);
      const bgToken = parseColor(pair.vars[pair.bg]);
      const bg = bgToken.a < 1 ? composite(bgToken, canvas) : bgToken;
      const ratio = contrastRatio(fg, bg);
      report.push(`${pair.theme} ${pair.label}: ${ratio.toFixed(2)}:1`);
      expect(ratio, `${pair.theme} ${pair.label} 应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`).toBeGreaterThanOrEqual(
        4.5,
      );
    }
    expect(report).toHaveLength(pairs.length);
  });

  it("深色主色不回退到低于当前实测", () => {
    const canvas = parseColor(jade["canvas-bg"]);
    const fg = parseColor(jade["text-inverse"]);
    const primary = composite(parseColor(jade["primary-background"]), canvas);
    expect(contrastRatio(fg, primary)).toBeGreaterThanOrEqual(5.0);
    const muted = parseColor(jade["text-muted"]);
    expect(contrastRatio(muted, canvas)).toBeGreaterThanOrEqual(5.0);
  });
});
