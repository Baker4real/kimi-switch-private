import { describe, expect, it } from "vitest";

import { accountDisplayName, deviceAuthBusyLabel, parseAliasInput } from "./labels";

describe("accountDisplayName", () => {
  it("有别名时显示别名", () => {
    expect(accountDisplayName("工作机", "acc-1")).toBe("工作机");
  });

  it("无别名时退回账号 id", () => {
    expect(accountDisplayName("", "acc-1")).toBe("acc-1");
    expect(accountDisplayName("   ", "acc-1")).toBe("acc-1");
    expect(accountDisplayName(null, "acc-1")).toBe("acc-1");
    expect(accountDisplayName(undefined, "acc-1")).toBe("acc-1");
  });
});

describe("deviceAuthBusyLabel", () => {
  it("链接到达前后胶囊文案不同", () => {
    expect(deviceAuthBusyLabel("fetching-link")).toBe("正在获取授权链接…");
    expect(deviceAuthBusyLabel("waiting-browser")).toBe("等待浏览器授权…");
  });
});

describe("parseAliasInput", () => {
  it("拒绝空别名与纯空白", () => {
    expect(parseAliasInput("")).toBeNull();
    expect(parseAliasInput(" \t ")).toBeNull();
  });

  it("提交前 trim", () => {
    expect(parseAliasInput("  工作机  ")).toBe("工作机");
  });
});
