import { describe, expect, it, vi } from "vitest";

import { CommandError, commandError, createApi } from "./api";

function mockInvoke() {
  return vi.fn().mockResolvedValue({ ok: true });
}

describe("createApi 动词与命令字符串", () => {
  it("每个动词只发对应 Tauri 命令", async () => {
    const invoke = mockInvoke();
    const api = createApi(invoke);

    await api.listAccounts();
    await api.importCurrent();
    await api.swapAccount("acc-1");
    await api.removeAccount("acc-2");
    await api.renameAccount("acc-3", "工作机");
    await api.startDeviceAuth(null);
    await api.startDeviceAuth("acc-4");
    await api.completeDeviceAuth();
    await api.cancelDeviceAuth();
    await api.openUrl("https://auth.kimi.com/x");
    await api.setTheme("jade-night");
    await api.setProactiveRenewal(true);

    expect(invoke.mock.calls).toEqual([
      ["list_accounts"],
      ["import_current"],
      ["swap_account", { id: "acc-1" }],
      ["remove_account", { id: "acc-2" }],
      ["rename_account", { id: "acc-3", label: "工作机" }],
      ["start_device_auth", { target: null }],
      ["start_device_auth", { target: "acc-4" }],
      ["complete_device_auth"],
      ["cancel_device_auth"],
      ["open_url", { url: "https://auth.kimi.com/x" }],
      ["set_theme", { theme: "jade-night" }],
      ["set_proactive_renewal", { enabled: true }],
    ]);
  });

  it("不提供列出/刷新重复命令或取主题死命令", () => {
    const api = createApi(mockInvoke());
    expect(api).not.toHaveProperty("refreshAccounts");
    expect(api).not.toHaveProperty("getTheme");
    expect("refresh_accounts" in api).toBe(false);
    expect("get_theme" in api).toBe(false);
  });
});

describe("CommandError", () => {
  it("invoke 拒绝的 {kind,message} 变成可按 kind 分支的 CommandError", async () => {
    const invoke = vi.fn().mockRejectedValue({
      kind: "cancelled",
      message: "wording that never mentions cancel",
    });
    const api = createApi(invoke);

    await expect(api.listAccounts()).rejects.toEqual(
      expect.objectContaining({
        name: "CommandError",
        kind: "cancelled",
        message: "wording that never mentions cancel",
      }),
    );
  });

  it("已经是 CommandError 时原样返回", () => {
    const err = new CommandError("no_pending_auth", "没有进行中的授权");
    expect(commandError(err)).toBe(err);
  });

  it("普通 Error 与未知值落成 other，不猜中文文案", () => {
    expect(commandError(new Error("获取授权链接失败"))).toEqual(
      expect.objectContaining({ kind: "other", message: "获取授权链接失败" }),
    );
    expect(commandError("boom")).toEqual(
      expect.objectContaining({ kind: "other", message: "boom" }),
    );
  });
});
