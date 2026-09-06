import { escapeHtml } from "./escape";
import { quotaHtml, type QuotaView, type Tone } from "./quota";

export interface MembershipFields {
  membership: string | null;
  membership_expiry: string | null;
  membership_expiry_hint: boolean;
}

/** 档位行展示 HTML（总览与账号卡共用）。
 *
 * - 有档位 + 到期日：只档位名（日期改由会员轨道右列承担，#60）
 * - 有档位无到期日 + hint：`档位 <span class="plan-expiry">绑定后可显示有效期</span>`
 *   （弱提示，非错误红；#49 票面原文，从未绑定不回归）
 * - 有档位 + 到期日 + hint（刷新失败缓存降级）：仍只档位名，不写常驻
 *   「重新绑定后可刷新有效期」；重新绑定入口在按钮（#63）
 * - 无档位（无额度数据）：返回 null，调用方保持现状，不强加提示
 *
 * 日期由 Rust 侧格式化为东八区日历日，前端只拼接不解析。 */
export function membershipPlanHtml(row: MembershipFields): string | null {
  const membership = row.membership;
  if (!membership) {
    return null;
  }
  const plan = escapeHtml(membership);
  if (row.membership_expiry) {
    return plan;
  }
  if (row.membership_expiry_hint) {
    return `${plan} <span class="plan-expiry">绑定后可显示有效期</span>`;
  }
  return plan;
}

/** 档位行旁的绑定入口（issue #51；#63 两态）：
 * `membership != null && membership_expiry_hint` 时出现。
 * 无到期日 →「绑定」；有到期日（刷新失败）→「重新绑定」+ tooltip 同步重新绑定语义。
 * 复用 `profile-pill` 弱样式（非错误红）；点击由 main.ts 事件委托打开绑定对话框。
 * 无档位（无额度数据）的卡不出现该按钮。 */
export function bindWebButtonHtml(
  row: MembershipFields & { id: string },
  busy = false,
): string {
  const bindable = row.membership !== null && row.membership_expiry_hint;
  if (!bindable) {
    return "";
  }
  const rebind = Boolean(row.membership_expiry);
  const label = rebind ? "重新绑定" : "绑定";
  const title = rebind
    ? "重新绑定网页登录，覆盖旧网页 token"
    : "绑定网页登录，刷新会员有效期";
  const disabled = busy ? " disabled" : "";
  return `<button class="profile-pill plan-bind" data-bind="${escapeHtml(row.id)}" type="button" title="${title}"${disabled}>${label}</button>`;
}

export interface MembershipTrackFields {
  membership: string | null;
  membership_expiry: string | null;
  membership_remaining_days: number | null;
  membership_period_days: number | null;
}

function remainingBadge(days: number): string {
  if (days > 0) {
    return `剩余 ${days} 天`;
  }
  if (days === 0) {
    return "今天到期";
  }
  return `已过期 ${-days} 天`;
}

/** 剩余 >30 天绿、8–30 天黄、1–7 天橙、0 与负数红，与额度条四档 tone 同一套视觉语言。 */
function remainingTone(days: number): Tone {
  if (days <= 0) {
    return "full";
  }
  if (days <= 7) {
    return "high";
  }
  if (days <= 30) {
    return "warn";
  }
  return "ok";
}

/** 已度过比例（%）：（周期天数 − 剩余天数）/ 周期天数，保留 1 位小数。
 * 周期最后一天剩余为 0 → 恰为 100%；过期后按满条处理（封顶 100%）。 */
function remainingPercent(remaining: number, period: number): number {
  const raw = ((period - remaining) / period) * 100;
  return Math.round(Math.max(0, Math.min(100, raw)) * 10) / 10;
}

/** 档位近似的周期天数：老绑定没有周期起点键时的回退分母。
 * Allegretto 按年、其余已知档位按月；与真实计费方式可能不符，重新绑定后
 * 即被 currentStartTime 的真实值取代。未知档位返回 null（不画填充）。 */
function fallbackPeriodDays(membership: string): number | null {
  switch (membership) {
    case "Allegretto":
      return 365;
    case "Allegro":
    case "Andante":
    case "Moderato":
      return 30;
    default:
      return null;
  }
}

/** 额度 stack 底部的会员轨道：有档位且有到期日（含剩余天数，0 与负数也算）才画。
 * 填充=已度过比例（朝到期日推进：真实周期来自 currentStartTime 的日历天数，
 * 缺失时按档位近似回退），最后一天走满；不是用量压力，四档 tone 仍按剩余天数。 */
export function membershipTrackHtml(row: MembershipTrackFields): string {
  if (
    row.membership == null ||
    row.membership_expiry == null ||
    row.membership_remaining_days == null
  ) {
    return "";
  }
  const badge = remainingBadge(row.membership_remaining_days);
  const date = row.membership_expiry;
  const sentence = `会员 ${badge} 有效期至 ${date}`;
  const period =
    row.membership_period_days != null && row.membership_period_days > 0
      ? row.membership_period_days
      : row.membership != null
        ? fallbackPeriodDays(row.membership)
        : null;
  const width =
    period == null ? null : remainingPercent(row.membership_remaining_days, period);
  const fill =
    width == null
      ? ""
      : `<div class="quota-fill quota-fill--${remainingTone(row.membership_remaining_days)}" style="width:${width}%"></div>`;
  const progressAttrs =
    width == null
      ? ""
      : ` role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${Math.round(width)}" aria-valuetext="${escapeHtml(sentence)}"`;
  return `<div class="quota-row">
        <span class="quota-label">会员</span>
        <div class="quota-track"${progressAttrs} title="${escapeHtml(sentence)}" aria-label="${escapeHtml(sentence)}">
          ${fill}<span class="quota-value">${escapeHtml(badge)}</span>
        </div>
        <span class="quota-reset">${escapeHtml(date)}</span>
      </div>`;
}

/** 总览额度面板与账号卡 `.profile-quota-stack` 共用：会员轨道排在用量窗口 / n/a / 失败文案之后。 */
export function quotaStackHtml(
  row: MembershipTrackFields & {
    quotas: QuotaView[];
    error: string | null;
    cached_at: string | null;
    credential_expired: boolean;
  },
): string {
  return `${quotaHtml(row.quotas, row.error, row.cached_at, row.credential_expired)}${membershipTrackHtml(row)}`;
}
