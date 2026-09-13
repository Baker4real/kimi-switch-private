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
    await api.listAccounts(true);
    await api.listAccounts(false, true);
    await api.listAccounts(true, true);
    await api.importCurrent();
    await api.swapAccount("acc-1");
    await api.removeAccount("acc-2");
    await api.renameAccount("acc-3", "工作机");
    await api.startDeviceAuth(null);
    await api.startDeviceAuth("acc-4");
    await api.completeDeviceAuth();
    await api.cancelDeviceAuth();
    await api.bindWebAccount("acc-5", "web-token");
    await api.openUrl("https://auth.kimi.com/x");
    await api.setProactiveRenewal(true);
    await api.setAutoSwap(true);
    await api.setAutoSwapPreferLowerTier(true);
    await api.setAutoSwapResidentLowerTier(true);
    await api.setAutoSwapPreResetBurn(true);
    await api.setAutoSwapPacePreSwap(true);
    await api.setAutoSwapOptOut("acc-6", true);

    expect(invoke.mock.calls).toEqual([
      ["list_accounts"],
      ["list_accounts", { userInitiated: true }],
      // currentOnly（issue #161 加密轮）必须走 camelCase 键：Tauri v2 命令参数
      // 是 snake_case `current_only`，键名写错两侧测试都探测不到。
      ["list_accounts", { currentOnly: true }],
      ["list_accounts", { userInitiated: true, currentOnly: true }],
      ["import_current"],
      ["swap_account", { id: "acc-1" }],
      ["remove_account", { id: "acc-2" }],
      ["rename_account", { id: "acc-3", label: "工作机" }],
      ["start_device_auth", { target: null }],
      ["start_device_auth", { target: "acc-4" }],
      ["complete_device_auth"],
      ["cancel_device_auth"],
      ["bind_web_account", { id: "acc-5", token: "web-token" }],
      ["open_url", { url: "https://auth.kimi.com/x" }],
      ["set_proactive_renewal", { enabled: true }],
      ["set_auto_swap", { enabled: true }],
      ["set_auto_swap_prefer_lower_tier", { enabled: true }],
      ["set_auto_swap_resident_lower_tier", { enabled: true }],
      ["set_auto_swap_pre_reset_burn", { enabled: true }],
      ["set_auto_swap_pace_pre_swap", { enabled: true }],
      ["set_auto_swap_opt_out", { id: "acc-6", enabled: true }],
    ]);
  });

  it("不提供列出/刷新重复命令或主题类死命令", () => {
    const api = createApi(mockInvoke());
    expect(api).not.toHaveProperty("refreshAccounts");
    expect(api).not.toHaveProperty("setTheme");
    expect(api).not.toHaveProperty("getTheme");
    expect("refresh_accounts" in api).toBe(false);
    expect("set_theme" in api).toBe(false);
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
