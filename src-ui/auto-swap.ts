import type { UiMessage } from "./api";

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

/** 90s 静默列出只保留自动切换通知；「刚刚更新过」等节流提示仍丢弃。 */
export function silentQuotaMessage(message: UiMessage | null): UiMessage | null {
  if (!message) {
    return null;
  }
  const { text } = message;
  if (
    text.startsWith("已自动切换到") ||
    text === "没有可自动切换的账号" ||
    text === "自动切换失败"
  ) {
    return message;
  }
  return null;
}
