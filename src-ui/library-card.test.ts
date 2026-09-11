import { describe, expect, it } from "vitest";

import type { AccountRow } from "./api";
import { accountCardHtml, cardActionButtons } from "./library-card";

function row(partial: Partial<AccountRow> = {}): AccountRow {
  return {
    id: "acc-1",
    label: "acc-1",
    active: false,
    membership: "Allegro",
    membership_expiry: null,
    membership_remaining_days: null,
    membership_period_days: null,
    membership_expiry_hint: false,
    quotas: [],
    error: null,
    credential_expired: false,
    cached_at: null,
    display_name: "Baker",
    official_nickname_known: true,
    auto_swap_opt_out: false,
    ...partial,
  };
}

describe("accountCardHtml", () => {
  it("当前账号用色片，不把「 · 当前」拼进标题", () => {
    const html = accountCardHtml(row({ active: true, display_name: "Forever4real" }), false);
    expect(html).toContain("Forever4real");
    expect(html).toContain("profile-current-chip");
    expect(html).not.toContain("Forever4real · 当前");
    expect(html).not.toContain("当前使用中");
  });

  it("停泊账号写「已加入账号库」，无当前色片", () => {
    const html = accountCardHtml(row({ active: false }), false);
    expect(html).toContain("已加入账号库");
    expect(html).not.toContain("profile-current-chip");
  });

  it("凭证待续期用奶油提示条，不把「账号状态」扩到账号库", () => {
    const html = accountCardHtml(
      row({ active: true, credential_expired: true, error: "引导" }),
      false,
    );
    expect(html).toContain("profile-renewal-banner");
    expect(html).toContain("凭证待续期");
    expect(html).toContain("在 Kimi CLI 里发条消息即可恢复，或重新授权");
    expect(html).not.toContain("account-status-banner");
    expect(html).not.toContain("账号状态正常");
  });

  it("官方昵称未知时「…」只收重命名；已知则隐藏", () => {
    const unknown = accountCardHtml(row({ official_nickname_known: false }), false);
    expect(unknown).toContain('data-overflow="acc-1"');
    expect(unknown).toContain("重命名");
    const known = accountCardHtml(row({ official_nickname_known: true }), false);
    expect(known).not.toContain("data-overflow");
    expect(known).not.toContain("重命名");
  });

  it("保留摘出自动切换，不用效果图的「输出」", () => {
    const html = accountCardHtml(row(), false);
    expect(html).toContain("摘出自动切换");
    expect(html).not.toContain("输出自动切换");
  });

  it("头像圆取显示名首字符", () => {
    const html = accountCardHtml(row({ display_name: "Baker" }), false);
    expect(html).toMatch(/class="profile-avatar"[^>]*>B</);
  });

  it("档位名与绑定入口留在档位行，不进「…」", () => {
    const html = accountCardHtml(
      row({ membership: "Allegro", membership_expiry: null, membership_expiry_hint: true }),
      false,
    );
    expect(html).toMatch(
      /class="profile-plan">Allegro <span class="plan-expiry">绑定后可显示会员到期<\/span><button class="profile-pill plan-bind"/,
    );
    expect(html).toContain('data-bind="acc-1"');
    expect(html).toContain(">绑定</button>");
    // 该卡官方昵称已知、「…」整颗隐藏，绑定入口不会落进菜单。
    expect(html).not.toContain("data-overflow");
  });
});

describe("cardActionButtons", () => {
  it("停泊账号有切换，当前账号没有", () => {
    expect(cardActionButtons(row({ active: false }), false)).toContain("data-swap");
    expect(cardActionButtons(row({ active: true }), false)).not.toContain("data-swap");
  });

  it("重新授权与删除留在右侧胶囊，不进「…」；删除保留危险样式", () => {
    const html = cardActionButtons(row({ active: false }), false);
    expect(html).toContain("重新授权");
    expect(html).toContain("删除");
    expect(html).toContain("profile-pill--danger");
    expect(html).not.toContain("data-rename");
    expect(html).not.toContain("data-overflow");
  });

  it("三颗胶囊都在文案前带手绘内联 SVG；当前账号只剩两颗", () => {
    const html = cardActionButtons(row({ active: false }), false);
    // 图标必须落在按钮内部、文案之前：data-* 起到 </button> 的片段以 </svg> + 文案收尾。
    const endings: Record<string, string> = {
      "data-swap": "</svg>切换",
      "data-reauth": "</svg>重新授权",
      "data-delete": "</svg>删除",
    };
    for (const [attr, ending] of Object.entries(endings)) {
      const start = html.indexOf(attr);
      const end = html.indexOf("</button>", start);
      expect(html.slice(start, end), `${attr} 缺少文案前的内联图标`).toContain(ending);
    }
    expect((html.match(/<svg/g) ?? []).length).toBe(3);
    const current = cardActionButtons(row({ active: true }), false);
    expect((current.match(/<svg/g) ?? []).length).toBe(2);
    expect(current).not.toContain("切换");
  });
});
