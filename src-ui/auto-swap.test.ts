import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { autoSwapSwitchView, silentQuotaMessage } from "./auto-swap";

const html = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "index.html"),
  "utf8",
);
const settingsPage = html.slice(html.indexOf('id="page-settings"'));

describe("autoSwapSwitchView", () => {
  it("开态映射 checked 与开启 toast", () => {
    expect(autoSwapSwitchView(true)).toEqual({
      checked: true,
      toast: "已开启额度耗尽自动切换",
    });
  });

  it("关态映射 unchecked 与关闭 toast", () => {
    expect(autoSwapSwitchView(false)).toEqual({
      checked: false,
      toast: "已关闭额度耗尽自动切换",
    });
  });
});

describe("silentQuotaMessage", () => {
  it("保留自动切换成功 / 无候选 / 失败", () => {
    expect(
      silentQuotaMessage({ text: "已自动切换到 工作机", tone: "ok" }),
    ).toEqual({ text: "已自动切换到 工作机", tone: "ok" });
    expect(
      silentQuotaMessage({ text: "没有可自动切换的账号", tone: "err" }),
    ).toEqual({ text: "没有可自动切换的账号", tone: "err" });
    expect(silentQuotaMessage({ text: "自动切换失败", tone: "err" })).toEqual({
      text: "自动切换失败",
      tone: "err",
    });
  });

  it("丢掉刚刚更新过和 null", () => {
    expect(silentQuotaMessage({ text: "刚刚更新过", tone: "info" })).toBeNull();
    expect(silentQuotaMessage(null)).toBeNull();
  });
});

describe("设置页「额度耗尽自动切换」开关（issue #68）", () => {
  it("开关出现在设置页且默认关", () => {
    expect(settingsPage).toContain('id="auto-swap-toggle"');
    expect(settingsPage).toMatch(/id="auto-swap-toggle"[^>]*role="switch"/);
    // 静态首帧即关态；权威值由快照涂刷覆盖。
    expect(settingsPage).toMatch(/id="auto-swap-toggle"[^>]*aria-checked="false"/);
  });

  it("开关旁带阈值切换说明，不写 daemon / 403", () => {
    expect(settingsPage).toContain("额度耗尽自动切换");
    expect(settingsPage).toContain("当前账号任一额度窗口达到阈值时，自动切到剩余最多的停泊账号。默认关闭。");
    expect(settingsPage).not.toContain("daemon");
    expect(settingsPage).not.toContain("403");
  });
});
