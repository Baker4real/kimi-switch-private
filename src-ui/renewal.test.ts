import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { renewalSwitchView } from "./renewal";

const html = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "index.html"),
  "utf8",
);
const settingsPage = html.slice(html.indexOf('id="page-settings"'));

describe("renewalSwitchView", () => {
  it("开态映射 checked 与开启 toast", () => {
    expect(renewalSwitchView(true)).toEqual({
      checked: true,
      toast: "已开启当前账号主动续期",
    });
  });

  it("关态映射 unchecked 与关闭 toast", () => {
    expect(renewalSwitchView(false)).toEqual({
      checked: false,
      toast: "已关闭当前账号主动续期",
    });
  });
});

describe("设置页「当前账号主动续期」开关（issue #42）", () => {
  it("开关出现在设置页且默认关", () => {
    expect(settingsPage).toContain('id="renewal-toggle"');
    expect(settingsPage).toMatch(/role="switch"/);
    // 静态首帧即关态；权威值由快照涂刷覆盖。
    expect(settingsPage).toMatch(/id="renewal-toggle"[^>]*aria-checked="false"/);
  });

  it("开关旁带 ADR 0003 要求的风险说明", () => {
    expect(settingsPage).toContain("当前账号主动续期");
    expect(settingsPage).toContain("极端情况下官方会话可能要求重新登录");
  });
});
