import { describe, expect, it } from "vitest";

import {
  bindWebButtonHtml,
  membershipPlanHtml,
  membershipTrackHtml,
  quotaStackHtml,
} from "./membership";
import type { QuotaView } from "./quota";

/** 样例行：默认给弱提示关闭的 Allegretto；`id` 供绑定按钮（issue #51）用。 */
type MembershipRow = Parameters<typeof membershipPlanHtml>[0] & { id: string };

function fields(partial: Partial<MembershipRow> = {}): MembershipRow {
  return {
    id: "acc-1",
    membership: "Allegretto",
    membership_expiry: null,
    membership_expiry_hint: false,
    ...partial,
  };
}

describe("membershipPlanHtml", () => {
  it("无档位时返回 null：保持现状，不强加提示", () => {
    expect(membershipPlanHtml(fields({ membership: null }))).toBeNull();
  });

  it("有到期日时档位行只留档位名，日期不写在档位旁", () => {
    const html = membershipPlanHtml(fields({ membership_expiry: "2027-08-27" }));
    expect(html).toBe("Allegretto");
    expect(html).not.toContain("有效期至");
    expect(html).not.toContain("2027-08-27");
  });

  it("档位在但无到期日时显示绑定弱提示（概念名「会员到期」），不出现「有效期至」", () => {
    const html = membershipPlanHtml(fields({ membership_expiry_hint: true }));
    expect(html).toBe(
      `Allegretto <span class="plan-expiry">绑定后可显示会员到期</span>`,
    );
    expect(html).not.toContain("有效期至");
    expect(html).not.toContain("绑定后可显示有效期");
  });

  it("刷新失败缓存降级：档位行无日期、无常驻重新绑定提示句", () => {
    const html = membershipPlanHtml(
      fields({ membership_expiry: "2027-08-27", membership_expiry_hint: true }),
    );
    expect(html).toBe("Allegretto");
    expect(html).not.toContain("有效期至");
    expect(html).not.toContain("重新绑定后可刷新有效期");
    expect(html).not.toContain("绑定后可显示有效期");
    expect(html).not.toContain("绑定后可显示会员到期");
  });

  it("hint 关闭且无到期日时只显示档位", () => {
    expect(membershipPlanHtml(fields())).toBe("Allegretto");
  });

  it("转义档位名", () => {
    const html = membershipPlanHtml(
      fields({ membership: `A<b>`, membership_expiry: "2027-08-27" }),
    );
    expect(html).toContain(`A&lt;b&gt;`);
    expect(html).not.toContain(`<b>`);
  });
});

describe("bindWebButtonHtml（issue #51 绑定入口，#63 两态）", () => {
  it("从未绑定（hint 亮且无到期日）渲染「绑定」，弱样式非错误红", () => {
    const html = bindWebButtonHtml(fields({ membership_expiry_hint: true, id: "acc-1" }));
    expect(html).toContain(`data-bind="acc-1"`);
    expect(html).toContain("plan-bind");
    expect(html).toContain(">绑定</button>");
    expect(html).not.toContain("重新绑定");
    expect(html).toContain(`title="绑定网页登录，更新会员到期"`);
    expect(html).not.toContain("刷新会员有效期");
    expect(html).not.toContain("有效期");
    expect(html).not.toContain("danger");
    expect(html).not.toContain("error");
  });

  it("刷新失败态（hint 亮且有到期日）渲染「重新绑定」，tooltip 同步重新绑定语义", () => {
    const html = bindWebButtonHtml(
      fields({ membership_expiry: "2027-08-27", membership_expiry_hint: true, id: "acc-1" }),
    );
    expect(html).toContain(`data-bind="acc-1"`);
    expect(html).toContain("plan-bind");
    expect(html).toContain(">重新绑定</button>");
    expect(html).toContain(`title="重新绑定网页登录，覆盖旧网页 token"`);
    expect(html).not.toMatch(/>绑定</);
  });

  it("无提示（有日期且刷新正常，或无档位）的卡不出现绑定按钮", () => {
    expect(bindWebButtonHtml(fields({ membership_expiry: "2027-08-27", id: "a" }))).toBe("");
    expect(bindWebButtonHtml(fields({ membership: null, id: "a" }))).toBe("");
  });

  it("转义账号 id，busy 时禁用防重复提交", () => {
    const html = bindWebButtonHtml(fields({ membership_expiry_hint: true, id: `x">` }));
    expect(html).toContain(`data-bind="x&quot;&gt;"`);
    expect(bindWebButtonHtml(fields({ membership_expiry_hint: true, id: "a" }), true)).toContain(
      " disabled",
    );
  });
});

function trackFields(
  partial: Partial<Parameters<typeof membershipTrackHtml>[0]> = {},
): Parameters<typeof membershipTrackHtml>[0] {
  return {
    membership: "Allegretto",
    membership_expiry: "2027-08-27",
    membership_remaining_days: 360,
    membership_period_days: 365,
    ...partial,
  };
}

function quota(partial: Partial<QuotaView> = {}): QuotaView {
  return {
    window: "5小时",
    ratio: 0.42,
    text: "42 / 100",
    reset: "4h 后重置",
    tone: "ok",
    ...partial,
  };
}

describe("membershipTrackHtml", () => {
  it("无档位或无到期日或剩余天数缺省时不画轨道", () => {
    expect(membershipTrackHtml(trackFields({ membership: null }))).toBe("");
    expect(membershipTrackHtml(trackFields({ membership_expiry: null }))).toBe("");
    expect(membershipTrackHtml(trackFields({ membership_remaining_days: null }))).toBe("");
  });

  it("剩余正整数：左会员、中徽章、右到期日，完整句在 title/aria-label", () => {
    const html = membershipTrackHtml(trackFields());
    expect(html).toContain(`<span class="quota-label">会员</span>`);
    expect(html).toContain(`<span class="quota-value">剩余 360 天</span>`);
    expect(html).toContain(`<span class="quota-reset">2027-08-27</span>`);
    expect(html).toContain(`title="会员 剩余 360 天 有效期至 2027-08-27"`);
    expect(html).toContain(`aria-label="会员 剩余 360 天 有效期至 2027-08-27"`);
    expect(html).toContain(`class="quota-row"`);
    expect(html).toContain(`class="quota-track"`);
  });

  it("剩余 0 显示今天到期", () => {
    const html = membershipTrackHtml(trackFields({ membership_remaining_days: 0 }));
    expect(html).toContain(`>今天到期<`);
    expect(html).toContain(`title="会员 今天到期 有效期至 2027-08-27"`);
    expect(html).toContain(`aria-label="会员 今天到期 有效期至 2027-08-27"`);
  });

  it("负整数显示已过期 n 天", () => {
    const html = membershipTrackHtml(trackFields({ membership_remaining_days: -3 }));
    expect(html).toContain(`>已过期 3 天<`);
    expect(html).toContain(`title="会员 已过期 3 天 有效期至 2027-08-27"`);
    expect(html).toContain(`aria-label="会员 已过期 3 天 有效期至 2027-08-27"`);
  });

  it("填充=已度过比例（周期−剩余）/周期，progressbar 语义完整", () => {
    const html = membershipTrackHtml(trackFields());
    // 周期 365、剩余 360 → 已度过 5/365 = 1.4%。
    expect(html).toContain(`role="progressbar"`);
    expect(html).toContain(`aria-valuemin="0"`);
    expect(html).toContain(`aria-valuemax="100"`);
    expect(html).toContain(`aria-valuenow="1"`);
    expect(html).toContain(`aria-valuetext="会员 剩余 360 天 有效期至 2027-08-27"`);
    expect(html).toContain(`class="quota-fill quota-fill--ok"`);
    expect(html).toContain(`style="width:1.4%"`);
  });

  it("月付周期按真实天数走：周期 30、剩 19 → 36.7%", () => {
    const html = membershipTrackHtml(
      trackFields({ membership_period_days: 30, membership_remaining_days: 19 }),
    );
    expect(html).toContain(`style="width:36.7%"`);
    expect(html).toContain(`aria-valuenow="37"`);
  });

  it("填充档位随剩余天数换色：31 ok / 30、8 warn / 7、1 high / 0 与负 full", () => {
    const toneOf = (days: number) =>
      membershipTrackHtml(trackFields({ membership_remaining_days: days })).match(
        /quota-fill--(\w+)/,
      )?.[1];
    expect(toneOf(31)).toBe("ok");
    expect(toneOf(30)).toBe("warn");
    expect(toneOf(8)).toBe("warn");
    expect(toneOf(7)).toBe("high");
    expect(toneOf(1)).toBe("high");
    expect(toneOf(0)).toBe("full");
    expect(toneOf(-3)).toBe("full");
  });

  it("周期最后一天走满 100%，过期后保持满条", () => {
    const last = membershipTrackHtml(trackFields({ membership_remaining_days: 0 }));
    expect(last).toContain(`style="width:100%"`);
    expect(last).toContain(`aria-valuenow="100"`);
    const expired = membershipTrackHtml(trackFields({ membership_remaining_days: -3 }));
    expect(expired).toContain(`style="width:100%"`);
  });

  it("周期缺失回退档位近似：Allegretto 按 365、Allegro 按 30", () => {
    const allegretto = membershipTrackHtml(trackFields({ membership_period_days: null }));
    expect(allegretto).toContain(`class="quota-fill quota-fill--ok"`);
    expect(allegretto).toContain(`style="width:1.4%"`);
    const allegro = membershipTrackHtml(
      trackFields({
        membership: "Allegro",
        membership_period_days: null,
        membership_remaining_days: 18,
        membership_expiry: "2026-09-20",
      }),
    );
    expect(allegro).toContain(`style="width:40%"`);
    expect(allegro).toContain(`aria-valuenow="40"`);
  });

  it("未知档位且无周期数据：不画填充，徽章与日期照常", () => {
    const html = membershipTrackHtml(
      trackFields({ membership: "Mystery", membership_period_days: null }),
    );
    expect(html).not.toContain("progressbar");
    expect(html).not.toContain("quota-fill");
    expect(html).not.toContain("style=\"width");
    expect(html).toContain(`<span class="quota-value">剩余 360 天</span>`);
    expect(html).toContain(`title="会员 剩余 360 天 有效期至 2027-08-27"`);
    expect(html).toContain(`<span class="quota-reset">2027-08-27</span>`);
  });

  it("非法周期（0 或负数）等同缺失，走档位回退", () => {
    expect(membershipTrackHtml(trackFields({ membership_period_days: 0 }))).toContain(
      "quota-fill",
    );
    expect(
      membershipTrackHtml(trackFields({ membership: "Allegro", membership_period_days: -5 })),
    ).toContain("quota-fill");
  });

  it("转义到期日", () => {
    const html = membershipTrackHtml(
      trackFields({ membership_expiry: `2027-08-27">x` }),
    );
    expect(html).toContain("2027-08-27&quot;&gt;x");
    expect(html).not.toContain(`2027-08-27">x`);
  });
});

describe("quotaStackHtml", () => {
  it("会员轨道在用量窗口之下", () => {
    const html = quotaStackHtml({
      ...trackFields(),
      quotas: [quota()],
      error: null,
      cached_at: null,
      credential_expired: false,
    });
    const memberAt = html.indexOf("quota-label\">会员");
    const windowAt = html.indexOf("quota-label\">5小时");
    expect(memberAt).toBeGreaterThanOrEqual(0);
    expect(memberAt).toBeGreaterThan(windowAt);
  });

  it("会员轨道在 n/a 与查询失败文案之下", () => {
    const na = quotaStackHtml({
      ...trackFields(),
      quotas: [],
      error: null,
      cached_at: null,
      credential_expired: false,
    });
    expect(na.indexOf("会员")).toBeGreaterThan(na.indexOf("额度: n/a"));

    const failed = quotaStackHtml({
      ...trackFields(),
      quotas: [],
      error: "超时",
      cached_at: null,
      credential_expired: false,
    });
    expect(failed.indexOf("会员")).toBeGreaterThan(failed.indexOf("额度查询失败"));
  });

  it("无档位时不画轨道，额度条保持原样", () => {
    const html = quotaStackHtml({
      membership: null,
      membership_expiry: "2027-08-27",
      membership_remaining_days: 360,
      membership_period_days: 365,
      quotas: [quota()],
      error: null,
      cached_at: null,
      credential_expired: false,
    });
    expect(html).not.toContain("会员");
    expect(html).toContain(`role="progressbar"`);
    expect(html).toContain("quota-fill--ok");
  });
});
