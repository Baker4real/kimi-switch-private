import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

const dir = dirname(fileURLToPath(import.meta.url));
const css = readFileSync(join(dir, "styles.css"), "utf8").replace(/\r\n/g, "\n");
const html = readFileSync(join(dir, "index.html"), "utf8").replace(/\r\n/g, "\n");

function ruleBody(selector: string): string {
  const idx = css.indexOf(`${selector} {`);
  if (idx < 0) {
    throw new Error(`缺少规则: ${selector}`);
  }
  const start = css.indexOf("{", idx);
  const end = css.indexOf("}", start);
  return css.slice(start + 1, end);
}

function rootBlockEnd(): number {
  const idx = css.indexOf(":root {");
  if (idx < 0) {
    throw new Error("缺少 :root");
  }
  const start = css.indexOf("{", idx);
  let depth = 0;
  for (let i = start; i < css.length; i += 1) {
    if (css[i] === "{") {
      depth += 1;
    } else if (css[i] === "}") {
      depth -= 1;
      if (depth === 0) {
        return i;
      }
    }
  }
  throw new Error(":root 未闭合");
}

function stripCssComments(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, "");
}

/** 颜色/字体那段独立 `body`，不是 `html, body` 高度规则。 */
function colorFontBodyRule(): string {
  const idx = css.lastIndexOf("\nbody {");
  if (idx < 0) {
    throw new Error("缺少独立 body 规则");
  }
  const start = css.indexOf("{", idx);
  const end = css.indexOf("}", start);
  return css.slice(start + 1, end);
}

/** 从 tauri 窗口配置读取 minWidth，避免测试里再写一套会漂移的数字。 */
function windowMinWidth(): number {
  const confPath = join(dir, "..", "src-tauri", "tauri.conf.json");
  const conf = JSON.parse(readFileSync(confPath, "utf8")) as {
    app: { windows: { label?: string; minWidth?: number }[] };
  };
  const main =
    conf.app.windows.find((window) => window.label === "main") ?? conf.app.windows[0];
  const minWidth = main?.minWidth;
  if (typeof minWidth !== "number") {
    throw new Error("tauri.conf.json 缺少 app.windows[].minWidth");
  }
  return minWidth;
}

function mediaMaxWidthBlocks(): { maxWidth: number; body: string }[] {
  const blocks: { maxWidth: number; body: string }[] = [];
  const re = /@media \(max-width:\s*(\d+)px\) \{/g;
  let match: RegExpExecArray | null = re.exec(css);
  while (match !== null) {
    const brace = match.index + match[0].length - 1;
    let depth = 0;
    let end = -1;
    for (let i = brace; i < css.length; i += 1) {
      if (css[i] === "{") {
        depth += 1;
      } else if (css[i] === "}") {
        depth -= 1;
        if (depth === 0) {
          end = i;
          break;
        }
      }
    }
    if (end < 0) {
      throw new Error(`@media (max-width: ${match[1]}px) 未闭合`);
    }
    blocks.push({
      maxWidth: Number(match[1]),
      body: css.slice(brace + 1, end),
    });
    match = re.exec(css);
  }
  return blocks;
}

describe("中文标题排印", () => {
  it("展示字体链在 Cormorant 失败后先 Times New Roman，再 Noto Sans SC", () => {
    expect(css).toMatch(
      /--font-display:\s*"Cormorant Garamond Custom",\s*"Times New Roman",\s*"Noto Sans SC Custom",\s*serif/,
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
    expect(label).toMatch(/font-size:\s*var\(--fs-xs\)\s*;/);
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

describe("中西文字重对齐（issue #125）", () => {
  it("声明 IBM Plex Sans Regular 400，且 Regular 文件存在", () => {
    const regularFace = css.match(/@font-face\s*\{[^}]*IBMPlexSans-Regular\.ttf[^}]*\}/);
    expect(regularFace, "缺少 IBMPlexSans-Regular.ttf 的 @font-face").toBeTruthy();
    expect(regularFace![0]).toMatch(/font-family:\s*"IBM Plex Sans Custom"/);
    expect(regularFace![0]).toMatch(/font-weight:\s*400\s*;/);
    expect(regularFace![0]).toMatch(/font-display:\s*swap\s*;/);

    const regularIdx = css.indexOf("IBMPlexSans-Regular.ttf");
    const mediumIdx = css.indexOf("IBMPlexSans-Medium.ttf");
    expect(regularIdx).toBeGreaterThanOrEqual(0);
    expect(mediumIdx).toBeGreaterThan(regularIdx);

    const fontPath = join(dir, "fonts", "IBMPlexSans-Regular.ttf");
    expect(existsSync(fontPath)).toBe(true);
    expect(statSync(fontPath).size).toBeGreaterThan(10 * 1024);
  });

  it("注释标明中文标题用黑体，Cormorant 只作拉丁/数字点缀", () => {
    expect(css).toMatch(/黑体/);
    expect(css).toMatch(/点缀/);
  });

  it("展示字体链仍是 Cormorant → Times New Roman → Noto，body 回退不退化", () => {
    expect(css).toMatch(
      /--font-display:\s*"Cormorant Garamond Custom",\s*"Times New Roman",\s*"Noto Sans SC Custom",\s*serif/,
    );
    expect(css).toMatch(
      /--font-body:\s*"IBM Plex Sans Custom",\s*"Noto Sans SC Custom",\s*"Segoe UI",\s*sans-serif/,
    );
  });
});

describe("排版基底（issue #124）", () => {
  it("body 颜色字体规则的基准行高为 1.5", () => {
    expect(colorFontBodyRule()).toMatch(/line-height:\s*1\.5\s*;/);
  });

  it("额度百分比与重置时间用 tabular-nums，不写在 body 或标签", () => {
    expect(ruleBody(".quota-value,\n.quota-reset")).toMatch(
      /font-variant-numeric:\s*tabular-nums\s*;/,
    );
    expect(ruleBody(".quota-label,\n.quota-reset")).not.toMatch(/tabular-nums/);
    expect(ruleBody("html,\nbody")).not.toMatch(/tabular-nums/);
    expect(colorFontBodyRule()).not.toMatch(/tabular-nums/);
  });

  it("窗口标题是页面唯一 h1，class 仍为 window-app-title", () => {
    const openings = [...html.matchAll(/<h1\b[^>]*>/g)].map((match) => match[0]);
    expect(openings).toHaveLength(1);
    expect(openings[0]).toBe('<h1 class="window-app-title">');
    expect(html).toContain('<h1 class="window-app-title">Kimi 换号器</h1>');
    expect(html).not.toContain('<span class="window-app-title">');
  });
});

describe("字号与间距设计 token", () => {
  it("字号语义 token 集合在 :root 完整且值为既定刻度", () => {
    const root = ruleBody(":root");
    expect(root).toMatch(/--fs-xs:\s*0\.75rem\s*;/);
    expect(root).toMatch(/--fs-sm:\s*0\.82rem\s*;/);
    expect(root).toMatch(/--fs-md:\s*0\.95rem\s*;/);
    expect(root).toMatch(/--fs-lg:\s*1\.35rem\s*;/);
    expect(root).toMatch(/--fs-xl:\s*2rem\s*;/);
    expect(root).toMatch(/--fs-display:\s*2\.125rem\s*;/);
  });

  it("间距 token 覆盖 xs=6 到 2xl=24 以及现有精确 px", () => {
    const root = ruleBody(":root");
    expect(root).toMatch(/--space-xs:\s*6px\s*;/);
    expect(root).toMatch(/--space-sm:\s*8px\s*;/);
    expect(root).toMatch(/--space-md:\s*12px\s*;/);
    expect(root).toMatch(/--space-lg:\s*16px\s*;/);
    expect(root).toMatch(/--space-xl:\s*20px\s*;/);
    expect(root).toMatch(/--space-2xl:\s*24px\s*;/);
    expect(root).toMatch(/--space-3xs:\s*2px\s*;/);
    expect(root).toMatch(/--space-2xs:\s*4px\s*;/);
    expect(root).toMatch(/--space-10:\s*10px\s*;/);
    expect(root).toMatch(/--space-14:\s*14px\s*;/);
    expect(root).toMatch(/--space-3xl:\s*28px\s*;/);
  });

  it("字号声明只能引用 --fs-* token 或额度徽标 12px", () => {
    const decls = [...css.matchAll(/font-size:\s*([^;]+);/g)].map((match) => match[1].trim());
    const allowed = /^var\(--fs-(?:xs|sm|md|lg|xl|display)\)$/;
    const pxBadges = decls.filter((value) => value === "12px");
    expect(pxBadges).toHaveLength(1);
    for (const value of decls) {
      if (value === "12px") {
        continue;
      }
      expect(value, `字号必须来自 token 集合，实际: ${value}`).toMatch(allowed);
    }
  });

  it("额度徽标 12px 保留且注明有意用 px", () => {
    const idx = css.indexOf(".quota-value {");
    expect(idx).toBeGreaterThanOrEqual(0);
    const body = css.slice(idx, idx + 700);
    expect(body).toMatch(/有意用 px/);
    expect(body).toMatch(/font-size:\s*12px\s*;/);
  });

  it("gap/padding 除 18px 与 22px 外只引用间距 token", () => {
    const afterRoot = stripCssComments(css.slice(rootBlockEnd() + 1));
    const props = /(?:gap|row-gap|column-gap|padding|padding-top|padding-right|padding-bottom|padding-left):\s*([^;]+);/g;
    const allowedPart = /^(?:0|18px|22px|var\(--space-[\w-]+\))$/;
    for (const match of afterRoot.matchAll(props)) {
      const value = match[1].trim();
      for (const part of value.split(/\s+/)) {
        expect(part, `间距必须为 token 或 18/22 例外，实际: ${value}`).toMatch(
          allowedPart,
        );
      }
    }
  });

  it(":root 之外不再有硬编码颜色", () => {
    const afterRoot = stripCssComments(css.slice(rootBlockEnd() + 1));
    expect(afterRoot).not.toMatch(/#[0-9a-fA-F]{3,8}\b/);
    expect(afterRoot).not.toMatch(/\brgba?\s*\(/i);
    expect(afterRoot).not.toMatch(/\bhsla?\s*\(/i);
  });
});

describe("交互状态与可达性（issue #126）", () => {
  it("开关是 44×24，拇指 18×18，选中位移 20px", () => {
    const track = ruleBody(".switch");
    expect(track).toMatch(/width:\s*44px/);
    expect(track).toMatch(/height:\s*24px/);
    expect(track).toMatch(/border:\s*1px solid var\(--switch-border\)/);
    const thumb = ruleBody(".switch-thumb");
    expect(thumb).toMatch(/width:\s*18px/);
    expect(thumb).toMatch(/height:\s*18px/);
    expect(ruleBody('.switch[aria-checked="true"] .switch-thumb')).toMatch(
      /transform:\s*translateX\(20px\)/,
    );
  });

  it("未选中开关边框走 :root token，不在规则里写死颜色", () => {
    expect(ruleBody(":root")).toMatch(/--switch-border:/);
    expect(ruleBody(".switch")).toMatch(/var\(--switch-border\)/);
  });

  it("开关轨道与拇指过渡 140ms；减弱动效时关掉", () => {
    expect(ruleBody(".switch")).toMatch(/transition:[^;]*140ms/);
    expect(ruleBody(".switch-thumb")).toMatch(/transition:[^;]*140ms/);
    const reduce = reducedMotionBlock();
    expect(reduce).toMatch(/\.switch[\s,{][\s\S]*?transition:\s*none/);
    expect(reduce).toMatch(/\.switch-thumb[\s,{][\s\S]*?transition:\s*none/);
  });

  it("未选中与选中开关悬停都有可见反馈", () => {
    expect(css).toMatch(/\.switch:hover:not\(:disabled\)/);
    expect(css).toMatch(/\.switch\[aria-checked="true"\]:hover:not\(:disabled\)/);
    const offHover = ruleBody(".switch:hover:not(:disabled)");
    const onHover = ruleBody('.switch[aria-checked="true"]:hover:not(:disabled)');
    expect(offHover).toMatch(/var\(--/);
    expect(onHover).toMatch(/var\(--/);
    expect(offHover).not.toEqual(onHover);
  });

  it("按钮与账号药丸按压回落 translateY，约 60ms，disabled 除外", () => {
    const body = ruleBody(
      ".action-button:active:not(:disabled),\n.profile-pill:active:not(:disabled)",
    );
    expect(body).toMatch(/transform:\s*translateY\(/);
    expect(body).toMatch(/60ms/);
  });

  it("减弱动效时按钮按压也不位移", () => {
    const reduce = reducedMotionBlock();
    expect(reduce).toMatch(/\.action-button:active:not\(:disabled\)/);
    expect(reduce).toMatch(/transform:\s*none/);
  });

  it("账号行为折叠行悬停有背景或边框反馈", () => {
    const hover = ruleBody(".settings-disclosure:hover");
    expect(hover).toMatch(/var\(--/);
    expect(`${hover}`).toMatch(/(background|border(?:-color)?):/);
  });

  it("危险按钮悬停用 --danger-hover-background，不改淡", () => {
    const idx = css.indexOf(".profile-pill--danger:hover:not(:disabled)");
    expect(idx).toBeGreaterThanOrEqual(0);
    const body = css.slice(idx, idx + 320);
    expect(body).toContain("var(--danger-hover-background)");
    expect(body).toContain("var(--danger-text)");
    expect(body).toContain(".action-button-danger:hover:not(:disabled)");
  });

  it("禁用按钮、药丸与开关光标都是 not-allowed", () => {
    expect(ruleBody(".action-button:disabled,\n.profile-pill:disabled")).toMatch(
      /cursor:\s*not-allowed/,
    );
    expect(ruleBody(".switch:disabled")).toMatch(/cursor:\s*not-allowed/);
  });

  it("子开关所在行在开关禁用时文案降不透明度", () => {
    const body = ruleBody(".settings-row:has(.switch:disabled) .settings-copy");
    expect(body).toMatch(/opacity:\s*0?\.\d+/);
  });
});

function reducedMotionBlock(): string {
  const start = css.indexOf("@media (prefers-reduced-motion: reduce)");
  if (start < 0) {
    throw new Error("缺少 prefers-reduced-motion");
  }
  const brace = css.indexOf("{", start);
  let depth = 0;
  for (let i = brace; i < css.length; i += 1) {
    if (css[i] === "{") {
      depth += 1;
    } else if (css[i] === "}") {
      depth -= 1;
      if (depth === 0) {
        return css.slice(start, i + 1);
      }
    }
  }
  throw new Error("prefers-reduced-motion 未闭合");
}

describe("状态色与视觉层次（issue #128）", () => {
  it("四档 tone 为指定浅色纯色，不含 90° 渐变", () => {
    const root = ruleBody(":root");
    expect(root).toMatch(/--tone-ok:\s*#1a7f37\s*;/);
    expect(root).toMatch(/--tone-warn:\s*#9a6700\s*;/);
    expect(root).toMatch(/--tone-high:\s*#bc4c00\s*;/);
    expect(root).toMatch(/--tone-full:\s*#cf222e\s*;/);
    expect(root).not.toMatch(/--tone-(?:ok|warn|high|full):\s*linear-gradient/i);
  });

  it("额度与会员填充有 6px 最小宽度，避免极窄时退化成圆点", () => {
    expect(ruleBody(".quota-fill")).toMatch(/min-width:\s*6px\s*;/);
  });

  it("body / muted 文字层次为指定值，且强于彼此可辨", () => {
    const root = ruleBody(":root");
    expect(root).toMatch(/--text-body:\s*#5b5d6e\s*;/);
    expect(root).toMatch(/--text-muted:\s*#686a7e\s*;/);
    expect(root).toMatch(/--text-strong:\s*#565867\s*;/);
  });

  it("账号卡与行为面板用 slate 发丝边，不用 cyan accent", () => {
    const card = ruleBody(".profile-card");
    const panel = ruleBody(".behavior-panel");
    expect(card).toMatch(/border:\s*1px solid var\(--border-card\)/);
    expect(card).not.toMatch(/--border-accent/);
    expect(panel).toMatch(/border:\s*1px solid var\(--border-card\)/);
    expect(panel).not.toMatch(/--border-accent/);
  });

  it("当前账号卡有独立于停泊卡的边框或光晕", () => {
    const parked = ruleBody(".profile-card");
    const current = ruleBody(".profile-card.is-current");
    const parkedBorder = parked.match(/border:\s*1px solid var\((--[\w-]+)\)/)?.[1];
    const currentBorder = current.match(/border-color:\s*var\((--[\w-]+)\)/)?.[1];
    const currentShadow = current.match(/box-shadow:\s*([^;]+)/)?.[1]?.trim();
    expect(parkedBorder, "停泊卡须声明边框 token").toBeTruthy();
    expect(current).not.toMatch(/--border-accent/);
    const distinctBorder = Boolean(currentBorder && currentBorder !== parkedBorder);
    const distinctShadow = Boolean(
      currentShadow && !parked.includes(`box-shadow: ${currentShadow}`),
    );
    expect(
      distinctBorder || distinctShadow,
      "当前卡须有不同于停泊卡的边框色或 box-shadow",
    ).toBe(true);
  });

  it("总览与账号库内容区限宽 1200px 居中，工具栏与标题栏不限宽", () => {
    const page = ruleBody(".page");
    expect(page).toMatch(/max-width:\s*1200px\s*;/);
    expect(page).toMatch(/margin-inline:\s*auto\s*;/);
    expect(ruleBody(".app-toolbar")).not.toMatch(/max-width:\s*1200px/);
    expect(ruleBody(".window-titlebar")).not.toMatch(/max-width:\s*1200px/);
  });

  it("cyan 点缀仍留在窗口徽标与监控脉冲点", () => {
    expect(ruleBody(".window-app-mark")).toMatch(/var\(--blue-fill\)/);
    expect(ruleBody(".window-app-mark")).toMatch(/var\(--monitor-dot\)/);
    expect(ruleBody(".quota-monitor-dot")).toMatch(/var\(--monitor-dot\)/);
    expect(ruleBody(":root")).toMatch(/--border-accent:/);
  });
});

describe("布局断点与窗口最小宽度自洽", () => {
  it("不再保留 1100px 与 760px 媒体查询", () => {
    const widths = mediaMaxWidthBlocks().map((block) => block.maxWidth);
    expect(widths).not.toContain(1100);
    expect(widths).not.toContain(760);
  });

  it("双栏塌列断点小于窗口 minWidth，且只作用于 .dashboard-grid", () => {
    const minWidth = windowMinWidth();
    const collapse = mediaMaxWidthBlocks().find((block) =>
      block.body.includes(".dashboard-grid"),
    );
    expect(collapse, "缺少 .dashboard-grid 塌列媒体查询").toBeDefined();
    expect(collapse!.maxWidth).toBeLessThan(minWidth);
    expect(collapse!.body).not.toMatch(/\.dashboard-grid\s*,/);
    expect(collapse!.body).not.toMatch(/\.status-grid--three/);
    expect(collapse!.body).not.toMatch(/\.control-stack--three/);
    expect(collapse!.body).toMatch(/grid-template-columns:\s*1fr/);
  });
});
