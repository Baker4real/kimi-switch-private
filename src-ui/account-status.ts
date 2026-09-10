/** 总览当前账号卡底部「账号状态」健康汇总条（issue #134）。
 *
 * 只读已收集的快照字段派生：不发起任何网络请求、不新增判定来源，也不改写
 * 「额度告急」（`crates/gui/src/dto.rs` 的 `URGENT_USED_RATIO` 与 CONTEXT.md 词条）
 * 与「凭证待续期」（`AccountRow.credential_expired`）的既有判定。 */

import { escapeHtml } from "./escape";
import {
  STATUS_FAILED_ICON,
  STATUS_OK_ICON,
  STATUS_RENEWAL_ICON,
  STATUS_URGENT_ICON,
} from "./icons";
import type { Tone } from "./quota";

/** 额度告急阈值：任一用量窗口已用比例 ≥ 此值。
 * 唯一出处是 Rust 侧 `crates/gui/src/dto.rs` 的 `URGENT_USED_RATIO`（CONTEXT.md「额度告急」），
 * 这里只镜像同一常量；`account-status.test.ts` 直接读 Rust 源做一致性守卫。 */
export const URGENT_USED_RATIO = 0.8;

/** 汇总条语义态：正常 / 额度告急 / 凭证待续期 / 额度查询失败。 */
export type AccountStatusState = "ok" | "urgent" | "renewal" | "failed";

export interface AccountStatusView {
  state: AccountStatusState;
  /** 复用四档状态色语义：正常=ok、凭证待续期=warn、额度告急=high、查询失败=full。 */
  tone: Tone;
  title: string;
  detail: string;
}

/** 判定只读的字段子集（结构化类型，便于测试传最小对象）。 */
export interface AccountStatusFields {
  error: string | null;
  credential_expired: boolean;
  quotas: ReadonlyArray<{ ratio: number | null }>;
}

/** 额度告急：任一用量窗口已用 ≥ [`URGENT_USED_RATIO`]。
 * `ratio` 缺失（额度未知）不计告急，与 Rust `AccountRow::is_urgent` 同义。 */
export function isQuotaUrgent(quotas: ReadonlyArray<{ ratio: number | null }>): boolean {
  return quotas.some((q) => q.ratio !== null && q.ratio >= URGENT_USED_RATIO);
}

const VIEWS: Record<AccountStatusState, AccountStatusView> = {
  ok: {
    state: "ok",
    tone: "ok",
    title: "账号状态正常",
    detail: "一切运行良好，继续享受 AI 服务吧！",
  },
  urgent: {
    state: "urgent",
    tone: "high",
    title: "账号额度告急",
    detail: "部分额度窗口即将耗尽，建议切换账号",
  },
  renewal: {
    state: "renewal",
    tone: "warn",
    title: "凭证待续期",
    detail: "在 Kimi CLI 里发条消息即可恢复，或重新授权",
  },
  failed: {
    state: "failed",
    tone: "full",
    title: "额度查询失败",
    detail: "暂时无法获取额度，稍后重试",
  },
};

/**
 * 汇总条视图决策（纯函数，无 DOM 副作用）。
 *
 * 无当前账号返回 `null`，调用方隐藏整条（总览空态文案由 #133 的 `current-quota-panel` 承担）。
 *
 * 多异常同时命中的优先级（票面未规定，此处固化）：
 * **凭证待续期 > 额度查询失败 > 额度告急**。
 * - 凭证待续期必须先判：后端 `QuotaState::Failed` 下 `credential_expired` 为真时 `error`
 *   必然也非空（`crates/gui/src/backend.rs`），若先判 `error` 则「凭证待续期」态永不可达，
 *   会把凭证生命周期状态误报成查询故障。
 * - 查询失败先于额度告急：`Failed` 时 `quotas` 为空，正常构造下二者不会同时命中；该顺序
 *   只在坏数据 / 未来字段扩展时兜底，取「数据缺失」优先于「用量压力」，因为前者下告急
 *   判定本身不可信。告急只是用量达到阈值，用户可自行决定是否切换。
 * 真值表见 `account-status.test.ts`。
 */
export function accountStatusView(
  row: AccountStatusFields | null,
): AccountStatusView | null {
  if (row === null) {
    return null;
  }
  if (row.credential_expired) {
    return VIEWS.renewal;
  }
  if (row.error !== null) {
    return VIEWS.failed;
  }
  if (isQuotaUrgent(row.quotas)) {
    return VIEWS.urgent;
  }
  return VIEWS.ok;
}

const ICONS: Record<AccountStatusState, string> = {
  ok: STATUS_OK_ICON,
  urgent: STATUS_URGENT_ICON,
  renewal: STATUS_RENEWAL_ICON,
  failed: STATUS_FAILED_ICON,
};

/** 汇总条内容 HTML（图标 + 标题 + 说明）；状态类名由调用方按 `view.tone` 涂在外层容器上。 */
export function accountStatusBodyHtml(view: AccountStatusView): string {
  return `<span class="account-status-icon" aria-hidden="true">${ICONS[view.state]}</span>
        <p class="account-status-copy">
          <strong class="account-status-title">${escapeHtml(view.title)}</strong>
          <span class="account-status-detail">${escapeHtml(view.detail)}</span>
        </p>`;
}
