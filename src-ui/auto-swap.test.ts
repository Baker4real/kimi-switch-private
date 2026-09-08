import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  autoSwapOptOutSwitchHtml,
  autoSwapOptOutSwitchView,
  autoSwapPreferLowerTierSwitchView,
  autoSwapResidentLowerTierSwitchView,
  autoSwapSwitchView,
  silentQuotaMessage,
} from "./auto-swap";

const html = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "index.html"),
  "utf8",
);
const libraryPage = html.slice(
  html.indexOf('id="page-library"'),
  html.indexOf('id="page-settings"'),
);

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
  it("按 kind 保留自动切换成功 / 无候选 / 失败，文案只作钉版", () => {
    const success = {
      text: "已自动切换到 工作机",
      tone: "ok" as const,
      kind: "auto_swap_success" as const,
    };
    const empty = {
      text: "没有可自动切换的账号",
      tone: "err" as const,
      kind: "auto_swap_no_candidate" as const,
    };
    const failed = {
      text: "自动切换失败",
      tone: "err" as const,
      kind: "auto_swap_failed" as const,
    };
    expect(silentQuotaMessage(success)).toEqual(success);
    expect(silentQuotaMessage(empty)).toEqual(empty);
    expect(silentQuotaMessage(failed)).toEqual(failed);
  });

  it("普通 toast 即使文案像自动切换也丢掉", () => {
    expect(
      silentQuotaMessage({
        text: "已自动切换到 工作机",
        tone: "ok",
        kind: "generic",
      }),
    ).toBeNull();
  });

  it("丢掉刚刚更新过和 null", () => {
    expect(
      silentQuotaMessage({ text: "刚刚更新过", tone: "info", kind: "generic" }),
    ).toBeNull();
    expect(silentQuotaMessage(null)).toBeNull();
  });
});

describe("账号库页「额度耗尽自动切换」开关（issue #68）", () => {
  it("开关出现在账号库页且默认关", () => {
    expect(libraryPage).toContain('id="auto-swap-toggle"');
    expect(libraryPage).toMatch(/id="auto-swap-toggle"[^>]*role="switch"/);
    // 静态首帧即关态；权威值由快照涂刷覆盖。
    expect(libraryPage).toMatch(/id="auto-swap-toggle"[^>]*aria-checked="false"/);
  });

  it("开关旁带阈值切换说明，不写 daemon / 403", () => {
    expect(libraryPage).toContain("额度耗尽自动切换");
    expect(libraryPage).toContain("当前账号任一额度窗口达到阈值时，自动切到合格停泊账号（默认按已用比例升序）。默认关闭。");
    expect(libraryPage).not.toContain("剩余最多");
    expect(libraryPage).not.toContain("daemon");
    expect(libraryPage).not.toContain("403");
  });
});

describe("autoSwapPreferLowerTierSwitchView", () => {
  it("总开关关时子开关置灰，checked 仍反映持久化值", () => {
    expect(autoSwapPreferLowerTierSwitchView(false, false)).toEqual({
      checked: false,
      disabled: true,
      toast: "已关闭自动切换时优先低档位账号",
    });
    expect(autoSwapPreferLowerTierSwitchView(false, true)).toEqual({
      checked: true,
      disabled: true,
      toast: "已开启自动切换时优先低档位账号",
    });
  });

  it("总开关开时子开关可点，checked 反映持久化值", () => {
    expect(autoSwapPreferLowerTierSwitchView(true, false)).toEqual({
      checked: false,
      disabled: false,
      toast: "已关闭自动切换时优先低档位账号",
    });
    expect(autoSwapPreferLowerTierSwitchView(true, true)).toEqual({
      checked: true,
      disabled: false,
      toast: "已开启自动切换时优先低档位账号",
    });
  });
});

describe("账号库页「自动切换时优先低档位账号」子开关（issue #71）", () => {
  it("紧挨总开关下方，role=switch，静态关且 disabled", () => {
    expect(libraryPage).toContain('id="auto-swap-prefer-lower-tier-toggle"');
    expect(libraryPage).toMatch(
      /id="auto-swap-prefer-lower-tier-toggle"[^>]*role="switch"/,
    );
    expect(libraryPage).toMatch(
      /id="auto-swap-prefer-lower-tier-toggle"[^>]*aria-checked="false"/,
    );
    expect(libraryPage).toMatch(
      /id="auto-swap-prefer-lower-tier-toggle"[^>]*\sdisabled/,
    );
    const master = libraryPage.indexOf('id="auto-swap-toggle"');
    const sub = libraryPage.indexOf('id="auto-swap-prefer-lower-tier-toggle"');
    expect(sub).toBeGreaterThan(master);
  });

  it("文案为优先低档位，不写套餐或 plan", () => {
    expect(libraryPage).toContain("自动切换时优先低档位账号");
    const start = libraryPage.indexOf("auto-swap-prefer-lower-tier");
    const snippet = libraryPage.slice(start, start + 900);
    expect(snippet).not.toContain("套餐");
    expect(snippet).not.toMatch(/\bplan\b/i);
  });
});

describe("autoSwapResidentLowerTierSwitchView", () => {
  it("总开关关时子开关置灰，checked 仍反映持久化值", () => {
    expect(autoSwapResidentLowerTierSwitchView(false, false)).toEqual({
      checked: false,
      disabled: true,
      toast: "已关闭额度恢复后回驻低档账号",
    });
    expect(autoSwapResidentLowerTierSwitchView(false, true)).toEqual({
      checked: true,
      disabled: true,
      toast: "已开启额度恢复后回驻低档账号",
    });
  });

  it("总开关开时子开关可点，checked 反映持久化值", () => {
    expect(autoSwapResidentLowerTierSwitchView(true, false)).toEqual({
      checked: false,
      disabled: false,
      toast: "已关闭额度恢复后回驻低档账号",
    });
    expect(autoSwapResidentLowerTierSwitchView(true, true)).toEqual({
      checked: true,
      disabled: false,
      toast: "已开启额度恢复后回驻低档账号",
    });
  });
});

describe("账号库页「额度恢复后回驻低档账号」子开关（issue #95）", () => {
  it("位于优先低档位开关之后，role=switch，静态关且 disabled", () => {
    expect(libraryPage).toContain('id="auto-swap-resident-lower-tier-toggle"');
    expect(libraryPage).toMatch(
      /id="auto-swap-resident-lower-tier-toggle"[^>]*role="switch"/,
    );
    expect(libraryPage).toMatch(
      /id="auto-swap-resident-lower-tier-toggle"[^>]*aria-checked="false"/,
    );
    expect(libraryPage).toMatch(
      /id="auto-swap-resident-lower-tier-toggle"[^>]*\sdisabled/,
    );
    const prefer = libraryPage.indexOf('id="auto-swap-prefer-lower-tier-toggle"');
    const resident = libraryPage.indexOf('id="auto-swap-resident-lower-tier-toggle"');
    expect(resident).toBeGreaterThan(prefer);
  });

  it("文案为回驻低档账号并说明保持期，不写套餐或 plan", () => {
    expect(libraryPage).toContain("额度恢复后回驻低档账号");
    expect(libraryPage).toContain("手动切换后 8 小时内不回驻");
    const start = libraryPage.indexOf("auto-swap-resident-lower-tier");
    const snippet = libraryPage.slice(start, start + 900);
    expect(snippet).not.toContain("套餐");
    expect(snippet).not.toMatch(/\bplan\b/i);
  });
});

describe("autoSwapOptOutSwitchView（issue #112）", () => {
  it("开态映射 checked 与摘出 toast", () => {
    expect(autoSwapOptOutSwitchView(true)).toEqual({
      checked: true,
      toast: "已摘出自动切换",
    });
  });

  it("关态映射 unchecked 与恢复 toast", () => {
    expect(autoSwapOptOutSwitchView(false)).toEqual({
      checked: false,
      toast: "已恢复参与自动切换",
    });
  });
});

describe("账号卡片摘出开关 HTML（issue #112）", () => {
  it("关态可见：role=switch、aria-checked=false、带账号 id", () => {
    const html = autoSwapOptOutSwitchHtml({ id: "acc-1", auto_swap_opt_out: false }, false);
    expect(html).toContain("摘出自动切换");
    expect(html).toMatch(/role="switch"/);
    expect(html).toMatch(/aria-checked="false"/);
    expect(html).toContain('data-optout="acc-1"');
    expect(html).not.toMatch(/\sdisabled/);
  });

  it("开态 aria-checked=true，忙碌时禁用", () => {
    const html = autoSwapOptOutSwitchHtml({ id: "bob", auto_swap_opt_out: true }, true);
    expect(html).toMatch(/aria-checked="true"/);
    expect(html).toContain('data-optout="bob"');
    expect(html).toMatch(/\sdisabled/);
  });

  it("转义账号 id，不把尖括号写进属性", () => {
    const html = autoSwapOptOutSwitchHtml(
      { id: `a"b<c>`, auto_swap_opt_out: false },
      false,
    );
    expect(html).toContain('data-optout="a&quot;b&lt;c&gt;"');
    expect(html).not.toContain('data-optout="a"b<c>"');
  });
});
