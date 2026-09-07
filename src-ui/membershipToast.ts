import { commandError } from "./api";

/** 刷新失败降级卡：hint 亮且已有到期日（缓存日期仍在）。从未绑定不纳入。 */
export interface MembershipToastAccount {
  display_name: string;
  membership_expiry: string | null;
  membership_expiry_hint: boolean;
}

/** 用户点「刷新」后的瞬时普通 toast 文案。无失败卡返回 null。 */
export function membershipRefreshToast(
  accounts: readonly MembershipToastAccount[],
): string | null {
  const failed = accounts.filter(
    (row) => row.membership_expiry_hint && row.membership_expiry,
  );
  if (failed.length === 0) {
    return null;
  }
  const names = failed.map((row) => row.display_name).join("、");
  return `会员到期未更新：${names} 网页登录已失效，可重新绑定`;
}

/** 用户主动刷新列出成功之后调用。启动 / 静默 / 定时列出不要走这里。 */
export function afterUserQuotaRefreshSuccess(
  accounts: readonly MembershipToastAccount[],
  toast: (text: string) => void,
): void {
  const text = membershipRefreshToast(accounts);
  if (text !== null) {
    toast(text);
  }
}

export async function runUserQuotaRefresh<
  T extends {
    accounts: readonly MembershipToastAccount[];
    message?: { text: string } | null;
  },
>(opts: {
  list: () => Promise<T>;
  applySnapshot: (snapshot: T) => void;
  toast: (text: string, isError?: boolean) => void;
}): Promise<void> {
  try {
    const snapshot = await opts.list();
    opts.applySnapshot(snapshot);
    afterUserQuotaRefreshSuccess(snapshot.accounts, (text) => opts.toast(text));
  } catch (error) {
    opts.toast(commandError(error).message, true);
  }
}
