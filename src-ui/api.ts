import { invoke } from "@tauri-apps/api/core";

import type { QuotaView } from "./quota";
import type { ThemeId } from "./theme";

export type MessageTone = "ok" | "err" | "info";

export interface AccountRow {
  id: string;
  label: string;
  active: boolean;
  membership: string | null;
  quotas: QuotaView[];
  error: string | null;
  /** 凭证待续期：凭证过期型 401。此时 error 是引导文案，不计告急、不按错误红渲染。 */
  credential_expired: boolean;
  cached_at: string | null;
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
    listAccounts: () => call<Snapshot>("list_accounts"),
    importCurrent: () => call<Snapshot>("import_current"),
    swapAccount: (id: string) => call<Snapshot>("swap_account", { id }),
    removeAccount: (id: string) => call<Snapshot>("remove_account", { id }),
    renameAccount: (id: string, label: string) =>
      call<Snapshot>("rename_account", { id, label }),
    startDeviceAuth: (target: string | null) =>
      call<AuthLink>("start_device_auth", { target }),
    completeDeviceAuth: () => call<Snapshot>("complete_device_auth"),
    cancelDeviceAuth: () => call<void>("cancel_device_auth"),
    openUrl: (url: string) => call<void>("open_url", { url }),
    setTheme: (theme: ThemeId) => call<ThemeId>("set_theme", { theme }),
    setProactiveRenewal: (enabled: boolean) =>
      call<boolean>("set_proactive_renewal", { enabled }),
  };
}

export type Api = ReturnType<typeof createApi>;

export const api = createApi((cmd, args) => invoke(cmd, args));
