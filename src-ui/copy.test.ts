import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { appVersionLabel } from "./app-version";

const dir = dirname(fileURLToPath(import.meta.url));

/** 用户可见 GUI 源（不含测试、api 字段注释、escape、静默额度）。 */
const productionFiles = [
  "main.ts",
  "credentialToast.ts",
  "membership.ts",
  "membershipToast.ts",
  "webBind.ts",
  "labels.ts",
  "deviceAuth.ts",
  "quota.ts",
  "icons.ts",
  "account-status.ts",
  "auto-swap.ts",
  "store.ts",
  "app-version.ts",
  "library-card.ts",
  "index.html",
] as const;

function loadCorpus(): string {
  return productionFiles
    .map((name) => {
      const text = readFileSync(join(dir, name), "utf8");
      expect(text.length, `${name} 不应为空`).toBeGreaterThan(0);
      return text;
    })
    .join("\n");
}

describe("CONTEXT 术语（issue #86）", () => {
  it("busy 用「正在查询额度」，不用「正在刷新额度」", () => {
    const corpus = loadCorpus();
    expect(corpus).toContain("正在查询额度");
    expect(corpus).not.toContain("正在刷新额度");
  });

  it("账号卡按钮写「重新授权」，不是单独的「授权」", () => {
    const corpus = loadCorpus();
    expect(corpus).toContain("重新授权</button>");
    expect(corpus).not.toContain(">授权</button>");
  });

  it("GUI 概念用「会员到期」；弱提示与 tooltip 不用「有效期」当概念；日期句保留「有效期至」", () => {
    const corpus = loadCorpus();
    expect(corpus).toContain("绑定后可显示会员到期");
    expect(corpus).toContain("绑定网页登录，更新会员到期");
    expect(corpus).toContain("这张卡可显示会员到期");
    expect(corpus).toContain("会员到期未更新");
    expect(corpus).toContain("有效期至");
    expect(corpus).not.toContain("绑定后可显示有效期");
    expect(corpus).not.toContain("刷新会员有效期");
    expect(corpus).not.toContain("会员有效期");
    const withoutDatePhrase = corpus.replaceAll("有效期至", "");
    expect(withoutDatePhrase).not.toContain("有效期");
  });
});

describe("文案与空态（issue #127）", () => {
  it("状态 pill 空闲是「就绪」，加载失败时不得再声称额度健康", () => {
    const corpus = loadCorpus();
    expect(corpus).toContain('id="app-busy-label">就绪</span>');
    expect(corpus).not.toContain("额度就绪");
  });

  it("账号库空态指引当前视图的「添加账号」，并提示可切到总览导入", () => {
    const html = readFileSync(join(dir, "index.html"), "utf8");
    const library = html.slice(
      html.indexOf('id="page-library"'),
      html.indexOf('id="auth-dialog"'),
    );
    expect(library).toContain('id="library-add-button"');
    const emptyStart = library.indexOf('id="accounts-empty"');
    const emptyInnerStart = library.indexOf(">", emptyStart);
    const emptyInnerEnd = library.indexOf("</div>", emptyInnerStart);
    const empty = library.slice(emptyInnerStart + 1, emptyInnerEnd);
    expect(empty).toContain("添加账号");
    expect(empty).toContain("总览");
    expect(empty.indexOf("总览")).toBeLessThan(empty.indexOf("导入"));
    expect(empty).not.toContain("或先在 Kimi Code 登录后点「导入当前账号」");
  });

  it("账号库页眉副标题写收纳与授权，不用效果图营销句", () => {
    const html = readFileSync(join(dir, "index.html"), "utf8");
    const library = html.slice(
      html.indexOf('id="page-library"'),
      html.indexOf('id="auth-dialog"'),
    );
    expect(library).toContain("管理本机收纳的账号，切换与授权都在这里");
    expect(library).not.toContain("畅享无限可能");
    expect(html).not.toContain("简单 · 高效");
  });

  it("总览空态按账号数分支，不再写「请先切换」", () => {
    const corpus = loadCorpus();
    expect(corpus).toContain("请先添加或导入");
    expect(corpus).toContain("请到「账号库」切换");
    expect(corpus).not.toContain("暂无当前账号，请先切换");
  });

  it("凭证恢复 toast 是「凭证已恢复」，不是额度事件", () => {
    const corpus = loadCorpus();
    expect(corpus).toContain("凭证已恢复");
    expect(corpus).not.toContain("额度已恢复");
  });

  it("账号卡开关是「摘出自动切换」，不用效果图的「输出」", () => {
    const corpus = loadCorpus();
    expect(corpus).toContain("摘出自动切换");
    expect(corpus).not.toContain("输出自动切换");
  });

  it("授权对话框两个复制按钮语义可区分", () => {
    const html = readFileSync(join(dir, "index.html"), "utf8");
    expect(html).toMatch(/id="auth-copy-url"[^>]*>复制链接</);
    expect(html).toMatch(/id="auth-copy-code"[^>]*>复制授权码</);
  });

  it("「控制面板」标题改为「账号操作」", () => {
    const corpus = loadCorpus();
    expect(corpus).toContain("账号操作");
    expect(corpus).not.toContain("控制面板");
  });

  it("总览状态卡不与 hero 重复「当前账号」，改为「可切换」", () => {
    const html = readFileSync(join(dir, "index.html"), "utf8");
    const overview = html.slice(
      html.indexOf('id="page-overview"'),
      html.indexOf('id="page-library"'),
    );
    expect(overview).toContain(">当前账号<");
    const grid = overview.slice(
      overview.indexOf("status-grid"),
      overview.indexOf("control-deck"),
    );
    expect(grid).toContain("可切换");
    expect(grid).not.toContain("当前账号");
    expect(grid).toContain('id="metric-current"');
  });

  it("额度相关用户可见冒号用全角", () => {
    const corpus = loadCorpus();
    expect(corpus).toContain("额度查询失败：");
    expect(corpus).toContain("额度：n/a");
    expect(corpus).not.toContain("额度查询失败: ");
    expect(corpus).not.toContain("额度: n/a");
  });
});

describe("总览页 1:1 复刻结构（issue #133）", () => {
  const html = readFileSync(join(dir, "index.html"), "utf8");
  const overview = html.slice(
    html.indexOf('id="page-overview"'),
    html.indexOf('id="page-library"'),
  );

  it("左卡：中文标题 + 英文 kicker + 装饰图标块", () => {
    expect(overview).toContain('class="current-account-mark"');
    expect(overview).toContain("CURRENT ACCOUNT");
    expect(overview).toMatch(/<h2[^>]*class="current-account-heading"/);
    expect(overview).toContain("当前账号");
  });

  it("左卡：头像圆、显示名、档位、会员徽章、额度面板槽位", () => {
    expect(overview).toMatch(/id="current-avatar"[^>]*class="current-avatar"[^>]*aria-hidden="true"/);
    expect(overview).toContain('id="current-title"');
    expect(overview).toContain('id="current-plan"');
    expect(overview).toMatch(/id="current-membership-badge"[^>]*class="[^"]*membership-badge[^"]*\bhidden\b/);
    expect(overview).toContain("会员账号");
    expect(overview).toContain('id="current-quota-panel"');
  });

  it("三统计卡：彩色图标 + 标签 + 大数字 + 一行说明，数字仍在既有 id 上", () => {
    for (const [mark, metric] of [
      ["status-card-mark--switch", "metric-current"],
      ["status-card-mark--total", "metric-total"],
      ["status-card-mark--urgent", "metric-urgent"],
    ] as const) {
      expect(overview).toContain(mark);
      expect(overview).toContain(`id="${metric}"`);
    }
    expect(overview).toContain("可切换账号");
    expect(overview).toContain("账号总数");
    expect(overview).toContain("额度告急");
    expect(overview).toContain("可立即切换使用");
    expect(overview).toContain("已配置的账号数量");
    expect(overview).toContain("需要关注的账号");
    // 统计卡数字不得进 live region（announce.test 另有独立断言）。
    expect(overview).toMatch(/id="metric-current"[^>]*class="status-card-value"/);
  });

  it("操作卡：标题 + 副标题 + 三个带图标说明的按钮，id 与行为入口不变", () => {
    expect(overview).toContain("账号操作");
    expect(overview).toContain("管理你的 Kimi 账号，轻松切换，提升效率");
    expect(overview).toContain('id="add-account-button"');
    expect(overview).toContain('id="import-button"');
    expect(overview).toContain('id="refresh-button"');
    expect(overview).toContain("添加账号");
    expect(overview).toContain("导入当前账号");
    expect(overview).toContain("刷新");
    expect(overview).toContain("添加新的 Kimi 账号");
    expect(overview).toContain("快速导入当前登录的账号");
    expect(overview).toContain("获取最新账号状态");
    // 三个按钮各自带内联 SVG 图标，无图标库依赖。
    expect(overview.match(/class="control-action-mark"/g) ?? []).toHaveLength(3);
  });

  it("「添加账号」是蓝色渐变主按钮，其余两个为白卡片按钮", () => {
    expect(overview).toMatch(/id="add-account-button"[^>]*class="control-action control-action--primary"/);
    expect(overview).toMatch(/id="import-button"[^>]*class="control-action"/);
    expect(overview).toMatch(/id="refresh-button"[^>]*class="control-action"/);
  });

  it("图标全部内联 SVG，不引入图标库", () => {
    const corpus = loadCorpus();
    expect(corpus).not.toMatch(/lucide|heroicons|font-awesome|feather-icons/i);
    expect(overview).toContain("<svg");
  });

  it("账号状态汇总条：正常态与三类异常态文案齐备，容器只留槽位", () => {
    const corpus = loadCorpus();
    // 正常态（参考图文案，issue #134 起落地）。
    expect(corpus).toContain("账号状态正常");
    expect(corpus).toContain("一切运行良好，继续享受 AI 服务吧！");
    // 三类异常态同位置换色换文案。
    expect(corpus).toContain("账号额度告急");
    expect(corpus).toContain("部分额度窗口即将耗尽，建议切换账号");
    expect(corpus).toContain("凭证待续期");
    expect(corpus).toContain("在 Kimi CLI 里发条消息即可恢复，或重新授权");
    expect(corpus).toContain("额度查询失败");
    expect(corpus).toContain("暂时无法获取额度，稍后重试");
    // index.html 只留容器槽位，文案不进静态 HTML（由 account-status.ts 派生）。
    expect(overview).toContain('id="current-status-banner"');
    expect(overview).not.toContain("账号状态正常");
  });
});

describe("壳层文案（issue #132）", () => {
  function statusBar(): string {
    const html = readFileSync(join(dir, "index.html"), "utf8");
    return html.slice(html.indexOf('class="app-statusbar"'), html.indexOf("</footer>"));
  }

  it("状态栏 slogan 是「更智能的账号管理体验」，「✦」只是装饰前缀", () => {
    const bar = statusBar();
    expect(bar).toContain("更智能的账号管理体验");
    // 「✦」独立成 aria-hidden 装饰 span，不并进 slogan 文本节点。
    expect(bar).toMatch(/class="app-statusbar-sparkle" aria-hidden="true">✦<\/span>/);
    expect(bar).not.toContain("✦更智能");
  });

  it("版本号展示为「Kimi 换号器 v<版本>」，由注入常量驱动", () => {
    const bar = statusBar();
    expect(bar).toContain('id="app-version"');
    expect(bar).not.toContain("v1.0.0");
    expect(appVersionLabel("1.4.17")).toBe("Kimi 换号器 v1.4.17");
    const main = readFileSync(join(dir, "main.ts"), "utf8");
    expect(main).toContain("appVersionLabel(injectedAppVersion)");
    expect(main).toContain('required<HTMLElement>("app-version")');
  });

  it("busy 状态 pill 的 aria-live 与「就绪」语义不变", () => {
    const html = readFileSync(join(dir, "index.html"), "utf8");
    const pill = html.slice(html.indexOf('id="app-busy"'), html.indexOf('id="app-busy-label"'));
    expect(pill).toContain('aria-live="polite"');
    expect(html).toContain('id="app-busy-label">就绪</span>');
  });
});
