import type { AccountRow } from "./api";
import { libraryCredentialRenewalHtml } from "./account-status";
import { autoSwapOptOutSwitchHtml } from "./auto-swap";
import { escapeHtml } from "./escape";
import { KEY_ICON, SWAP_ICON, TRASH_ICON } from "./icons";
import {
  accountDisplayName,
  currentAccountChipHtml,
  displayInitial,
  renameOverflowHtml,
} from "./labels";
import { bindWebButtonHtml, membershipPlanHtml, quotaStackHtml } from "./membership";
import { accountFocusAttr } from "./store";

/** 账号库卡片右侧胶囊：切换 / 重新授权 / 删除；重命名改走右上角菜单。 */
export function cardActionButtons(row: AccountRow, busy: boolean): string {
  const disabled = busy ? " disabled" : "";
  const id = escapeHtml(row.id);
  const swap = `<button class="profile-pill" data-swap="${id}" type="button"${disabled}>${SWAP_ICON}切换</button>`;
  const reauth = `<button class="profile-pill" data-reauth="${id}" type="button" title="给这张卡重新走浏览器授权"${disabled}>${KEY_ICON}重新授权</button>`;
  const remove = `<button class="profile-pill profile-pill--danger" data-delete="${id}" type="button"${disabled}>${TRASH_ICON}删除</button>`;
  if (row.active) {
    return `${reauth}
           ${remove}`;
  }
  return `${swap}
           ${reauth}
           ${remove}`;
}

function profileStatusHtml(row: AccountRow): string {
  if (row.credential_expired) {
    return libraryCredentialRenewalHtml();
  }
  if (row.active) {
    return "";
  }
  return `<span class="profile-updated">已加入账号库</span>`;
}

/** 账号库一张卡的 HTML。busy 只用于首绘；之后由 listedActionSelector 就地改 disabled。 */
export function accountCardHtml(row: AccountRow, busy: boolean): string {
  const plan = membershipPlanHtml(row) ?? "";
  /* 长名补读（issue #167）：标题单行省略归 CSS，完整显示名写进 title 供悬停/读屏；
   * 值沿用 accountDisplayName 后端单一出处，前端不重推官方昵称优先级。 */
  const displayName = accountDisplayName(row, row.id);
  return `<article class="profile-card${row.active ? " is-current" : ""}" ${accountFocusAttr(row.id)}>
        ${renameOverflowHtml(row, busy)}
        <div class="profile-identity">
          <span class="profile-avatar" aria-hidden="true">${escapeHtml(displayInitial(row.display_name))}</span>
          <div class="profile-identity-copy">
            <div class="profile-title-row">
              <span class="profile-title" title="${escapeHtml(displayName)}">${escapeHtml(displayName)}</span>
              ${currentAccountChipHtml(row.active)}
            </div>
            <span class="profile-plan">${plan}${bindWebButtonHtml(row, busy)}</span>
            ${profileStatusHtml(row)}
            ${autoSwapOptOutSwitchHtml(row, busy)}
          </div>
        </div>
        <div class="profile-quota-block">
          <span class="profile-quota-heading">额度使用情况</span>
          <div class="profile-quota-stack">${quotaStackHtml(row)}</div>
        </div>
        <div class="profile-card-actions">${cardActionButtons(row, busy)}</div>
      </article>`;
}
