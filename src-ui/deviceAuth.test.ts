import { describe, expect, it, vi } from "vitest";

import { CommandError, type AuthLink, type Snapshot } from "./api";
import {
  runDeviceAuth,
  type DeviceAuthCommands,
  type DeviceAuthDialog,
  type DeviceAuthHost,
} from "./deviceAuth";
import { deviceAuthBusyLabel } from "./labels";

function sampleSnapshot(): Snapshot {
  return {
    accounts: [],
    current_label: "--",
    account_count: 0,
    urgent_count: 0,
    theme: "classic",
    message: { text: "授权成功，已添加账号 u1", tone: "ok" },
  };
}

function sampleLink(): AuthLink {
  return { url: "https://auth.kimi.com/device", user_code: "ABCD" };
}

function createDialog(): DeviceAuthDialog & { open: boolean; title: string; copy: string } {
  const state = {
    open: false,
    title: "",
    copy: "",
    url: "",
    userCode: "",
    isOpen() {
      return state.open;
    },
    show(input: { title: string; copy: string }) {
      state.open = true;
      state.title = input.title;
      state.copy = input.copy;
      state.url = "";
      state.userCode = "----";
    },
    close() {
      state.open = false;
    },
    setLink(url: string, userCode: string) {
      state.url = url;
      state.userCode = userCode;
    },
    setCopy(text: string) {
      state.copy = text;
    },
  };
  return state;
}

function createHost(): DeviceAuthHost & {
  errors: string[];
  snapshots: Snapshot[];
  busy: boolean;
  busyLabels: string[];
} {
  const state = {
    busy: false,
    errors: [] as string[],
    snapshots: [] as Snapshot[],
    busyLabels: [] as string[],
    isBusy() {
      return state.busy;
    },
    setBusy(next: boolean, label?: string) {
      state.busy = next;
      if (label) {
        state.busyLabels.push(label);
      }
    },
    reserveGeneration() {
      return 1;
    },
    applySnapshot(snapshot: Snapshot) {
      state.snapshots.push(snapshot);
    },
    showError(message: string) {
      state.errors.push(message);
    },
  };
  return state;
}

describe("runDeviceAuth", () => {
  it("忙碌时直接返回，不打开对话框", async () => {
    const dialog = createDialog();
    const host = createHost();
    host.busy = true;
    const commands: DeviceAuthCommands = {
      startDeviceAuth: vi.fn(),
      completeDeviceAuth: vi.fn(),
      cancelDeviceAuth: vi.fn(),
    };

    await runDeviceAuth(null, "", { dialog, host, commands });

    expect(dialog.open).toBe(false);
    expect(commands.startDeviceAuth).not.toHaveBeenCalled();
  });

  it("链接请求迟到失败且对话框已关时不弹错误（issue #6）", async () => {
    const dialog = createDialog();
    const host = createHost();
    let rejectStart!: (error: unknown) => void;
    const start = new Promise<AuthLink>((_, reject) => {
      rejectStart = reject;
    });
    const commands: DeviceAuthCommands = {
      startDeviceAuth: () => start,
      completeDeviceAuth: vi.fn(),
      cancelDeviceAuth: vi.fn(),
    };

    const running = runDeviceAuth(null, "", { dialog, host, commands });
    expect(dialog.open).toBe(true);
    dialog.close();
    rejectStart({ kind: "provider", message: "获取授权链接失败" });
    await running;

    expect(host.errors).toEqual([]);
    expect(host.busy).toBe(false);
    expect(commands.completeDeviceAuth).not.toHaveBeenCalled();
  });

  it("对话框仍打开时的授权失败照常弹错误", async () => {
    const dialog = createDialog();
    const host = createHost();
    const commands: DeviceAuthCommands = {
      startDeviceAuth: () =>
        Promise.reject(new CommandError("provider", "获取授权链接失败")),
      completeDeviceAuth: vi.fn(),
      cancelDeviceAuth: vi.fn(),
    };

    await runDeviceAuth(null, "", { dialog, host, commands });

    expect(host.errors).toEqual(["获取授权链接失败"]);
    expect(dialog.open).toBe(false);
  });

  it("cancelled 按 kind 静默，即使文案不含取消二字", async () => {
    const dialog = createDialog();
    const host = createHost();
    const commands: DeviceAuthCommands = {
      startDeviceAuth: () => Promise.resolve(sampleLink()),
      completeDeviceAuth: () =>
        Promise.reject(new CommandError("cancelled", "wording that never mentions cancel")),
      cancelDeviceAuth: vi.fn(),
    };

    await runDeviceAuth("acc-1", "工作机", { dialog, host, commands });

    expect(host.errors).toEqual([]);
    expect(dialog.open).toBe(false);
  });

  it("no_pending_auth 按 kind 静默", async () => {
    const dialog = createDialog();
    const host = createHost();
    const commands: DeviceAuthCommands = {
      startDeviceAuth: () => Promise.resolve(sampleLink()),
      completeDeviceAuth: () =>
        Promise.reject(new CommandError("no_pending_auth", "session already cleared")),
      cancelDeviceAuth: vi.fn(),
    };

    await runDeviceAuth(null, "", { dialog, host, commands });

    expect(host.errors).toEqual([]);
  });

  it("拿到链接前用户已关窗则取消后台会话，不再 complete", async () => {
    const dialog = createDialog();
    const host = createHost();
    let resolveStart!: (link: AuthLink) => void;
    const start = new Promise<AuthLink>((resolve) => {
      resolveStart = resolve;
    });
    const commands: DeviceAuthCommands = {
      startDeviceAuth: () => start,
      completeDeviceAuth: vi.fn(),
      cancelDeviceAuth: vi.fn(),
    };

    const running = runDeviceAuth(null, "", { dialog, host, commands });
    dialog.close();
    resolveStart(sampleLink());
    await running;

    expect(commands.cancelDeviceAuth).toHaveBeenCalledOnce();
    expect(commands.completeDeviceAuth).not.toHaveBeenCalled();
    expect(host.snapshots).toEqual([]);
    expect(host.errors).toEqual([]);
  });

  it("授权完成则关窗并套用快照", async () => {
    const dialog = createDialog();
    const host = createHost();
    const next = sampleSnapshot();
    const commands: DeviceAuthCommands = {
      startDeviceAuth: () => Promise.resolve(sampleLink()),
      completeDeviceAuth: () => Promise.resolve(next),
      cancelDeviceAuth: vi.fn(),
    };

    await runDeviceAuth(null, "", { dialog, host, commands });

    expect(dialog.title).toBe("添加账号 · 浏览器授权");
    expect(host.busyLabels).toEqual([
      deviceAuthBusyLabel("fetching-link"),
      deviceAuthBusyLabel("waiting-browser"),
    ]);
    expect(dialog.open).toBe(false);
    expect(host.snapshots).toEqual([next]);
    expect(host.errors).toEqual([]);
  });
});
