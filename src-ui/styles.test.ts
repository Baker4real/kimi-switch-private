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
  it("全应用统一无衬线链：IBM Plex Sans → Noto Sans SC，无展示字体 token", () => {
    expect(css).toMatch(
      /--font-body:\s*"IBM Plex Sans Custom",\s*"Noto Sans SC Custom",\s*"Segoe UI",\s*sans-serif/,
    );
    // 衬线展示字体已整体移除（issue #131）。
    expect(css).not.toMatch(/--font-display\s*:/);
    // 只允许 sans-serif 回退；不得再出现裸 serif 衬线回退。
    expect(css).not.toMatch(/(?<!sans-)serif/);
    expect(css).not.toMatch(/Cormorant|Times New Roman/);
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

  it("注释标明中文用 Noto Sans SC 黑体，字体目录不再含衬线文件", () => {
    expect(css).toMatch(/黑体/);
    expect(css).toMatch(/统一无衬线/);
    // 衬线展示字体文件已删除，构建产物不再打包（issue #131）。
    expect(existsSync(join(dir, "fonts", "CormorantGaramond-SemiBold.ttf"))).toBe(false);
  });

  it("字体链收敛为无衬线，body 回退不退化", () => {
    expect(css).not.toMatch(/--font-display/);
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
    // 总览「账号操作」大按钮与次级按钮同一按压语言（issue #133）。
    const control = ruleBody(".control-action:active:not(:disabled)");
    expect(control).toMatch(/transform:\s*translateY\(/);
    expect(control).toMatch(/60ms/);
  });

  it("减弱动效时按钮按压也不位移", () => {
    const reduce = reducedMotionBlock();
    expect(reduce).toMatch(/\.action-button:active:not\(:disabled\)/);
    expect(reduce).toMatch(/\.control-action:active:not\(:disabled\)/);
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
    expect(root).toMatch(/--tone-ok:\s*#128a3e\s*;/);
    expect(root).toMatch(/--tone-warn:\s*#a16207\s*;/);
    expect(root).toMatch(/--tone-high:\s*#c2410c\s*;/);
    expect(root).toMatch(/--tone-full:\s*#dc2626\s*;/);
    expect(root).not.toMatch(/--tone-(?:ok|warn|high|full):\s*linear-gradient/i);
  });

  it("额度与会员填充有 6px 最小宽度，避免极窄时退化成圆点", () => {
    expect(ruleBody(".quota-fill")).toMatch(/min-width:\s*6px\s*;/);
  });

  it("body / muted 文字层次为指定值，且强于彼此可辨", () => {
    const root = ruleBody(":root");
    expect(root).toMatch(/--text-body:\s*#5f6884\s*;/);
    expect(root).toMatch(/--text-muted:\s*#666e8c\s*;/);
    expect(root).toMatch(/--text-strong:\s*#1b2545\s*;/);
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

  it(".page 须有确定宽度：纵向 flex 父级下 auto margin 会取代 stretch 导致内容收缩", () => {
    // 回归：仅 max-width + margin-inline:auto 时页面按 max-content 收缩，
    // 账号卡额度轨道（1fr 列 + 绝对定位徽章）塌成约 6px，进度条被徽章盖住。
    expect(ruleBody(".page")).toMatch(/width:\s*100%\s*;/);
  });

  it("蓝色点缀仍留在窗口徽标渐变与监控脉冲点", () => {
    // issue #132 后徽标是内联 SVG 渐变：起止色分别落在两个 stop 规则上。
    expect(ruleBody(".window-app-mark-start")).toMatch(/stop-color:\s*var\(--blue-fill\)/);
    expect(ruleBody(".window-app-mark-end")).toMatch(/stop-color:\s*var\(--monitor-dot\)/);
    expect(ruleBody(".quota-monitor-dot")).toMatch(/var\(--monitor-dot\)/);
    expect(ruleBody(":root")).toMatch(/--border-accent:/);
  });
});

describe("浅蓝白卡片 token 体系（issue #131）", () => {
  it("底色与主色按参考图重取：浅蓝白底、亮蓝主色、白色卡片", () => {
    const root = ruleBody(":root");
    expect(root).toMatch(/--canvas-bg:\s*#eef4fd\s*;/);
    expect(root).toMatch(/--surface-pressed:\s*#2568f6\s*;/);
    expect(root).toMatch(/--primary-background:\s*#2568f6\s*;/);
    expect(root).toMatch(/--field-background:\s*#ffffff\s*;/);
    expect(root).toMatch(/--text-inverse:\s*#ffffff\s*;/);
    expect(root).toMatch(/--focus-ring:\s*#2568f6\s*;/);
  });

  it("圆角刻度：卡片 20、控件 14、输入 10，语义 token 引用刻度", () => {
    const root = ruleBody(":root");
    expect(root).toMatch(/--radius-lg:\s*20px\s*;/);
    expect(root).toMatch(/--radius-md:\s*14px\s*;/);
    expect(root).toMatch(/--radius-sm:\s*10px\s*;/);
    expect(root).toMatch(/--radius-card:\s*var\(--radius-lg\)\s*;/);
  });

  it("对话框 / 输入框 / 提示条圆角走刻度 token，不再硬编码 24/16px", () => {
    expect(ruleBody(".app-dialog")).toMatch(/border-radius:\s*var\(--radius-lg\)/);
    expect(ruleBody(".toast")).toMatch(/border-radius:\s*var\(--radius-md\)/);
    expect(ruleBody(".field input")).toMatch(/border-radius:\s*var\(--radius-sm\)/);
  });
});

describe("总览额度行与药丸 tab 排布（issue #136 验收回归）", () => {
  it("hero 额度行：重置时间与用量值同处右列，不再单独占第三行", () => {
    const row = ruleBody(".quota-row--hero");
    // 参考图排布：图标列 + 左列「窗口名 / 轨道」两行，右列竖排「用量值 / 重置时间」。
    expect(row).toMatch(/grid-template-areas:\s*"icon label tail"\s*"icon track tail"/);
    expect(row).not.toMatch(/"icon head"/);
    expect(row).not.toMatch(/"icon reset"/);
    // 右列容器承接用量值 + 重置时间。
    const tail = ruleBody(".quota-row-tail");
    expect(tail).toMatch(/flex-direction:\s*column/);
    expect(tail).toMatch(/align-items:\s*flex-end/);
  });

  it("「更新于」hero 行字号压过 `.quota-cached` 基础规则（同特异度顺序陷阱）", () => {
    // 基础 `.quota-cached` 与 hero 同特异度时，后者靠后的位置不再保证生效；
    // 必须用复合选择器提高一档，否则字号被静默压回 --fs-xs（issue #136 偏差 2/3）。
    const hero = ruleBody(".quota-cached.quota-cached--hero");
    expect(hero).toMatch(/font-size:\s*var\(--fs-md\)/);
  });

  it("「更新于」标签与时间之间的斜杠由紧邻容器控制，不被 flex gap 撑开", () => {
    expect(ruleBody(".quota-cached--hero .quota-cached-text")).toMatch(
      /display:\s*inline-flex/,
    );
    const sep = ruleBody(".quota-cached--hero .quota-cached-sep");
    expect(sep).toMatch(/margin:\s*0 var\(--space-2xs\)/);
  });

  it("hero 标签随参考图用 --fs-md，重置时间不再是 --fs-xs", () => {
    expect(ruleBody(".quota-row--hero .quota-label")).toMatch(/font-size:\s*var\(--fs-md\)/);
    expect(ruleBody(".quota-row--hero .quota-reset")).toMatch(/font-size:\s*var\(--fs-md\)/);
  });

  it("药丸 tab 按参考图比例：更高更宽，字号上提一档", () => {
    const tab = ruleBody(".route-tab");
    expect(tab).toMatch(/min-width:\s*128px\s*;/);
    expect(tab).toMatch(/height:\s*50px\s*;/);
    expect(tab).toMatch(/font-size:\s*var\(--fs-md\)/);
    // 容器内边距同步收敛，pill 与外壳比例与参考图一致。
    // 注意 `.route-tabs {` 会先命中 `.app-toolbar .route-tabs {`，故按行首定位独立规则。
    const standalone = css.slice(css.indexOf("\n.route-tabs {"));
    const wrap = standalone.slice(standalone.indexOf("{") + 1, standalone.indexOf("}"));
    expect(wrap).toMatch(/padding:\s*var\(--space-2xs\)/);
    expect(wrap).toMatch(/gap:\s*var\(--space-2xs\)/);
  });

  it("账号操作图标：次要按钮无底圈，主按钮白圆铺满图标位", () => {
    // 底圈与浅蓝底已从图标位移除（参考图次按钮是裸描边图标）。
    const mark = ruleBody(".control-action-mark");
    expect(mark).not.toMatch(/background\s*:/);
    expect(mark).not.toMatch(/border-radius\s*:/);
    expect(ruleBody(".control-action-mark .control-action-disc")).toMatch(/width:\s*100%/);
  });
});

describe("工具栏常驻可见（issue #129）", () => {
  it("滚动容器不是工具栏的父壳：纵向滚动下放到 .page", () => {
    // 工具栏是 .dashboard-shell 的直接子元素；若外壳仍是纵向滚动容器，
    // 列表一长工具栏就会被一起卷走。外壳只保留裁剪，滚动交给 .page。
    const shell = ruleBody(".dashboard-shell");
    expect(shell).toMatch(/overflow:\s*hidden\s*;/);
    expect(shell).not.toMatch(/overflow(?:-y)?:\s*(?:auto|scroll)/);

    const page = ruleBody(".page");
    expect(page).toMatch(/overflow:\s*(?:auto|scroll)\s*;/);
    expect(page).toMatch(/(?:^|\n)\s*flex:\s*1\s*;/);
    expect(page).toMatch(/min-height:\s*0\s*;/);
  });

  it("工具栏不收缩，且不依赖 position: sticky", () => {
    expect(ruleBody(".app-toolbar")).toMatch(/flex-shrink:\s*0\s*;/);
    // 注释里提到 sticky 不算实现（本文件注释正说明刻意不用它）。
    expect(stripCssComments(css)).not.toMatch(/position:\s*sticky/);
  });

  it("index.html 中工具栏是页面滚动区的兄弟，不是 .page 的后代", () => {
    const toolbarIdx = html.indexOf('<section class="app-toolbar">');
    const pageIdx = html.indexOf('<section class="page active"');
    expect(toolbarIdx).toBeGreaterThanOrEqual(0);
    expect(pageIdx).toBeGreaterThanOrEqual(0);
    expect(toolbarIdx).toBeLessThan(pageIdx);
    // 工具栏 section 在页面 section 之前闭合，二者同级。
    expect(html.slice(toolbarIdx, pageIdx)).toContain("</section>");
  });
});

describe("壳层底栏与窗口默认尺寸（issue #132）", () => {
  it("状态栏是壳层固定底栏：不收缩、不进滚动区、横向可达", () => {
    const bar = ruleBody(".app-statusbar");
    expect(bar).toMatch(/flex-shrink:\s*0\s*;/);
    expect(bar).not.toMatch(/position:\s*(?:absolute|fixed|sticky)/);
    // 滚动仍由 .dashboard-shell 裁剪、.page 负责，底栏不进入滚动区。
    expect(ruleBody(".dashboard-shell")).toMatch(/overflow:\s*hidden\s*;/);
    // 纵向滚动仍留在 .page：视口缩小时不把底栏挤出去。
    expect(ruleBody(".page")).toMatch(/overflow:\s*(?:auto|scroll)\s*;/);
    expect(ruleBody(".app-statusbar")).toMatch(/justify-content:\s*space-between\s*;/);
  });

  it("toast 让位高度与底栏高度绑定同一 token（单一出处，跨票契约）", () => {
    // 底栏实际渲染高度取 --statusbar-height，toast 的 bottom 必须引用同一 token；
    // 任一改动未同步时，toast 会盖住或远离底栏版本号。
    expect(ruleBody(":root")).toMatch(/--statusbar-height:\s*\d+px\s*;/);
    expect(ruleBody(".app-statusbar")).toMatch(/min-height:\s*var\(--statusbar-height\)\s*;/);
    expect(ruleBody(".toast")).toMatch(
      /bottom:\s*calc\(var\(--statusbar-height\)\s*\+\s*var\(/,
    );
    // 底栏高度不得在规则里另写一套 px 字面量（否则与 token 漂移）。
    expect(ruleBody(".app-statusbar")).not.toMatch(/min-height:\s*\d+px/);
  });

  it("底栏波浪是 token 化的多层径向弧，且挂在壳层底色上", () => {
    // 参考图底缘有肉眼可辨的丘形起伏；旧实现把多层弧塞进 --statusbar-background，
    // 横向半径远大于条宽导致起伏不可辨，且弧被整条不透明底盖平。
    const root = ruleBody(":root");
    expect(root, "缺少 --shell-bottom-wave token").toMatch(/--shell-bottom-wave:/);
    const wave = /--shell-bottom-wave:([\s\S]*?);/.exec(root)?.[1] ?? "";
    const arcs = wave.match(/radial-gradient\(/g) ?? [];
    expect(arcs.length, "波浪至少 4 层错位径向弧").toBeGreaterThanOrEqual(4);
    // 弧必须落在壳层底缘（y=100%），横向半径量级与纵向相当才能有横向起伏。
    expect(wave).toMatch(/at\s+\d+%\s+100%/);
    expect(wave).not.toMatch(/at\s+[\d.]+%\s+(?:1[2-9]\d|[2-9]\d\d)%/);
    // 波浪叠在 .window-shell 底色之上；底栏是半透明釉面，透出条内弧形明暗。
    expect(ruleBody(".window-shell")).toMatch(
      /background:\s*var\(--shell-bottom-wave\),\s*var\(--shell-background\)\s*;/,
    );
    expect(ruleBody(".app-statusbar")).toMatch(/background:\s*var\(--statusbar-background\)\s*;/);
    expect(root).toMatch(/--statusbar-background:\s*linear-gradient\(/);
  });

  it("状态栏在 index.html 里是 main 的兄弟、位于 window-shell 内", () => {
    const mainClose = html.indexOf("</main>");
    const barIdx = html.indexOf('class="app-statusbar"');
    expect(mainClose).toBeGreaterThanOrEqual(0);
    expect(barIdx).toBeGreaterThan(mainClose);
    expect(html.indexOf("</footer>")).toBeGreaterThan(barIdx);
    // 底栏仍是 .window-shell 的子元素（在对话框之前闭合）。
    const shellClose = html.indexOf('<dialog id="auth-dialog"');
    expect(barIdx).toBeLessThan(shellClose);
  });

  it("底栏文案：slogan 静态，「✦」为 aria-hidden 装饰", () => {
    const bar = html.slice(
      html.indexOf('class="app-statusbar"'),
      html.indexOf("</footer>"),
    );
    expect(bar).toContain("更智能的账号管理体验");
    expect(bar).toMatch(/aria-hidden="true">✦/);
    expect(bar).toContain('id="app-version"');
  });

  it("默认窗口尺寸约 1280×880，且不越过 minWidth / minHeight", () => {
    const confPath = join(dir, "..", "src-tauri", "tauri.conf.json");
    const conf = JSON.parse(readFileSync(confPath, "utf8")) as {
      app: { windows: { label?: string; width?: number; height?: number; minWidth?: number; minHeight?: number }[] };
    };
    const main =
      conf.app.windows.find((window) => window.label === "main") ?? conf.app.windows[0];
    expect(main.width).toBe(1280);
    expect(main.height).toBe(880);
    expect(main.width).toBeGreaterThanOrEqual(main.minWidth!);
    expect(main.height).toBeGreaterThanOrEqual(main.minHeight!);
  });
});

describe("账号状态汇总条（issue #134）", () => {
  it("横幅是圆角浅底条，四档 tone 类名对应四档底色 token", () => {
    const banner = ruleBody(".account-status-banner");
    expect(banner).toMatch(/display:\s*flex\s*;/);
    expect(banner).toMatch(/border-radius:\s*var\(--radius-md\)\s*;/);
    for (const tone of ["ok", "warn", "high", "full"] as const) {
      const rule = ruleBody(`.account-status-banner--${tone}`);
      expect(rule, `${tone} 底色须引用 :root token`).toMatch(
        new RegExp(`background:\\s*var\\(--status-${tone}-bg\\)`),
      );
      expect(rule).toMatch(new RegExp(`color:\\s*var\\(--status-${tone}-title\\)`));
    }
    // 状态色语义复用四档 token：标题 / 说明底色都在 :root 声明，规则里不写死颜色。
    expect(ruleBody(":root")).toMatch(/--status-ok-bg:\s*#e2f5ee\s*;/);
    expect(ruleBody(":root")).toMatch(/--status-ok-title:\s*#0f6b34\s*;/);
  });

  it("容器是当前账号卡内槽位，默认隐藏并只在有当前账号时由 main.ts 填充", () => {
    const bannerIdx = html.indexOf('id="current-status-banner"');
    expect(bannerIdx).toBeGreaterThan(html.indexOf('id="current-quota-panel"'));
    expect(html.slice(bannerIdx, bannerIdx + 120)).toContain("hidden");
    const main = readFileSync(join(dir, "main.ts"), "utf8");
    expect(main).toContain("paintCurrentStatus(");
    expect(main).toContain("accountStatusView(");
  });

  it("汇总条不进 live region（每次快照重涂，避免盖住用户操作公告）", () => {
    const bannerIdx = html.indexOf('id="current-status-banner"');
    expect(html.slice(bannerIdx, bannerIdx + 120)).not.toContain("aria-live");
  });

  it("状态图标颜色只在 styles.css 决定：圆盘 currentColor、图形 --text-inverse", () => {
    expect(ruleBody(".account-status-plate")).toMatch(/fill:\s*currentColor\s*;/);
    const glyph = ruleBody(".account-status-glyph");
    expect(glyph).toMatch(/stroke:\s*var\(--text-inverse\)\s*;/);
    expect(glyph).toMatch(/fill:\s*none\s*;/);
    // 图标源不写死颜色 / 不引图标库；四个状态各一个圆盘，差异只在内联 path。
    const icons = readFileSync(join(dir, "icons.ts"), "utf8");
    const statusSource = icons.slice(icons.indexOf("statusBadgeIcon"));
    expect(statusSource).not.toMatch(/#[0-9a-fA-F]{3,8}\b/);
    expect(statusSource).not.toMatch(/\brgba?\(/i);
    // 不写死 stroke/fill 颜色属性（颜色全交给上面两条 CSS 规则）。
    expect(statusSource).not.toMatch(/stroke="(?!none)/);
    expect(statusSource).not.toMatch(/fill="#/);
    // 圆盘 class 只在共享模板里出现一次（注释里另有一处引用不算），四态复用同一外层。
    const plateCount = (icons.match(/class="account-status-plate"/g) ?? []).length;
    expect(plateCount).toBe(1);
    for (const state of ["STATUS_OK_ICON", "STATUS_URGENT_ICON", "STATUS_RENEWAL_ICON", "STATUS_FAILED_ICON"]) {
      expect(icons).toContain(`export const ${state} = statusBadgeIcon(`);
    }
    // 四态图形互不相同：每个导出各带独立 path。
    const paths = (statusSource.match(/d="M/g) ?? []).length;
    expect(paths).toBeGreaterThanOrEqual(5);
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
