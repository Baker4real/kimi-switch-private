import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

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
  "store.ts",
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
    expect(corpus).toContain(">重新授权</button>");
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
