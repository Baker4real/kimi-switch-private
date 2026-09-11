import { describe, expect, it } from "vitest";

import {
  accountDisplayName,
  currentAccountChipHtml,
  deviceAuthBusyLabel,
  displayInitial,
  parseAliasInput,
  renameButtonHtml,
  renameOverflowHtml,
  showRenameButton,
} from "./labels";

describe("displayInitial（issue #133 总览头像圆）", () => {
  it("取显示名首字符，中文与英文一致", () => {
    expect(displayInitial("Forever4real")).toBe("F");
    expect(displayInitial("张三")).toBe("张");
  });

  it("代理对按整字取，不截断成半个码位", () => {
    expect(displayInitial("𝓐bc")).toBe("𝓐");
  });

  it("去掉首尾空白后取字符；空串回退 --", () => {
    expect(displayInitial("  Kimi  ")).toBe("K");
    expect(displayInitial("")).toBe("--");
    expect(displayInitial("   ")).toBe("--");
  });
});

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

describe("currentAccountChipHtml", () => {
  it("当前账号渲染色片，停泊账号不渲染", () => {
    expect(currentAccountChipHtml(true)).toContain("profile-current-chip");
    expect(currentAccountChipHtml(true)).toContain("当前");
    expect(currentAccountChipHtml(false)).toBe("");
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
    // disclosure 语义：菜单项是普通按钮，不挂 menu 角色。
    expect(
      renameButtonHtml({ id: "acc-1", official_nickname_known: false }, false),
    ).not.toContain('role="menuitem"');
  });
});

describe("renameOverflowHtml", () => {
  it("官方昵称已知时整颗「…」隐藏", () => {
    expect(renameOverflowHtml({ id: "acc-1", official_nickname_known: true }, false)).toBe("");
  });

  it("官方昵称未知时「…」只收重命名", () => {
    const html = renameOverflowHtml({ id: "acc-1", official_nickname_known: false }, false);
    expect(html).toContain('data-overflow="acc-1"');
    expect(html).toContain('aria-label="更多操作"');
    expect(html).toContain('data-rename="acc-1"');
    expect(html).toContain("重命名");
    // disclosure 语义：只有开合状态与名称，不声明 menu 键盘契约。
    expect(html).toContain('aria-expanded="false"');
    expect(html).not.toContain('role="menu"');
    expect(html).not.toContain('aria-haspopup="menu"');
    // 切换 / 重新授权 / 删除 / 绑定都不进菜单。
    expect(html).not.toContain("data-swap");
    expect(html).not.toContain("data-reauth");
    expect(html).not.toContain("data-delete");
    expect(html).not.toContain("data-bind");
  });

  it("忙碌时「…」开关与菜单项一并禁用", () => {
    const html = renameOverflowHtml({ id: "acc-1", official_nickname_known: false }, true);
    expect(html).toMatch(/data-overflow="acc-1"[^>]* disabled/);
    expect(html).toMatch(/data-rename="acc-1"[^>]* disabled/);
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
