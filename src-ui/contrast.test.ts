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

/** CSS 变量可能是纯色、rgba，或 linear-gradient 的若干 stop。
 * 渐变 stop 既可能是 rgba() 也可能是十六进制（如 #286eea），两者都按出现顺序取出。 */
function tokenLayers(value: string): string[] {
  if (value.startsWith("linear-gradient")) {
    const layers = [
      ...value.matchAll(/(rgba?\([^)]+\)|#[0-9a-fA-F]{3,8})/g),
    ].map((match) => match[0]);
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

/** 浅蓝白卡片是唯一主题（issue #119 / #131）：token 只在 `:root` 一处。 */
const tokens = blockVars(":root {");

describe("浅蓝白主题对比度守卫", () => {
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
      const canvas = parseColor(tokens["canvas-bg"]);
      const fg = parseColor(tokens[pair.fg]);
      const bgToken = parseColor(tokens[pair.bg]);
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
    const muted = parseColor(tokens["text-muted"]);
    for (const token of mutedSurfaceTokens) {
      for (const surface of opaqueSurfaces(tokens, token)) {
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

describe("danger / body 在实际表面", () => {
  it("danger-text 对 panel / empty / button / toast 均 ≥ 4.5:1", () => {
    const report: string[] = [];
    const danger = parseColor(tokens["danger-text"]);
    const surfaces = [
      "panel-background",
      "empty-background",
      "button-background",
      "toast-background",
    ] as const;
    for (const token of surfaces) {
      for (const surface of opaqueSurfaces(tokens, token)) {
        const ratio = contrastRatio(danger, surface.bg);
        report.push(`danger-text / ${surface.label}: ${ratio.toFixed(2)}:1`);
        expect(
          ratio,
          `danger-text / ${surface.label} 应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`,
        ).toBeGreaterThanOrEqual(4.5);
      }
    }
    expect(report.length).toBeGreaterThan(0);
  });

  it("text-body 对 field / profile 均 ≥ 4.5:1", () => {
    const report: string[] = [];
    const body = parseColor(tokens["text-body"]);
    const surfaces = ["field-background", "profile-background"] as const;
    for (const token of surfaces) {
      for (const surface of opaqueSurfaces(tokens, token)) {
        const ratio = contrastRatio(body, surface.bg);
        report.push(`text-body / ${surface.label}: ${ratio.toFixed(2)}:1`);
        expect(
          ratio,
          `text-body / ${surface.label} 应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`,
        ).toBeGreaterThanOrEqual(4.5);
      }
    }
    expect(report.length).toBeGreaterThan(0);
  });
});

describe("非文本边界对比度（issue #126）", () => {
  it("输入框边框对 field-background ≥ 3:1", () => {
    const canvas = parseColor(tokens["canvas-bg"]);
    const [field] = opaqueSurfaces(tokens, "field-background");
    const border = parseColor(tokens["field-border"]);
    const painted = border.a < 1 ? composite(border, field.bg) : border;
    const ratio = contrastRatio(painted, field.bg);
    expect(
      ratio,
      `field-border / field-background 应为 ≥ 3:1，实际 ${ratio.toFixed(2)}:1`,
    ).toBeGreaterThanOrEqual(3);
    expect(canvas.a).toBe(1);
  });

  it("未选中开关边框对轨道与纸面 ≥ 3:1", () => {
    const canvas = parseColor(tokens["canvas-bg"]);
    const track = parseColor(tokens["track"]);
    const border = parseColor(tokens["switch-border"]);
    const pairs: { bg: Rgba; label: string }[] = [
      { bg: track, label: "track" },
      { bg: canvas, label: "canvas-bg" },
    ];
    for (const pair of pairs) {
      const ratio = contrastRatio(border, pair.bg);
      expect(
        ratio,
        `switch-border / ${pair.label} 应为 ≥ 3:1，实际 ${ratio.toFixed(2)}:1`,
      ).toBeGreaterThanOrEqual(3);
    }
  });
});

describe("状态色非文本对比度（issue #128）", () => {
  const expectedTones = {
    "tone-ok": "#128a3e",
    "tone-warn": "#a16207",
    "tone-high": "#c2410c",
    "tone-full": "#dc2626",
  } as const;

  it("四档 tone 为指定纯色且对轨道 ≥ 3:1", () => {
    const track = parseColor(tokens.track);
    const report: string[] = [];
    for (const [token, hex] of Object.entries(expectedTones)) {
      const raw = tokens[token];
      expect(raw, `--${token} 须为指定纯色`).toBe(hex);
      expect(raw, `--${token} 不得使用渐变`).not.toMatch(/linear-gradient/i);
      const ratio = contrastRatio(parseColor(raw), track);
      report.push(`${token} / track: ${ratio.toFixed(2)}:1`);
      expect(
        ratio,
        `${token} / track 应为 ≥ 3:1，实际 ${ratio.toFixed(2)}:1`,
      ).toBeGreaterThanOrEqual(3);
    }
    expect(report).toHaveLength(4);
  });

  it("body / muted 为指定层次且对 canvas ≥ 4.5:1", () => {
    expect(tokens["text-body"]).toBe("#5f6884");
    expect(tokens["text-muted"]).toBe("#666e8c");
    const canvas = parseColor(tokens["canvas-bg"]);
    const bodyRatio = contrastRatio(parseColor(tokens["text-body"]), canvas);
    const mutedRatio = contrastRatio(parseColor(tokens["text-muted"]), canvas);
    expect(bodyRatio, `body / canvas 实际 ${bodyRatio.toFixed(2)}:1`).toBeGreaterThanOrEqual(
      4.5,
    );
    expect(mutedRatio, `muted / canvas 实际 ${mutedRatio.toFixed(2)}:1`).toBeGreaterThanOrEqual(
      4.5,
    );
    expect(bodyRatio).toBeGreaterThan(mutedRatio);
  });
});

describe("账号状态汇总条对比度（issue #134）", () => {
  // 每条横幅：浅底 + 同族标题 + 同族说明；标题 / 说明对底色都须 ≥ 4.5:1。
  const banners = ["ok", "warn", "high", "full"] as const;

  it("四态标题与说明对各自浅底 ≥ 4.5:1", () => {
    const report: string[] = [];
    for (const tone of banners) {
      const bg = parseColor(tokens[`status-${tone}-bg`]);
      for (const layer of ["title", "detail"] as const) {
        const fg = parseColor(tokens[`status-${tone}-${layer}`]);
        const ratio = contrastRatio(fg, bg);
        report.push(`status-${tone}-${layer} / status-${tone}-bg: ${ratio.toFixed(2)}:1`);
        expect(
          ratio,
          `status-${tone}-${layer} 对 status-${tone}-bg 应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`,
        ).toBeGreaterThanOrEqual(4.5);
      }
    }
    expect(report).toHaveLength(8);
  });

  it("正常态说明弱于标题但仍可读（参考图的浅一点说明）", () => {
    const bg = parseColor(tokens["status-ok-bg"]);
    const title = contrastRatio(parseColor(tokens["status-ok-title"]), bg);
    const detail = contrastRatio(parseColor(tokens["status-ok-detail"]), bg);
    expect(title).toBeGreaterThan(detail);
    expect(detail).toBeGreaterThanOrEqual(4.5);
  });

  it("状态图标白色图形对状态色圆盘 ≥ 4.5:1（四态）", () => {
    const glyph = parseColor(tokens["text-inverse"]);
    const report: string[] = [];
    for (const tone of banners) {
      const plate = parseColor(tokens[`status-${tone}-title`]);
      const ratio = contrastRatio(glyph, plate);
      report.push(`glyph / status-${tone}-title: ${ratio.toFixed(2)}:1`);
      expect(
        ratio,
        `白色图形对 status-${tone}-title 应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`,
      ).toBeGreaterThanOrEqual(4.5);
    }
    expect(report).toHaveLength(4);
  });
});

describe("账号库凭证待续期奶油提示条对比度（issue #142）", () => {
  it("提示条绑定总览续期态同一组 status-warn token，不另立色值", () => {
    // 与 #141 同一测法：直接从规则块提取引用，防止条底被改成未测比值的颜色。
    const banner = /(^|\n)\.profile-renewal-banner\s*\{[^}]*\}/.exec(css)?.[0] ?? "";
    expect(banner).toContain("background: var(--status-warn-bg)");
    expect(banner).toContain("color: var(--status-warn-title)");
    const detail =
      /\.profile-renewal-banner \.account-status-detail\s*\{[^}]*\}/.exec(css)?.[0] ?? "";
    expect(detail).toContain("color: var(--status-warn-detail)");
  });

  it("标题与说明对奶油底 ≥ 4.5:1，白色图形对状态色圆盘 ≥ 4.5:1", () => {
    const bg = parseColor(tokens["status-warn-bg"]);
    const plate = parseColor(tokens["status-warn-title"]);
    const glyph = parseColor(tokens["text-inverse"]);
    const pairs = [
      { fg: parseColor(tokens["status-warn-title"]), bg, label: "标题 / 奶油底" },
      { fg: parseColor(tokens["status-warn-detail"]), bg, label: "说明 / 奶油底" },
      { fg: glyph, bg: plate, label: "白色图形 / 状态色圆盘" },
    ];
    const report: string[] = [];
    for (const pair of pairs) {
      const ratio = contrastRatio(pair.fg, pair.bg);
      report.push(`${pair.label}: ${ratio.toFixed(2)}:1`);
      expect(
        ratio,
        `${pair.label} 应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`,
      ).toBeGreaterThanOrEqual(4.5);
    }
    expect(report).toHaveLength(pairs.length);
  });
});

describe("总览新按钮与金徽章对比度（issue #133）", () => {
  it("蓝色渐变主按钮的每一档 stop 上白字均 ≥ 4.5:1", () => {
    const surfaces = opaqueSurfaces(tokens, "control-primary-background");
    // 两档 stop 才拆得出生效的守卫；stop 被删到一档说明渐变被静默改回纯色。
    expect(surfaces).toHaveLength(2);
    const inverse = parseColor(tokens["text-inverse"]);
    const report: string[] = [];
    for (const surface of surfaces) {
      const ratio = contrastRatio(inverse, surface.bg);
      report.push(`白字 / --${surface.label}: ${ratio.toFixed(2)}:1`);
      expect(
        ratio,
        `白字对 ${surface.label} 应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`,
      ).toBeGreaterThanOrEqual(4.5);
    }
    expect(report).toHaveLength(surfaces.length);
  });

  it("「会员账号」金徽章文字对金底 ≥ 4.5:1", () => {
    const fg = parseColor(tokens["badge-gold-text"]);
    const bg = parseColor(tokens["badge-gold-background"]);
    const ratio = contrastRatio(fg, bg);
    expect(
      ratio,
      `badge-gold-text 对 badge-gold-background 应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`,
    ).toBeGreaterThanOrEqual(4.5);
  });
});

describe("账号卡身份区头像与色片对比度（issue #141）", () => {
  it("头像字对头像底 ≥ 4.5:1：当前蓝 / 停泊暖橙的指定纯色，不用绿", () => {
    // 指定纯色：当前=主色系蓝（字用主色渐变深档）、停泊=暖橙，沿用 #128 的指定色测法。
    expect(tokens["profile-avatar-current-background"]).toBe("#e4edfe");
    expect(tokens["profile-avatar-current-text"]).toBe("#1f5ae0");
    expect(tokens["profile-avatar-parked-background"]).toBe("#fdebd8");
    expect(tokens["profile-avatar-parked-text"]).toBe("#9a3412");
    const pairs = [
      {
        fg: "profile-avatar-current-text",
        bg: "profile-avatar-current-background",
        label: "当前头像字",
      },
      {
        fg: "profile-avatar-parked-text",
        bg: "profile-avatar-parked-background",
        label: "停泊头像字",
      },
    ] as const;
    const report: string[] = [];
    for (const pair of pairs) {
      const ratio = contrastRatio(parseColor(tokens[pair.fg]), parseColor(tokens[pair.bg]));
      report.push(`${pair.label}: ${ratio.toFixed(2)}:1`);
      expect(
        ratio,
        `${pair.label} 对头像底应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`,
      ).toBeGreaterThanOrEqual(4.5);
    }
    expect(report).toHaveLength(pairs.length);
  });

  it("「当前」色片字对色片底 ≥ 4.5:1，且与当前头像共用同一对 token 不漂移", () => {
    const ratio = contrastRatio(
      parseColor(tokens["profile-avatar-current-text"]),
      parseColor(tokens["profile-avatar-current-background"]),
    );
    expect(
      ratio,
      `色片字对色片底应为 ≥ 4.5:1，实际 ${ratio.toFixed(2)}:1`,
    ).toBeGreaterThanOrEqual(4.5);
    // 色片与当前头像引用同一对 token：一方调色时另一方不会掉出守卫。
    const chip = /(^|\n)\.profile-current-chip\s*\{[^}]*\}/.exec(css)?.[0] ?? "";
    expect(chip).toContain("var(--profile-avatar-current-background)");
    expect(chip).toContain("var(--profile-avatar-current-text)");
  });

  it("头像圆默认停泊暖橙对，当前卡变体切蓝对", () => {
    const avatar = /(^|\n)\.profile-avatar\s*\{[^}]*\}/.exec(css)?.[0] ?? "";
    expect(avatar).toContain("var(--profile-avatar-parked-background)");
    expect(avatar).toContain("var(--profile-avatar-parked-text)");
    const currentAvatar =
      /(^|\n)\.profile-card\.is-current \.profile-avatar\s*\{[^}]*\}/.exec(css)?.[0] ?? "";
    expect(currentAvatar).toContain("var(--profile-avatar-current-background)");
    expect(currentAvatar).toContain("var(--profile-avatar-current-text)");
  });
});

describe("焦点环对比度", () => {
  it("focus-ring 对 canvas/面板表面均 ≥ 3:1", () => {
    const report: string[] = [];
    const surfaces = ["canvas-bg", "field-background"] as const;
    for (const token of surfaces) {
      const canvas = parseColor(tokens["canvas-bg"]);
      const ring = parseColor(tokens["focus-ring"]);
      const bgToken = parseColor(tokens[token]);
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
