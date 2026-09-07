import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

const dir = dirname(fileURLToPath(import.meta.url));

/** 用户可见 GUI 源（不含测试、api 字段注释、theme/escape、静默额度）。 */
const productionFiles = [
  "main.ts",
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
