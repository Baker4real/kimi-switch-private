import type { UiMessage } from "./api";
import { escapeHtml } from "./escape";

/** 「额度耗尽自动切换」开关（issue #68）的展示决策。不碰 DOM。 */

export interface AutoSwapSwitchView {
  /** 开关是否处于开态（对应 aria-checked）。 */
  checked: boolean;
  /** 切换成功后的 toast 文案。 */
  toast: string;
}

/** 把持久化布尔值映射成开关视图状态。默认关由 Rust 侧 Settings 保证，这里只做映射。 */
export function autoSwapSwitchView(enabled: boolean): AutoSwapSwitchView {
  return {
    checked: enabled,
    toast: enabled ? "已开启额度耗尽自动切换" : "已关闭额度耗尽自动切换",
  };
}

/** 「自动切换时优先低档位账号」子开关（issue #71）的展示决策。不碰 DOM。 */
export interface AutoSwapPreferLowerTierSwitchView {
  /** 开关是否处于开态（对应 aria-checked）。总开关关闭时仍反映持久化值。 */
  checked: boolean;
  /** 总开关关闭时置灰，不可点。 */
  disabled: boolean;
  /** 切换成功后的 toast 文案。 */
  toast: string;
}

/** 总开关关只置灰子开关，不把持久化 checked 清掉。 */
export function autoSwapPreferLowerTierSwitchView(
  autoSwapEnabled: boolean,
  preferLowerTier: boolean,
): AutoSwapPreferLowerTierSwitchView {
  return {
    checked: preferLowerTier,
    disabled: !autoSwapEnabled,
    toast: preferLowerTier
      ? "已开启自动切换时优先低档位账号"
      : "已关闭自动切换时优先低档位账号",
  };
}

/** 「额度恢复后回驻低档账号」子开关（issue #95）的展示决策。不碰 DOM。 */
export interface AutoSwapResidentLowerTierSwitchView {
  /** 开关是否处于开态（对应 aria-checked）。总开关关闭时仍反映持久化值。 */
  checked: boolean;
  /** 总开关关闭时置灰，不可点。 */
  disabled: boolean;
  /** 切换成功后的 toast 文案。 */
  toast: string;
}

/** 总开关关只置灰子开关，不把持久化 checked 清掉。 */
export function autoSwapResidentLowerTierSwitchView(
  autoSwapEnabled: boolean,
  residentLowerTier: boolean,
): AutoSwapResidentLowerTierSwitchView {
  return {
    checked: residentLowerTier,
    disabled: !autoSwapEnabled,
    toast: residentLowerTier
      ? "已开启额度恢复后回驻低档账号"
      : "已关闭额度恢复后回驻低档账号",
  };
}

/**
 * 「账号行为」一层折叠收起态的状态行文案（issue #120，取代 #116/#117 内层折叠）：
 * 标题下用一行字写出当前生效项（「已开启：……」）。纯函数，不碰 DOM、不碰盘；由 paint 层只在收起时渲染。
 * 总开关关闭时不写「优先低档」/「回驻」（置灰不算生效项）；全关返回空串（不写状态行）。
 */
export function behaviorPanelStatusText(
  proactiveRenewal: boolean,
  autoSwapEnabled: boolean,
  preferLowerTier: boolean,
  residentLowerTier: boolean,
): string {
  const items: string[] = [];
  if (proactiveRenewal) {
    items.push("主动续期");
  }
  if (autoSwapEnabled) {
    items.push("自动切换");
    if (preferLowerTier) {
      items.push("优先低档");
    }
    if (residentLowerTier) {
      items.push("回驻");
    }
  }
  if (items.length === 0) {
    return "";
  }
  return `已开启：${items.join("、")}`;
}

/** 90s 静默列出只保留自动切换通知；「刚刚更新过」等节流提示仍丢弃。 */
export function silentQuotaMessage(message: UiMessage | null): UiMessage | null {
  if (!message) {
    return null;
  }
  switch (message.kind) {
    case "auto_swap_success":
    case "auto_swap_no_candidate":
    case "auto_swap_failed":
      return message;
    default:
      return null;
  }
}

/** 账号卡「摘出自动切换」开关（issue #112）的展示决策。不碰 DOM。 */
export interface AutoSwapOptOutSwitchView {
  /** 开关是否处于开态（对应 aria-checked）。真=已摘出。 */
  checked: boolean;
  /** 切换成功后的短 toast；实际 toast 由后端快照带显示名。 */
  toast: string;
}

export function autoSwapOptOutSwitchView(optedOut: boolean): AutoSwapOptOutSwitchView {
  return {
    checked: optedOut,
    toast: optedOut ? "已摘出自动切换" : "已恢复参与自动切换",
  };
}

/** 账号卡上的摘出开关：role=switch，状态可见。 */
export function autoSwapOptOutSwitchHtml(
  row: { id: string; auto_swap_opt_out: boolean },
  busy: boolean,
): string {
  const view = autoSwapOptOutSwitchView(row.auto_swap_opt_out);
  const disabled = busy ? " disabled" : "";
  return `<div class="profile-optout">
        <span class="profile-optout-label">摘出自动切换</span>
        <button class="switch" type="button" role="switch" aria-checked="${String(view.checked)}" data-optout="${escapeHtml(row.id)}" aria-label="摘出自动切换" title="不作为自动切换候选，手动切换不受影响"${disabled}>
          <span class="switch-thumb" aria-hidden="true"></span>
        </button>
      </div>`;
}
