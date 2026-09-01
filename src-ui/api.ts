import { invoke } from "@tauri-apps/api/core";

import type { QuotaView } from "./quota";
import type { ThemeId } from "./theme";

export type MessageTone = "ok" | "err" | "info";

export interface AccountRow {
  id: string;
  label: string;
  active: boolean;
  membership: string | null;
  /** 会员到期日（东八区 `YYYY-MM-DD`，Rust 侧已格式化；无到期数据为 null，不是错误）。
   * 用户发起刷新成功时是新值；刷新失败保留缓存旧值（issue #52）。 */
  membership_expiry: string | null;
  /** 剩余天数（到期展示日 − 今天，东八区日历日）。有到期日才有值：正=剩余、0=今天到期、负=已过期。
   * 前端只拼中文，不解析日期。 */
  membership_remaining_days: number | null;
  /** 当前会员周期天数（到期展示日 − currentStartTime，东八区日历日）。
   * 会员轨道「已度过比例」的分母；开始键缺失或坏数据为 null（不画填充）。 */
  membership_period_days: number | null;
  /** 档位在且（无到期日，或本次刷新中会员获取失败）。布尔算法由后端计算。
   * GUI：无日期 →「绑定后可显示有效期」+「绑定」；有日期的刷新失败态 → 无常驻提示句、「重新绑定」。 */
  membership_expiry_hint: boolean;
  quotas: QuotaView[];
  error: string | null;
  /** 凭证待续期：凭证过期型 401。此时 error 是引导文案，不计告急、不按错误红渲染。 */
  credential_expired: boolean;
  cached_at: string | null;
  /** 后端计算的显示名（官方昵称 > 用户改过的别名 > 账号 id）。 */
  display_name: string;
  /** 官方昵称缓存非空。已知时隐藏「重命名」入口。 */
  official_nickname_known: boolean;
}

export interface UiMessage {
  text: string;
  tone: MessageTone;
}

export interface Snapshot {
  accounts: AccountRow[];
  current_label: string;
  account_count: number;
  urgent_count: number;
  theme: ThemeId;
  /** 「当前账号主动续期」开关（issue #42）：与 CLI 同源，来自 config.toml。 */
  proactive_renewal: boolean;
  message: UiMessage | null;
}

export interface AuthLink {
  url: string;
  user_code: string;
}

/** Tauri 命令字符串的唯一存放地；对外只暴露类型化动词。 */
export type InvokeFn = <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;

export class CommandError extends Error {
  readonly kind: string;

  constructor(kind: string, message: string) {
    super(message);
    this.name = "CommandError";
    this.kind = kind;
  }
}

export function commandError(error: unknown): CommandError {
  if (error instanceof CommandError) {
    return error;
  }
  if (error && typeof error === "object" && "kind" in error && "message" in error) {
    const { kind, message } = error as { kind: unknown; message: unknown };
    if (typeof kind === "string" && typeof message === "string") {
      return new CommandError(kind, message);
    }
  }
  if (error instanceof Error) {
    return new CommandError("other", error.message);
  }
  return new CommandError("other", String(error));
}

async function run<T>(action: () => Promise<T>): Promise<T> {
  try {
    return await action();
  } catch (error) {
    throw commandError(error);
  }
}

export function createApi(invokeFn: InvokeFn) {
  const call = <T>(cmd: string, args?: Record<string, unknown>): Promise<T> =>
    run(() => (args === undefined ? invokeFn<T>(cmd) : invokeFn<T>(cmd, args)));

  return {
    // Tauri v2 命令参数键为 camelCase（userInitiated），多词参数不得用 snake_case 键。
    listAccounts: (userInitiated = false) =>
      userInitiated
        ? call<Snapshot>("list_accounts", { userInitiated: true })
        : call<Snapshot>("list_accounts"),
    importCurrent: () => call<Snapshot>("import_current"),
    swapAccount: (id: string) => call<Snapshot>("swap_account", { id }),
    removeAccount: (id: string) => call<Snapshot>("remove_account", { id }),
    renameAccount: (id: string, label: string) =>
      call<Snapshot>("rename_account", { id, label }),
    startDeviceAuth: (target: string | null) =>
      call<AuthLink>("start_device_auth", { target }),
    completeDeviceAuth: () => call<Snapshot>("complete_device_auth"),
    cancelDeviceAuth: () => call<void>("cancel_device_auth"),
    /** 绑定网页登录（issue #51）：token 只经 Tauri IPC 发给后端管道，不落 store。 */
    bindWebAccount: (id: string, token: string) =>
      call<Snapshot>("bind_web_account", { id, token }),
    openUrl: (url: string) => call<void>("open_url", { url }),
    setTheme: (theme: ThemeId) => call<ThemeId>("set_theme", { theme }),
    setProactiveRenewal: (enabled: boolean) =>
      call<boolean>("set_proactive_renewal", { enabled }),
  };
}

export type Api = ReturnType<typeof createApi>;

export const api = createApi((cmd, args) => invoke(cmd, args));
