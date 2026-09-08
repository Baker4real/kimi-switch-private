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
 * 「优先低档与回驻」折叠面板（issue #116/#117）的开合决策。不碰 DOM、不读盘、不落盘。
 * override 只活在 GUI 会话内存：null=用户尚未点过（静态首帧收起）。
 * opened 是本会话「曾经展开过（含自动展开）」的闩，只由调用方写回模块级变量。
 */
export type AutoSwapNestedPanelOverride = "open" | "closed" | null;

export interface AutoSwapNestedPanelView {
  /** 面板是否展开（true 对应 aria-expanded="true" 且面板不隐藏）。 */
  expanded: boolean;
  /** 写回会话记忆的「曾展开过」闩；任何路径都不得清回 false。 */
  opened: boolean;
}

/**
 * 折叠面板开合真值表（issue #117）：
 * - override=open  → 展开
 * - override=closed → 收起
 * - override=null  → opened 或任一子开关为开则展开
 * 自动路径算出展开时把 opened 闩住；收起不改写 opened。
 */
export function autoSwapNestedPanelView(
  override: AutoSwapNestedPanelOverride,
  opened: boolean,
  preferLowerTier: boolean,
  residentLowerTier: boolean,
): AutoSwapNestedPanelView {
  if (override === "open") {
    return { expanded: true, opened: true };
  }
  if (override === "closed") {
    return { expanded: false, opened };
  }
  const expanded = opened || preferLowerTier || residentLowerTier;
  return { expanded, opened: expanded ? true : opened };
}

export interface AutoSwapNestedPanelToggleView extends AutoSwapNestedPanelView {
  /** 点击后写回会话记忆的 override 值。 */
  override: AutoSwapNestedPanelOverride;
}

/**
 * 点击折叠标题后的开合决策：目标开合取当前展开态的反面；目标为展开时把 opened 置 true，
 * 目标为收起时 opened 保持不变（不清回 false）。
 */
export function autoSwapNestedPanelToggle(
  override: AutoSwapNestedPanelOverride,
  opened: boolean,
  preferLowerTier: boolean,
  residentLowerTier: boolean,
): AutoSwapNestedPanelToggleView {
  const current = autoSwapNestedPanelView(
    override,
    opened,
    preferLowerTier,
    residentLowerTier,
  );
  const expanded = !current.expanded;
  return {
    expanded,
    override: expanded ? "open" : "closed",
    opened: expanded ? true : current.opened,
  };
}

/**
 * 折叠标题下方状态行文案（issue #117）：收起态也要标明哪项已开。
 * 都关时返回空串（不写状态行、折叠标题也不挂 aria-describedby）。
 */
export function autoSwapNestedStatusText(
  preferLowerTier: boolean,
  residentLowerTier: boolean,
): string {
  if (preferLowerTier && residentLowerTier) {
    return "优先低档、回驻已开";
  }
  if (preferLowerTier) {
    return "优先低档已开";
  }
  if (residentLowerTier) {
    return "回驻已开";
  }
  return "";
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
