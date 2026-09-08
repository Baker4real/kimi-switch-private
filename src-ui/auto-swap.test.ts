import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  autoSwapNestedPanelToggle,
  autoSwapNestedPanelView,
  autoSwapNestedStatusText,
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

describe("autoSwapNestedPanelView 开合真值表（issue #117）", () => {
  it("override=open 恒展开，且把 opened 闩住", () => {
    expect(autoSwapNestedPanelView("open", false, false, false)).toEqual({
      expanded: true,
      opened: true,
    });
    expect(autoSwapNestedPanelView("open", true, false, false)).toEqual({
      expanded: true,
      opened: true,
    });
  });

  it("override=closed 恒收起，且不清 opened 闩", () => {
    expect(autoSwapNestedPanelView("closed", false, false, false)).toEqual({
      expanded: false,
      opened: false,
    });
    // 用户收起后，即使子开关为开、甚至本会话曾展开过，也不得撑开。
    expect(autoSwapNestedPanelView("closed", true, true, true)).toEqual({
      expanded: false,
      opened: true,
    });
  });

  it("override=null 时按 opened 或任一子开关自动展开", () => {
    expect(autoSwapNestedPanelView(null, false, false, false)).toEqual({
      expanded: false,
      opened: false,
    });
    expect(autoSwapNestedPanelView(null, false, true, false)).toEqual({
      expanded: true,
      opened: true,
    });
    expect(autoSwapNestedPanelView(null, false, false, true)).toEqual({
      expanded: true,
      opened: true,
    });
    expect(autoSwapNestedPanelView(null, false, true, true)).toEqual({
      expanded: true,
      opened: true,
    });
    // 本会话一旦展开过，关掉最后一个子开关也不自动收起。
    expect(autoSwapNestedPanelView(null, true, false, false)).toEqual({
      expanded: true,
      opened: true,
    });
  });
});

describe("autoSwapNestedPanelToggle 用户点击（issue #117）", () => {
  it("未点过且都关时点击展开，并记 open / opened", () => {
    expect(autoSwapNestedPanelToggle(null, false, false, false)).toEqual({
      expanded: true,
      override: "open",
      opened: true,
    });
  });

  it("收起态点击展开（override 为 closed 或 null）", () => {
    expect(autoSwapNestedPanelToggle("closed", false, false, false)).toEqual({
      expanded: true,
      override: "open",
      opened: true,
    });
  });

  it("展开态点击收起，override 写 closed 且保留 opened", () => {
    expect(autoSwapNestedPanelToggle("open", true, false, false)).toEqual({
      expanded: false,
      override: "closed",
      opened: true,
    });
  });

  it("子开关开导致自动展开后点击收起，仍是收起并保留 opened", () => {
    expect(autoSwapNestedPanelToggle(null, false, true, false)).toEqual({
      expanded: false,
      override: "closed",
      opened: true,
    });
  });
});

describe("autoSwapNestedStatusText 状态行文案（issue #117）", () => {
  it("都关不写；只开优先低档 / 只开回驻 / 都开各自钉版", () => {
    expect(autoSwapNestedStatusText(false, false)).toBe("");
    expect(autoSwapNestedStatusText(true, false)).toBe("优先低档已开");
    expect(autoSwapNestedStatusText(false, true)).toBe("回驻已开");
    expect(autoSwapNestedStatusText(true, true)).toBe("优先低档、回驻已开");
  });

  it("文案不含禁用词", () => {
    for (const prefer of [false, true]) {
      for (const resident of [false, true]) {
        const text = autoSwapNestedStatusText(prefer, resident);
        expect(text).not.toContain("切回");
        expect(text).not.toContain("下拉");
        expect(text).not.toContain("高级");
        expect(text).not.toContain("更多");
        expect(text).not.toContain("套餐");
        expect(text).not.toMatch(/plan/i);
      }
    }
  });
});

describe("账号库页「优先低档与回驻」折叠标题与面板（issue #116 / #117）", () => {
  it("折叠标题为 button，带 aria-expanded=false 与 aria-controls，caret aria-hidden", () => {
    expect(libraryPage).toContain('id="auto-swap-nested-toggle"');
    expect(libraryPage).toMatch(/id="auto-swap-nested-toggle"[^>]*type="button"/);
    expect(libraryPage).toMatch(/id="auto-swap-nested-toggle"[^>]*aria-expanded="false"/);
    expect(libraryPage).toMatch(
      /id="auto-swap-nested-toggle"[^>]*aria-controls="auto-swap-nested-panel"/,
    );
    expect(libraryPage).toContain("优先低档与回驻");
    const toggle = libraryPage.indexOf('id="auto-swap-nested-toggle"');
    const snippet = libraryPage.slice(toggle, toggle + 600);
    expect(snippet).toContain('class="settings-disclosure-caret" aria-hidden="true"');
    // 标题行右侧是 caret 不是 switch：标题按钮自身不声明 role=switch。
    expect(snippet).not.toMatch(/id="auto-swap-nested-toggle"[^>]*role="switch"/);
  });

  it("状态行位于折叠标题行内（不在会被 hidden 的面板里），静态首帧为空且隐藏", () => {
    const toggle = libraryPage.indexOf('id="auto-swap-nested-toggle"');
    const status = libraryPage.indexOf('id="auto-swap-nested-status"');
    const panel = libraryPage.indexOf('id="auto-swap-nested-panel"');
    expect(status).toBeGreaterThan(toggle);
    // 必须落在面板之前：面板收起时 class="hidden"，状态行要仍看得见。
    expect(status).toBeLessThan(panel);
    const snippet = libraryPage.slice(status, status + 120);
    expect(snippet).toMatch(/class="[^"]*\bhidden\b[^"]*"/);
    // 静态首帧都关：标题不挂 aria-describedby，等 paint 层按真值补。
    expect(libraryPage).not.toMatch(/id="auto-swap-nested-toggle"[^>]*aria-describedby/);
  });

  it("静态首帧面板收起且对视觉/辅助技术隐藏，节点仍在文档里", () => {
    expect(libraryPage).toMatch(
      /id="auto-swap-nested-panel"[^>]*class="[^"]*\bhidden\b[^"]*"/,
    );
    // 面板容器存在（未卸节点），两个子开关仍在其内。
    const panel = libraryPage.indexOf('id="auto-swap-nested-panel"');
    const prefer = libraryPage.indexOf('id="auto-swap-prefer-lower-tier-toggle"');
    const resident = libraryPage.indexOf('id="auto-swap-resident-lower-tier-toggle"');
    expect(panel).toBeGreaterThan(-1);
    expect(prefer).toBeGreaterThan(panel);
    expect(resident).toBeGreaterThan(prefer);
  });

  it("折叠标题位于总开关之后，不用原生 details / select", () => {
    const master = libraryPage.indexOf('id="auto-swap-toggle"');
    const toggle = libraryPage.indexOf('id="auto-swap-nested-toggle"');
    expect(toggle).toBeGreaterThan(master);
    expect(libraryPage).not.toContain("<details");
    expect(libraryPage).not.toContain("<select");
  });

  it("主动续期仍在折叠面板与总开关之外", () => {
    const renewal = libraryPage.indexOf('id="renewal-toggle"');
    const master = libraryPage.indexOf('id="auto-swap-toggle"');
    const panel = libraryPage.indexOf('id="auto-swap-nested-panel"');
    expect(renewal).toBeLessThan(master);
    expect(master).toBeLessThan(panel);
  });
});

describe("main.ts 折叠面板会话接线（issue #117）", () => {
  const main = readFileSync(
    join(dirname(fileURLToPath(import.meta.url)), "main.ts"),
    "utf8",
  );

  it("override 与 opened 只作模块级会话变量，不写 localStorage / config.toml", () => {
    expect(main).toContain("let autoSwapNestedOverride: AutoSwapNestedPanelOverride = null;");
    expect(main).toContain("let autoSwapNestedOpened = false;");
    expect(main).not.toMatch(/localStorage[^;]*autoSwapNested/);
    expect(main).not.toContain("config.toml");
  });

  it("paintSnapshot 在子开关之后接折叠 paint，自动展开读子开关真值", () => {
    const prefer = main.indexOf("paintAutoSwapPreferLowerTier(next.auto_swap,");
    const resident = main.indexOf("paintAutoSwapResidentLowerTier(next.auto_swap,");
    const nested = main.indexOf("paintAutoSwapNestedPanel(next.auto_swap_prefer_lower_tier");
    expect(prefer).toBeGreaterThan(-1);
    expect(resident).toBeGreaterThan(prefer);
    expect(nested).toBeGreaterThan(resident);
  });

  it("折叠标题 click 走纯函数开合，不弹 toast、不在 BUSY_BUTTON_IDS", () => {
    const start = main.indexOf('required<HTMLButtonElement>("auto-swap-nested-toggle").addEventListener');
    expect(start).toBeGreaterThan(-1);
    const handler = main.slice(start, start + 700);
    expect(handler).toContain("autoSwapNestedPanelToggle(");
    expect(handler).toContain("autoSwapNestedOpened = view.opened;");
    expect(handler).toContain("paintAutoSwapNestedPanel(");
    expect(handler).not.toContain("showToast");
    const busyList = main.slice(
      main.indexOf("const BUSY_BUTTON_IDS = ["),
      main.indexOf("] as const;", main.indexOf("const BUSY_BUTTON_IDS = [")),
    );
    expect(busyList).not.toContain("auto-swap-nested-toggle");
  });

  it("两个子开关点击成功后立刻重算折叠 paint（不等下一次快照）", () => {
    const preferStart = main.indexOf(
      'required<HTMLButtonElement>("auto-swap-prefer-lower-tier-toggle").addEventListener',
    );
    const residentStart = main.indexOf(
      'required<HTMLButtonElement>("auto-swap-resident-lower-tier-toggle").addEventListener',
    );
    expect(preferStart).toBeGreaterThan(-1);
    expect(residentStart).toBeGreaterThan(preferStart);
    const preferHandler = main.slice(preferStart, residentStart);
    expect(preferHandler).toContain("paintAutoSwapNestedPanel(saved, currentNestedSwitchState().resident)");
    const residentHandler = main.slice(residentStart, residentStart + 800);
    expect(residentHandler).toContain(
      "paintAutoSwapNestedPanel(currentNestedSwitchState().prefer, saved)",
    );
  });

  it("状态行 aria-describedby 由 paint 层按真值增删", () => {
    expect(main).toContain('toggle.setAttribute("aria-describedby", "auto-swap-nested-status")');
    expect(main).toContain('toggle.removeAttribute("aria-describedby")');
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
  it("位于总开关之后、折叠面板之内，role=switch，静态关且 disabled", () => {
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
    const panel = libraryPage.indexOf('id="auto-swap-nested-panel"');
    const sub = libraryPage.indexOf('id="auto-swap-prefer-lower-tier-toggle"');
    expect(sub).toBeGreaterThan(master);
    expect(sub).toBeGreaterThan(panel);
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
  it("位于优先低档位开关之后且同处折叠面板内，role=switch，静态关且 disabled", () => {
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
    const panel = libraryPage.indexOf('id="auto-swap-nested-panel"');
    const prefer = libraryPage.indexOf('id="auto-swap-prefer-lower-tier-toggle"');
    const resident = libraryPage.indexOf('id="auto-swap-resident-lower-tier-toggle"');
    expect(resident).toBeGreaterThan(prefer);
    expect(resident).toBeGreaterThan(panel);
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
