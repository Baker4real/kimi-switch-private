import { describe, expect, it } from "vitest";

import { parseWebToken, webBindCopy, WEB_BIND_CONSOLE_URL } from "./webBind";

describe("WEB_BIND_CONSOLE_URL（issue #51）", () => {
  it("固定指向官方控制台页（token 来源页），不是订阅页或第三方域名", () => {
    expect(WEB_BIND_CONSOLE_URL).toBe("https://www.kimi.com/code/console");
  });
});

describe("parseWebToken", () => {
  it("去首尾空白与换行（粘贴常见形态）", () => {
    expect(parseWebToken("  eyJhbGciOiJ9.sig\n")).toBe("eyJhbGciOiJ9.sig");
  });

  it("空白输入返回 null，不发起绑定", () => {
    expect(parseWebToken("")).toBeNull();
    expect(parseWebToken("   \n\t ")).toBeNull();
  });
});

describe("webBindCopy", () => {
  it("含账号显示名与粘贴来源说明，帮助核对绑定的卡", () => {
    const copy = webBindCopy("工作机");
    expect(copy).toContain("工作机");
    expect(copy).toContain("access_token");
    expect(copy).toContain("www.kimi.com");
    expect(copy).toContain("会员到期");
    expect(copy).toContain("这张卡可显示会员到期");
    expect(copy).not.toContain("会员有效期");
    expect(copy).not.toContain("有效期");
  });

  it("拆成编号短句，便于按步操作", () => {
    const copy = webBindCopy("工作机");
    expect(copy).toContain("1. 绑定「工作机」后这张卡可显示会员到期。");
    expect(copy).toContain("2. 打开 kimi.com 控制台并登录同一账号。");
    expect(copy).toContain("3. ");
    expect(copy).toContain("access_token");
    expect(copy).toContain("4. 粘贴到下面。");
    expect(copy.indexOf("1. ")).toBeLessThan(copy.indexOf("2. "));
    expect(copy.indexOf("2. ")).toBeLessThan(copy.indexOf("3. "));
    expect(copy.indexOf("3. ")).toBeLessThan(copy.indexOf("4. "));
  });
});
