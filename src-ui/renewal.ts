/** 「当前账号主动续期」开关（issue #42）的展示决策。不碰 DOM。 */

export interface RenewalSwitchView {
  /** 开关是否处于开态（对应 aria-checked）。 */
  checked: boolean;
  /** 切换成功后的 toast 文案。 */
  toast: string;
}

/** 把持久化布尔值映射成开关视图状态。默认关由 Rust 侧 Settings 保证，这里只做映射。 */
export function renewalSwitchView(enabled: boolean): RenewalSwitchView {
  return {
    checked: enabled,
    toast: enabled ? "已开启当前账号主动续期" : "已关闭当前账号主动续期",
  };
}
