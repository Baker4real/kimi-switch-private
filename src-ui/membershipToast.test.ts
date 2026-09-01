import { describe, expect, it, vi } from "vitest";

import { CommandError } from "./api";
import {
  afterUserQuotaRefreshSuccess,
  membershipRefreshToast,
  runUserQuotaRefresh,
} from "./membershipToast";

type ToastRow = Parameters<typeof membershipRefreshToast>[0][number];

function row(partial: Partial<ToastRow> = {}): ToastRow {
  return {
    display_name: "Forever",
    membership_expiry: "2027-08-27",
    membership_expiry_hint: true,
    ...partial,
  };
}

describe("membershipRefreshToast", () => {
  it("无刷新失败降级卡时返回 null", () => {
    expect(membershipRefreshToast([])).toBeNull();
    expect(
      membershipRefreshToast([row({ membership_expiry_hint: false })]),
    ).toBeNull();
  });

  it("单卡用显示名拼一条普通文案", () => {
    expect(membershipRefreshToast([row({ display_name: "Forever" })])).toBe(
      "会员有效期未更新：Forever 网页登录已失效，可重新绑定",
    );
  });

  it("别名兜底的显示名同样直接使用", () => {
    expect(membershipRefreshToast([row({ display_name: "工作机" })])).toBe(
      "会员有效期未更新：工作机 网页登录已失效，可重新绑定",
    );
  });

  it("多卡聚合成一条，按行序列出各显示名", () => {
    expect(
      membershipRefreshToast([
        row({ display_name: "Forever" }),
        row({ display_name: "工作机" }),
      ]),
    ).toBe("会员有效期未更新：Forever、工作机 网页登录已失效，可重新绑定");
  });

  it("从未绑定（hint 亮但无到期日）不纳入", () => {
    expect(
      membershipRefreshToast([
        row({ membership_expiry: null, membership_expiry_hint: true, display_name: "新卡" }),
      ]),
    ).toBeNull();
    expect(
      membershipRefreshToast([
        row({ display_name: "Forever" }),
        row({ membership_expiry: null, membership_expiry_hint: true, display_name: "新卡" }),
      ]),
    ).toBe("会员有效期未更新：Forever 网页登录已失效，可重新绑定");
  });
});

describe("afterUserQuotaRefreshSuccess", () => {
  it("有失败卡时弹瞬时普通 toast，可重复触发", () => {
    const toast = vi.fn();
    const accounts = [row({ display_name: "Forever" })];
    afterUserQuotaRefreshSuccess(accounts, toast);
    afterUserQuotaRefreshSuccess(accounts, toast);
    expect(toast).toHaveBeenCalledTimes(2);
    expect(toast).toHaveBeenNthCalledWith(
      1,
      "会员有效期未更新：Forever 网页登录已失效，可重新绑定",
    );
    expect(toast.mock.calls[0]).toHaveLength(1);
  });

  it("无失败卡不 toast", () => {
    const toast = vi.fn();
    afterUserQuotaRefreshSuccess([row({ membership_expiry_hint: false })], toast);
    expect(toast).not.toHaveBeenCalled();
  });
});

describe("runUserQuotaRefresh", () => {
  it("列出成功后先 apply 再弹会员 toast（覆盖快照 message）", async () => {
    const order: string[] = [];
    const snapshot = {
      accounts: [row({ display_name: "Forever" })],
      message: { text: "刚刚更新过", tone: "info" as const },
    };
    await runUserQuotaRefresh({
      list: async () => snapshot,
      applySnapshot: (next) => {
        order.push("apply");
        if (next.message) {
          order.push(`msg:${next.message.text}`);
        }
      },
      toast: (text, isError) => {
        order.push(isError ? `err:${text}` : `toast:${text}`);
      },
    });
    expect(order).toEqual([
      "apply",
      "msg:刚刚更新过",
      "toast:会员有效期未更新：Forever 网页登录已失效，可重新绑定",
    ]);
  });

  it("命令失败只弹错误 toast，不弹会员 toast，不 apply", async () => {
    const applySnapshot = vi.fn();
    const toast = vi.fn();
    await runUserQuotaRefresh({
      list: async () => {
        throw new CommandError("quota_fetch", "额度查询失败");
      },
      applySnapshot,
      toast,
    });
    expect(applySnapshot).not.toHaveBeenCalled();
    expect(toast).toHaveBeenCalledTimes(1);
    expect(toast).toHaveBeenCalledWith("额度查询失败", true);
  });
});

