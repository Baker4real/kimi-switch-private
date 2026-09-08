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

/** CSS 变量可能是纯色、rgba，或 linear-gradient 的若干 stop。 */
function tokenLayers(value: string): string[] {
  if (value.startsWith("linear-gradient")) {
    const layers = [...value.matchAll(/rgba?\([^)]+\)/gi)].map((match) => match[0]);
    if (layers.length === 0) {
      throw new Error(`无法解析渐变: ${value}`);
    }
    return layers;
  }
  return [value];
}

function opaqueSurfaces(vars: ThemeVars, token: string): { bg: Rgba; label: string }[] {
  const raw = vars[token];
  if (!raw) {
    throw new Error(`缺少变量 --${token}`);
  }
  const canvas = parseColor(vars["canvas-bg"]);
  const layers = tokenLayers(raw);
  return layers.map((layer, index) => {
    const bgToken = parseColor(layer);
    const bg = bgToken.a < 1 ? composite(bgToken, canvas) : bgToken;
    return {
      bg,
      label: layers.length > 1 ? `${token}[${index}]` : token,
    };
  });
}

/** 纸色是唯一主题（issue #119）：token 只在 `:root` 一处。 */
const paper = blockVars(":root {");

describe("纸色对比度守卫", () => {
  it("不再有深色主题选择器", () => {
    expect(css).not.toMatch(/data-theme/);
  });

  it("关键前景/背景对均 ≥ 4.5:1", () => {
    const pairs: { fg: string; bg: string; label: string }[] = [
      { fg: "text-inverse", bg: "primary-background", label: "主按钮白字" },
      { fg: "text-inverse", bg: "surface-pressed", label: "激活 Tab 白字" },
      { fg: "text-body", bg: "canvas-bg", label: "body 文字" },
      { fg: "text-muted", bg: "canvas-bg", label: "muted 文字" },
    ];
    const report: string[] = [];
    for (const pair of pairs) {
      const canvas = parseColor(paper["canvas-bg"]);
      const fg = parseColor(paper[pair.fg]);
      const bgToken = parseColor(paper[pair.bg]);
      const bg = bgToken.a < 1 ? composite(bgToken, canvas) : bgToken;
      const ratio = contrastRatio(fg, bg);
      report.push(`${pair.label}: ${ratio.toFixed(2)}:1`);
      expect(ratio, `${pair.label} 应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`).toBeGreaterThanOrEqual(
        4.5,
      );
    }
    expect(report).toHaveLength(pairs.length);
  });
});

const mutedSurfaceTokens = [
  "canvas-bg",
  "field-background",
  "panel-background",
  "profile-background",
] as const;

describe("muted 在实际面板表面", () => {
  it("muted 对 canvas / 表单底 / 面板 / 账号卡叠底均 ≥ 4.5:1", () => {
    const report: string[] = [];
    const muted = parseColor(paper["text-muted"]);
    for (const token of mutedSurfaceTokens) {
      for (const surface of opaqueSurfaces(paper, token)) {
        const ratio = contrastRatio(muted, surface.bg);
        report.push(`muted / ${surface.label}: ${ratio.toFixed(2)}:1`);
        expect(
          ratio,
          `muted / ${surface.label} 应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`,
        ).toBeGreaterThanOrEqual(4.5);
      }
    }
    expect(report.length).toBeGreaterThan(0);
  });
});

describe("焦点环对比度", () => {
  it("focus-ring 对 canvas/面板表面均 ≥ 3:1", () => {
    const report: string[] = [];
    const surfaces = ["canvas-bg", "field-background"] as const;
    for (const token of surfaces) {
      const canvas = parseColor(paper["canvas-bg"]);
      const ring = parseColor(paper["focus-ring"]);
      const bgToken = parseColor(paper[token]);
      const bg = bgToken.a < 1 ? composite(bgToken, canvas) : bgToken;
      const ratio = contrastRatio(ring, bg);
      report.push(`焦点环 / ${token}: ${ratio.toFixed(2)}:1`);
      expect(ratio, `焦点环 / ${token} 应为 ≥ 3:1，实际 ${ratio.toFixed(2)}:1`).toBeGreaterThanOrEqual(
        3,
      );
    }
    expect(report).toHaveLength(surfaces.length);
  });
});
