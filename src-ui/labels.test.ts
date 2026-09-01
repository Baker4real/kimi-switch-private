import { describe, expect, it } from "vitest";

import {
  accountCardTitle,
  accountDisplayName,
  deviceAuthBusyLabel,
  parseAliasInput,
  renameButtonHtml,
  showRenameButton,
} from "./labels";

describe("accountDisplayName", () => {
  it("使用后端下发的显示名", () => {
    expect(accountDisplayName({ display_name: "Forever" }, "acc-1")).toBe("Forever");
  });

  it("行缺失或显示名为空时回退账号 id", () => {
    expect(accountDisplayName({ display_name: "" }, "acc-1")).toBe("acc-1");
    expect(accountDisplayName({ display_name: "   " }, "acc-1")).toBe("acc-1");
    expect(accountDisplayName(null, "acc-1")).toBe("acc-1");
    expect(accountDisplayName(undefined, "acc-1")).toBe("acc-1");
  });
});

describe("accountCardTitle", () => {
  it("卡片标题用显示名，当前账号加标记", () => {
    expect(accountCardTitle({ display_name: "Forever", active: false })).toBe("Forever");
    expect(accountCardTitle({ display_name: "Forever", active: true })).toBe("Forever · 当前");
  });
});

describe("rename button visibility", () => {
  it("官方昵称已知时隐藏重命名", () => {
    expect(showRenameButton(true)).toBe(false);
    expect(
      renameButtonHtml({ id: "acc-1", official_nickname_known: true }, false),
    ).toBe("");
  });

  it("官方昵称未知时保留重命名按钮", () => {
    expect(showRenameButton(false)).toBe(true);
    expect(
      renameButtonHtml({ id: "acc-1", official_nickname_known: false }, false),
    ).toContain('data-rename="acc-1"');
    expect(
      renameButtonHtml({ id: "acc-1", official_nickname_known: false }, false),
    ).toContain(">重命名</button>");
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
