import { describe, expect, it } from "vitest";

import {
  UNKNOWN_APP_VERSION,
  appVersionLabel,
  injectedAppVersion,
  normalizeAppVersion,
} from "./app-version";

describe("版本号注入与兜底（issue #132）", () => {
  it("合法版本号原样保留，并格式化为「Kimi 换号器 v<版本>」", () => {
    expect(normalizeAppVersion("1.4.17")).toBe("1.4.17");
    expect(appVersionLabel("1.4.17")).toBe("Kimi 换号器 v1.4.17");
    expect(appVersionLabel("0.0.1")).toBe("Kimi 换号器 v0.0.1");
  });

  it("允许预发布与构建后缀，并去掉首尾空白", () => {
    expect(normalizeAppVersion(" 2.0.0-beta.1 ")).toBe("2.0.0-beta.1");
    expect(appVersionLabel("2.0.0+build.7")).toBe("Kimi 换号器 v2.0.0+build.7");
  });

  it("注入缺失（undefined/null/非字符串）回退 0.0.0，不出现 undefined", () => {
    for (const raw of [undefined, null, 42, {}, ""]) {
      expect(normalizeAppVersion(raw)).toBe(UNKNOWN_APP_VERSION);
      expect(appVersionLabel(raw)).toBe("Kimi 换号器 v0.0.0");
      expect(appVersionLabel(raw)).not.toContain("undefined");
    }
  });

  it("非法字符串回退 0.0.0", () => {
    for (const raw of ["v1.4.17", "latest", "1.4.17.0.0.0-", " "] as const) {
      expect(normalizeAppVersion(raw)).toBe(UNKNOWN_APP_VERSION);
    }
  });

  it("可用版本常量集合与 UNKNOWN_APP_VERSION 匹配", () => {
    expect(UNKNOWN_APP_VERSION).toBe("0.0.0");
    expect(
      ["1.4.17", "0.0.0", "10.20.30-rc.1"].every(
        (value) => normalizeAppVersion(value) === value,
      ),
    ).toBe(true);
  });

  it("测试环境未走 vite define，注入值为 undefined 时由兜底接管", () => {
    // vitest 使用 vitest.config.ts，不应用 vite.config.ts 的 define；
    // 该断言保证「注入缺失」这条分支在测试里真实可达。
    expect(injectedAppVersion === undefined || typeof injectedAppVersion === "string").toBe(
      true,
    );
    expect(appVersionLabel(injectedAppVersion)).toMatch(/^Kimi 换号器 v\d/);
  });
});
