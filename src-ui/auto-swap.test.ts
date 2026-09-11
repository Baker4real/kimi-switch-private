import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  autoSwapOptOutSwitchHtml,
  autoSwapOptOutSwitchView,
  autoSwapPreferLowerTierSwitchView,
  autoSwapPreResetBurnSwitchView,
  autoSwapResidentLowerTierSwitchView,
  autoSwapSwitchView,
  behaviorPanelStatusText,
  silentQuotaMessage,
} from "./auto-swap";

const html = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "index.html"),
  "utf8",
);
const libraryPage = html.slice(
  html.indexOf('id="page-library"'),
  html.indexOf('id="auth-dialog"'),
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

  it("抢用 / 回程成功文案只要 kind 是 auto_swap_success 就保留", () => {
    const burn = {
      text: "已抢用 工作机",
      tone: "ok" as const,
      kind: "auto_swap_success" as const,
    };
    const ret = {
      text: "已回程 原号",
      tone: "ok" as const,
      kind: "auto_swap_success" as const,
    };
    expect(silentQuotaMessage(burn)).toEqual(burn);
    expect(silentQuotaMessage(ret)).toEqual(ret);
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

describe("behaviorPanelStatusText 收起态状态行真值表（issue #120 / #150）", () => {
  it("全关返回空串；只开续期 → 已开启：主动续期", () => {
    expect(behaviorPanelStatusText(false, false, false, false, false)).toBe("");
    expect(behaviorPanelStatusText(true, false, false, false, false)).toBe(
      "已开启：主动续期",
    );
  });

  it("只开自动切换 → 已开启：自动切换；自动切换 + 回驻 → 已开启：自动切换、回驻", () => {
    expect(behaviorPanelStatusText(false, true, false, false, false)).toBe(
      "已开启：自动切换",
    );
    expect(behaviorPanelStatusText(false, true, false, true, false)).toBe(
      "已开启：自动切换、回驻",
    );
  });

  it("自动切换 + 优先低档 → 已开启：自动切换、优先低档；全开五项钉版", () => {
    expect(behaviorPanelStatusText(false, true, true, false, false)).toBe(
      "已开启：自动切换、优先低档",
    );
    expect(behaviorPanelStatusText(true, true, true, true, true)).toBe(
      "已开启：主动续期、自动切换、优先低档、回驻、重置前抢用",
    );
  });

  it("自动切换 + 重置前抢用 → 已开启：自动切换、重置前抢用", () => {
    expect(behaviorPanelStatusText(false, true, false, false, true)).toBe(
      "已开启：自动切换、重置前抢用",
    );
  });

  it("自动切换关时不写优先低档 / 回驻 / 重置前抢用（总开关关不算生效项）", () => {
    expect(behaviorPanelStatusText(false, false, true, true, true)).toBe("");
    // 续期也关则无状态行；续期开则只写续期。
    expect(behaviorPanelStatusText(true, false, true, true, true)).toBe("已开启：主动续期");
  });

  it("文案不含禁用词", () => {
    for (const renewal of [false, true]) {
      for (const autoSwap of [false, true]) {
        for (const prefer of [false, true]) {
          for (const resident of [false, true]) {
            for (const burn of [false, true]) {
              const text = behaviorPanelStatusText(
                renewal,
                autoSwap,
                prefer,
                resident,
                burn,
              );
              expect(text).not.toContain("切回");
              expect(text).not.toContain("下拉");
              expect(text).not.toContain("高级");
              expect(text).not.toContain("更多");
              expect(text).not.toContain("套餐");
              expect(text).not.toMatch(/plan/i);
            }
          }
        }
      }
    }
  });
});

describe("账号库页「账号行为」一层折叠（issue #120）", () => {
  it("标题是折叠按钮：h3 内 type=button，带 aria-expanded=false 与 aria-controls，caret aria-hidden，右侧不是 switch", () => {
    expect(libraryPage).toContain('id="behavior-panel-toggle"');
    expect(libraryPage).toMatch(/id="behavior-panel-toggle"[^>]*type="button"/);
    expect(libraryPage).toMatch(/id="behavior-panel-toggle"[^>]*aria-expanded="false"/);
    expect(libraryPage).toMatch(
      /id="behavior-panel-toggle"[^>]*aria-controls="behavior-panel-body"/,
    );
    // 保留标题层级：折叠按钮包在 h3#behavior-panel-title 里，页面上只有一个「账号行为」标题控件。
    const title = libraryPage.indexOf('id="behavior-panel-title"');
    const toggle = libraryPage.indexOf('id="behavior-panel-toggle"');
    expect(title).toBeGreaterThan(-1);
    expect(toggle).toBeGreaterThan(title);
    expect(libraryPage.match(/账号行为/g)).toHaveLength(1);
    const snippet = libraryPage.slice(toggle, toggle + 300);
    expect(snippet).toContain('class="settings-disclosure-caret" aria-hidden="true"');
    expect(snippet).not.toMatch(/id="behavior-panel-toggle"[^>]*role="switch"/);
  });

  it("状态行挂在标题区（不在会被隐藏的面板里），静态首帧为空且隐藏", () => {
    const toggle = libraryPage.indexOf('id="behavior-panel-toggle"');
    const status = libraryPage.indexOf('id="behavior-panel-status"');
    const panel = libraryPage.indexOf('id="behavior-panel-body"');
    expect(status).toBeGreaterThan(toggle);
    // 必须落在面板之前：面板收起时 class="hidden"，状态行要仍看得见。
    expect(status).toBeLessThan(panel);
    const snippet = libraryPage.slice(status, status + 120);
    expect(snippet).toMatch(/class="[^"]*\bhidden\b[^"]*"/);
    // 静态首帧全关：标题不挂 aria-describedby，等 paint 层按真值补。
    expect(libraryPage).not.toMatch(/id="behavior-panel-toggle"[^>]*aria-describedby/);
  });

  it("静态首帧收起且对视觉/辅助技术隐藏，五个开关留在文档里且顺序正确", () => {
    expect(libraryPage).toMatch(
      /id="behavior-panel-body"[^>]*class="[^"]*\bhidden\b[^"]*"/,
    );
    const panel = libraryPage.indexOf('id="behavior-panel-body"');
    const renewal = libraryPage.indexOf('id="renewal-toggle"');
    const master = libraryPage.indexOf('id="auto-swap-toggle"');
    const prefer = libraryPage.indexOf('id="auto-swap-prefer-lower-tier-toggle"');
    const resident = libraryPage.indexOf('id="auto-swap-resident-lower-tier-toggle"');
    const burn = libraryPage.indexOf('id="auto-swap-pre-reset-burn-toggle"');
    expect(panel).toBeGreaterThan(-1);
    expect(renewal).toBeGreaterThan(panel);
    expect(master).toBeGreaterThan(renewal);
    expect(prefer).toBeGreaterThan(master);
    expect(resident).toBeGreaterThan(prefer);
    expect(burn).toBeGreaterThan(resident);
  });

  it("#117 内层折叠不并存：无「优先低档与回驻」标题、无第二层 caret / aria-expanded、无旧节点", () => {
    expect(libraryPage).not.toContain("优先低档与回驻");
    expect(libraryPage).not.toContain('id="auto-swap-nested-toggle"');
    expect(libraryPage).not.toContain('id="auto-swap-nested-panel"');
    expect(libraryPage).not.toContain('id="auto-swap-nested-status"');
    // 只有一层折叠：标题那一层仅一个 caret 与一个 aria-expanded。
    expect(libraryPage.match(/settings-disclosure-caret/g)).toHaveLength(1);
    expect(libraryPage.match(/aria-expanded/g)).toHaveLength(1);
  });

  it("不用原生 details / select / 弹出层", () => {
    expect(libraryPage).not.toContain("<details");
    expect(libraryPage).not.toContain("<select");
    expect(libraryPage).not.toContain("popover");
  });
});

describe("main.ts 账号行为折叠会话接线（issue #120）", () => {
  const main = readFileSync(
    join(dirname(fileURLToPath(import.meta.url)), "main.ts"),
    "utf8",
  );

  /** 取 main.ts 某顶层函数的源文本（从声明起到下一个顶层 function 声明为止）。 */
  const functionSrc = (name: string): string => {
    const start = main.indexOf(`function ${name}(`);
    expect(start, `缺少函数: ${name}`).toBeGreaterThan(-1);
    const next = main.indexOf("\nfunction ", start + 1);
    return main.slice(start, next < 0 ? main.length : next);
  };

  it("开合是会话布尔：仅初始化与标题取反两个赋值点，paint / applyRoute / 快照绘制不改写", () => {
    expect(main).toContain("let behaviorPanelExpanded = false;");
    expect(main).toContain("behaviorPanelExpanded = !behaviorPanelExpanded;");
    // 全文件只有声明与取反两处赋值：快照、静默列出、换路由没有第三条写路径。
    expect(main.match(/behaviorPanelExpanded = /g)).toHaveLength(2);
    expect(functionSrc("paintBehaviorPanel")).not.toMatch(/behaviorPanelExpanded = /);
    expect(functionSrc("applyRoute")).not.toContain("behaviorPanelExpanded");
    const snapshot = functionSrc("paintSnapshot");
    expect(snapshot).not.toMatch(/behaviorPanelExpanded = /);
    // 快照只读开合态重涂状态行，不得借机改写开合。
    expect(snapshot).toContain("paintBehaviorPanel();");
  });

  it("折叠相关代码不写 localStorage / sessionStorage / config.toml（任何盘上存储）", () => {
    expect(main).not.toContain("localStorage");
    expect(main).not.toContain("sessionStorage");
    expect(main).not.toContain("config.toml");
  });

  it("五个开关保存成功后都立刻重涂折叠状态行（不等下一次快照）", () => {
    const renewalStart = main.indexOf(
      'required<HTMLButtonElement>("renewal-toggle").addEventListener',
    );
    const titleStart = main.indexOf(
      'required<HTMLButtonElement>("behavior-panel-toggle").addEventListener',
    );
    const masterStart = main.indexOf(
      'required<HTMLButtonElement>("auto-swap-toggle").addEventListener',
    );
    const preferStart = main.indexOf(
      'required<HTMLButtonElement>("auto-swap-prefer-lower-tier-toggle").addEventListener',
    );
    const residentStart = main.indexOf(
      'required<HTMLButtonElement>("auto-swap-resident-lower-tier-toggle").addEventListener',
    );
    const burnStart = main.indexOf(
      'required<HTMLButtonElement>("auto-swap-pre-reset-burn-toggle").addEventListener',
    );
    expect(renewalStart).toBeGreaterThan(-1);
    expect(titleStart).toBeGreaterThan(renewalStart);
    expect(masterStart).toBeGreaterThan(titleStart);
    expect(preferStart).toBeGreaterThan(masterStart);
    expect(residentStart).toBeGreaterThan(preferStart);
    expect(burnStart).toBeGreaterThan(residentStart);
    expect(main.slice(renewalStart, titleStart)).toContain("paintBehaviorPanel();");
    expect(main.slice(masterStart, preferStart)).toContain("paintBehaviorPanel();");
    expect(main.slice(preferStart, residentStart)).toContain("paintBehaviorPanel();");
    expect(main.slice(residentStart, burnStart)).toContain("paintBehaviorPanel();");
    expect(main.slice(burnStart, burnStart + 900)).toContain("paintBehaviorPanel();");
  });

  it("标题 click 只取反开合并重涂：不在 BUSY_BUTTON_IDS、不弹 toast（busy 时仍可开合）", () => {
    const titleStart = main.indexOf(
      'required<HTMLButtonElement>("behavior-panel-toggle").addEventListener',
    );
    const masterStart = main.indexOf(
      'required<HTMLButtonElement>("auto-swap-toggle").addEventListener',
    );
    const handler = main.slice(titleStart, masterStart);
    expect(handler).toContain("behaviorPanelExpanded = !behaviorPanelExpanded;");
    expect(handler).toContain("paintBehaviorPanel();");
    expect(handler).not.toContain("showToast");
    const busyList = main.slice(
      main.indexOf("const BUSY_BUTTON_IDS = ["),
      main.indexOf("] as const;", main.indexOf("const BUSY_BUTTON_IDS = [")),
    );
    expect(busyList).not.toContain("behavior-panel-toggle");
  });

  it("状态行 aria-describedby 由 paint 层按真值增删：空移除、非空挂 behavior-panel-status", () => {
    expect(main).toContain(
      'toggle.setAttribute("aria-describedby", "behavior-panel-status")',
    );
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
  it("位于总开关之后、账号行为折叠面板之内，role=switch，静态关且 disabled", () => {
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
    const panel = libraryPage.indexOf('id="behavior-panel-body"');
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

describe("autoSwapPreResetBurnSwitchView（issue #150）", () => {
  it("总开关关时子开关置灰，checked 仍反映持久化值", () => {
    expect(autoSwapPreResetBurnSwitchView(false, false)).toEqual({
      checked: false,
      disabled: true,
      toast: "已关闭重置前抢用",
    });
    expect(autoSwapPreResetBurnSwitchView(false, true)).toEqual({
      checked: true,
      disabled: true,
      toast: "已开启重置前抢用",
    });
  });

  it("总开关开时子开关可点，checked 反映持久化值", () => {
    expect(autoSwapPreResetBurnSwitchView(true, false)).toEqual({
      checked: false,
      disabled: false,
      toast: "已关闭重置前抢用",
    });
    expect(autoSwapPreResetBurnSwitchView(true, true)).toEqual({
      checked: true,
      disabled: false,
      toast: "已开启重置前抢用",
    });
  });
});

describe("账号库页「重置前抢用」子开关（issue #150）", () => {
  it("位于回驻开关之后且同处折叠面板内，role=switch，静态关且 disabled", () => {
    expect(libraryPage).toContain('id="auto-swap-pre-reset-burn-toggle"');
    expect(libraryPage).toMatch(/id="auto-swap-pre-reset-burn-toggle"[^>]*role="switch"/);
    expect(libraryPage).toMatch(
      /id="auto-swap-pre-reset-burn-toggle"[^>]*aria-checked="false"/,
    );
    expect(libraryPage).toMatch(/id="auto-swap-pre-reset-burn-toggle"[^>]*\sdisabled/);
    const panel = libraryPage.indexOf('id="behavior-panel-body"');
    const resident = libraryPage.indexOf('id="auto-swap-resident-lower-tier-toggle"');
    const burn = libraryPage.indexOf('id="auto-swap-pre-reset-burn-toggle"');
    expect(burn).toBeGreaterThan(resident);
    expect(burn).toBeGreaterThan(panel);
  });

  it("文案为重置前抢用并说明烧尾巴与自动切回，不写套餐、plan 或三个参数", () => {
    expect(libraryPage).toContain("重置前抢用");
    expect(libraryPage).toContain("临近重置时自动切到快清零的停泊账号烧尾巴，烧完自动切回");
    expect(libraryPage).toContain("默认关闭");
    const start = libraryPage.indexOf("auto-swap-pre-reset-burn");
    const snippet = libraryPage.slice(start, start + 900);
    expect(snippet).not.toContain("套餐");
    expect(snippet).not.toMatch(/\bplan\b/i);
    expect(snippet).not.toContain("pre_reset_burn_min_ratio");
    expect(snippet).not.toContain("pre_reset_burn_near_5h_ms");
    expect(snippet).not.toContain("pre_reset_burn_near_7d_ms");
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
    const panel = libraryPage.indexOf('id="behavior-panel-body"');
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
