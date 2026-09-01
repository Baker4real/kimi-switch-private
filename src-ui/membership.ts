import { escapeHtml } from "./escape";

export interface MembershipFields {
  membership: string | null;
  membership_expiry: string | null;
  membership_expiry_hint: boolean;
}

/** 档位行展示 HTML（总览与账号卡共用）。
 *
 * - 有档位 + 到期日：`档位 <span class="plan-expiry">有效期至 2027-08-27</span>`
 * - 有档位无到期日：`档位 <span class="plan-expiry">绑定后可显示有效期</span>`
 *   （弱提示，非错误红；#49 票面原文）
 * - 到期日 + 提示共存（issue #52 缓存降级）：刷新失败（含网页 token 失效）时
 *   旧日期仍在，提示改为重新绑定语义——「重新绑定后可刷新有效期」，与绑定
 *   对话框（webBind.ts）的绑定文案同一语义：两个后缀各占一个弱样式 span
 * - 无档位（无额度数据）：返回 null，调用方保持现状，不强加提示
 *
 * 日期由 Rust 侧格式化为东八区日历日，前端只拼接不解析。 */
export function membershipPlanHtml(row: MembershipFields): string | null {
  const membership = row.membership;
  if (!membership) {
    return null;
  }
  const plan = escapeHtml(membership);
  const suffixes: string[] = [];
  if (row.membership_expiry) {
    suffixes.push(`有效期至 ${row.membership_expiry}`);
  }
  if (row.membership_expiry_hint) {
    // 两态文案：无日期 = 从未绑定（引导首次绑定）；有日期 = 刷新失败降级
    // （网页 token 失效，引导重新绑定）。
    suffixes.push(
      row.membership_expiry ? "重新绑定后可刷新有效期" : "绑定后可显示有效期",
    );
  }
  if (suffixes.length === 0) {
    return plan;
  }
  return `${plan} ${suffixes
    .map((suffix) => `<span class="plan-expiry">${escapeHtml(suffix)}</span>`)
    .join(" ")}`;
}

/** 档位行旁的「绑定」入口（issue #51；issue #52 扩展到刷新失败态）：弱提示
 * 出现时即可绑定 / 重新绑定——档位在但无到期日（从未绑定），或刷新失败后
 * 日期旁再现提示（网页 token 失效，重新绑定即可恢复刷新）。复用
 * `profile-pill` 弱样式（非错误红）；点击由 main.ts 事件委托打开绑定对话框。
 * 无档位（无额度数据）的卡不出现该按钮。 */
export function bindWebButtonHtml(
  row: MembershipFields & { id: string },
  busy = false,
): string {
  const bindable = row.membership !== null && row.membership_expiry_hint;
  if (!bindable) {
    return "";
  }
  const disabled = busy ? " disabled" : "";
  return `<button class="profile-pill plan-bind" data-bind="${escapeHtml(row.id)}" type="button" title="绑定网页登录，刷新会员有效期"${disabled}>绑定</button>`;
}
