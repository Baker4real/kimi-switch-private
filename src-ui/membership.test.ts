import { describe, expect, it } from "vitest";

import { bindWebButtonHtml, membershipPlanHtml } from "./membership";

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

  it("有到期日时档位旁显示「有效期至 2027-08-27」，后缀用弱样式", () => {
    const html = membershipPlanHtml(
      fields({ membership_expiry: "2027-08-27" }),
    );
    expect(html).toBe(
      `Allegretto <span class="plan-expiry">有效期至 2027-08-27</span>`,
    );
    // 弱提示用 muted 样式，不是错误红。
    expect(html).not.toContain("error");
  });

  it("档位在但无到期日时显示绑定弱提示（#49 票面原文），不出现「有效期至」", () => {
    const html = membershipPlanHtml(fields({ membership_expiry_hint: true }));
    expect(html).toBe(
      `Allegretto <span class="plan-expiry">绑定后可显示有效期</span>`,
    );
    expect(html).not.toContain("有效期至");
  });

  it("到期日与提示共存（issue #52 刷新失败缓存降级）：日期在前、提示改为重新绑定语义", () => {
    const html = membershipPlanHtml(
      fields({ membership_expiry: "2027-08-27", membership_expiry_hint: true }),
    );
    expect(html).toBe(
      `Allegretto <span class="plan-expiry">有效期至 2027-08-27</span>` +
        ` <span class="plan-expiry">重新绑定后可刷新有效期</span>`,
    );
    // 日期不被提示挤掉：网页 token 失效不单独把日期藏起来。
    expect(html).toContain("有效期至 2027-08-27");
    expect(html).not.toContain("error");
  });

  it("hint 关闭且无到期日时只显示档位", () => {
    expect(membershipPlanHtml(fields())).toBe("Allegretto");
  });

  it("转义档位名与后缀文案", () => {
    const html = membershipPlanHtml(
      fields({ membership: `A<b>`, membership_expiry: "2027-08-27" }),
    );
    expect(html).toContain(`A&lt;b&gt;`);
    expect(html).toContain(`有效期至 2027-08-27`);
    expect(html).not.toContain(`<b>`);
  });
});

describe("bindWebButtonHtml（issue #51 绑定入口，#52 扩展）", () => {
  it("弱提示（档位在但无到期日）时渲染绑定按钮，弱样式非错误红", () => {
    const html = bindWebButtonHtml(fields({ membership_expiry_hint: true, id: "acc-1" }));
    expect(html).toContain(`data-bind="acc-1"`);
    expect(html).toContain("plan-bind");
    expect(html).toContain(">绑定</button>");
    // 弱样式：不带 danger / error 语义。
    expect(html).not.toContain("danger");
    expect(html).not.toContain("error");
  });

  it("刷新失败态（有日期 + 提示共存）也出现绑定按钮：重新绑定即可恢复刷新", () => {
    const html = bindWebButtonHtml(
      fields({ membership_expiry: "2027-08-27", membership_expiry_hint: true, id: "acc-1" }),
    );
    expect(html).toContain(`data-bind="acc-1"`);
    expect(html).toContain("plan-bind");
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
