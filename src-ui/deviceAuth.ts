import { commandError, type AuthLink, type Snapshot } from "./api";
import { deviceAuthBusyLabel } from "./labels";

export interface DeviceAuthDialog {
  isOpen(): boolean;
  show(input: { title: string; copy: string }): void;
  close(): void;
  setLink(url: string, userCode: string): void;
  setCopy(text: string): void;
}

export interface DeviceAuthHost {
  isBusy(): boolean;
  setBusy(busy: boolean, label?: string): void;
  reserveGeneration(): number;
  applySnapshot(snapshot: Snapshot, generation: number): void;
  showError(message: string): void;
}

export interface DeviceAuthCommands {
  startDeviceAuth(target: string | null): Promise<AuthLink>;
  completeDeviceAuth(): Promise<Snapshot>;
  cancelDeviceAuth(): Promise<void> | void;
}

function authTitle(target: string | null, displayName: string): string {
  return target === null ? "添加账号 · 浏览器授权" : `重新授权账号：${displayName}`;
}

function waitingCopy(target: string | null, displayName: string): string {
  return target === null
    ? "复制下面的链接，到浏览器打开并登录授权。完成后会自动添加。"
    : `复制下面的链接，到浏览器打开并登录。完成后会把新凭证写回「${displayName}」，登录的官方账号对不上会被拒绝。`;
}

function shouldShowAuthError(kind: string, dialogOpen: boolean): boolean {
  return dialogOpen && kind !== "cancelled" && kind !== "no_pending_auth";
}

/** 设备码授权编排：取消竞态与迟到错误过滤都在这里，不 grep 中文文案。 */
export async function runDeviceAuth(
  target: string | null,
  displayName: string,
  deps: {
    dialog: DeviceAuthDialog;
    host: DeviceAuthHost;
    commands: DeviceAuthCommands;
  },
): Promise<void> {
  const { dialog, host, commands } = deps;
  if (host.isBusy()) {
    return;
  }
  dialog.show({ title: authTitle(target, displayName), copy: "正在获取授权链接…" });
  host.setBusy(true, deviceAuthBusyLabel("fetching-link"));
  const generation = host.reserveGeneration();
  try {
    const link = await commands.startDeviceAuth(target);
    if (!dialog.isOpen()) {
      void commands.cancelDeviceAuth();
      return;
    }
    dialog.setLink(link.url, link.user_code || "----");
    host.setBusy(true, deviceAuthBusyLabel("waiting-browser"));
    dialog.setCopy(waitingCopy(target, displayName));
    const snapshotAfter = await commands.completeDeviceAuth();
    dialog.close();
    host.applySnapshot(snapshotAfter, generation);
  } catch (error) {
    const { kind, message } = commandError(error);
    if (shouldShowAuthError(kind, dialog.isOpen())) {
      host.showError(message);
    }
    dialog.close();
  } finally {
    host.setBusy(false);
  }
}
