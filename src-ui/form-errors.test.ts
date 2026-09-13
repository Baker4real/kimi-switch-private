import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it, vi } from "vitest";

import {
  evaluateBindSubmit,
  evaluateRenameSubmit,
  paintActionRetry,
  runSwapFlow,
} from "./main";
import type { Snapshot } from "./api";

const html = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "index.html"),
  "utf8",
).replace(/\r\n/g, "\n");

const css = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "styles.css"),
  "utf8",
).replace(/\r\n/g, "\n");

function fieldAround(controlId: string): string {
  const idx = html.indexOf(`id="${controlId}"`);
  expect(idx).toBeGreaterThanOrEqual(0);
  const start = html.lastIndexOf("<label", idx);
  const end = html.indexOf("</label>", idx);
  expect(start).toBeGreaterThanOrEqual(0);
  expect(end).toBeGreaterThan(start);
  return html.slice(start, end + "</label>".length);
}

function tagWithId(fragment: string, id: string): string {
  const idx = fragment.indexOf(`id="${id}"`);
  expect(idx).toBeGreaterThanOrEqual(0);
  const start = fragment.lastIndexOf("<", idx);
  const end = fragment.indexOf(">", idx);
  return fragment.slice(start, end + 1);
}

function attr(tag: string, name: string): string | null {
  const match = new RegExp(`${name}="([^"]*)"`).exec(tag);
  return match?.[1] ?? null;
}

function dialogAround(dialogId: string): string {
  const start = html.indexOf(`id="${dialogId}"`);
  expect(start).toBeGreaterThanOrEqual(0);
  const open = html.lastIndexOf("<dialog", start);
  const end = html.indexOf("</dialog>", start);
  return html.slice(open, end + "</dialog>".length);
}

describe("表单字段就近错误（issue #84）", () => {
  it("别名字段 aria-describedby 指向同一 field 内的错误节点", () => {
    const field = fieldAround("rename-input");
    const describedBy = attr(tagWithId(field, "rename-input"), "aria-describedby");
    expect(describedBy).toBe("rename-error");
    expect(field).toContain('id="rename-error"');
    expect(field).toMatch(/id="rename-error"[^>]*class="[^"]*field-error/);
  });

  it("网页 token 字段 aria-describedby 指向同一 field 内的错误节点", () => {
    const field = fieldAround("bind-token");
    const describedBy = attr(tagWithId(field, "bind-token"), "aria-describedby");
    expect(describedBy).toBe("bind-token-error");
    expect(field).toContain('id="bind-token-error"');
    expect(field).toMatch(/id="bind-token-error"[^>]*class="[^"]*field-error/);
  });

  it("字段错误默认隐藏，不占用对话框主文案", () => {
    expect(tagWithId(html, "rename-error")).toMatch(/\bhidden\b/);
    expect(tagWithId(html, "bind-token-error")).toMatch(/\bhidden\b/);
  });
});

describe("操作失败就近重试（issue #84）", () => {
  it("总览首载失败仍提供 #overview-retry-button", () => {
    const block = html.slice(
      html.indexOf('id="overview-load-error"'),
      html.indexOf('id="overview-dashboard"'),
    );
    expect(block).toContain('id="overview-retry-button"');
    expect(block).toContain("重试");
  });

  it("重命名、绑定、删除、授权对话框带就近重试按钮", () => {
    for (const dialogId of ["rename-dialog", "bind-dialog", "delete-dialog", "auth-dialog"]) {
      const dialog = dialogAround(dialogId);
      expect(dialog, dialogId).toMatch(/id="[a-z-]+-retry-button"/);
      expect(dialog, dialogId).toContain("重试");
    }
    expect(dialogAround("rename-dialog")).toContain('id="rename-retry-button"');
    expect(dialogAround("bind-dialog")).toContain('id="bind-retry-button"');
    expect(dialogAround("delete-dialog")).toContain('id="delete-retry-button"');
    expect(dialogAround("auth-dialog")).toContain('id="auth-retry-button"');
  });

  it("账号库切换失败有就近重试，不要求回到某一张卡上再点", () => {
    const library = html.slice(
      html.indexOf('id="page-library"'),
      html.indexOf('id="auth-dialog"'),
    );
    expect(library).toContain('id="library-action-error"');
    expect(library).toContain('id="library-retry-button"');
    expect(library).toContain("重试");
  });
});

describe("字段错误样式（issue #84）", () => {
  it("字段旁错误用危险色，不改焦点环规则", () => {
    expect(css).toMatch(/\.field-error\s*\{/);
    const start = css.indexOf(".field-error {");
    const body = css.slice(start, css.indexOf("}", start) + 1);
    expect(body).toContain("var(--danger-text)");
    expect(css).toMatch(
      /button:focus-visible,\s*input:focus-visible,\s*textarea:focus-visible,\s*a:focus-visible \{/,
    );
  });
});

function fakeField(errorId: string): {
  input: { setAttribute(name: string, value: string): void; removeAttribute(name: string): void; attrs: Record<string, string> };
  error: {
    id: string;
    textContent: string;
    classList: { add(token: string): void; remove(token: string): void };
    hidden: boolean;
  };
} {
  const classes = new Set<string>(["hidden"]);
  const attrs: Record<string, string> = { "aria-describedby": errorId };
  return {
    input: {
      attrs,
      setAttribute(name: string, value: string) {
        attrs[name] = value;
      },
      removeAttribute(name: string) {
        delete attrs[name];
      },
    },
    error: {
      id: errorId,
      textContent: "",
      classList: {
        add(token: string) {
          classes.add(token);
        },
        remove(token: string) {
          classes.delete(token);
        },
      },
      get hidden() {
        return classes.has("hidden");
      },
    },
  };
}

describe("校验失败保持对话框打开（issue #84）", () => {
  it("空别名时错误经 aria-describedby 指向字段旁节点，对话框仍 open", () => {
    const dialog = {
      open: true,
      close() {
        this.open = false;
      },
    };
    const { input, error } = fakeField("rename-error");
    const label = evaluateRenameSubmit("   ", input, error);
    if (label !== null) {
      dialog.close();
    }
    expect(label).toBeNull();
    expect(dialog.open).toBe(true);
    expect(error.textContent).toBe("别名不能为空");
    expect(error.hidden).toBe(false);
    expect(input.attrs["aria-describedby"]).toBe("rename-error");
    expect(input.attrs["aria-invalid"]).toBe("true");
  });

  it("空 token 时错误经 aria-describedby 指向字段旁节点，对话框仍 open", () => {
    const dialog = {
      open: true,
      close() {
        this.open = false;
      },
    };
    const { input, error } = fakeField("bind-token-error");
    const token = evaluateBindSubmit("", input, error);
    if (token !== null) {
      dialog.close();
    }
    expect(token).toBeNull();
    expect(dialog.open).toBe(true);
    expect(error.textContent).toBe("请先粘贴网页 token");
    expect(error.hidden).toBe(false);
    expect(input.attrs["aria-describedby"]).toBe("bind-token-error");
    expect(input.attrs["aria-invalid"]).toBe("true");
  });

  it("合法别名通过校验并清掉字段错误，仍把关闭留给提交成功之后", () => {
    const dialog = {
      open: true,
      close() {
        this.open = false;
      },
    };
    const { input, error } = fakeField("rename-error");
    const label = evaluateRenameSubmit("工作机", input, error);
    expect(label).toBe("工作机");
    expect(dialog.open).toBe(true);
    expect(error.textContent).toBe("");
    expect(error.hidden).toBe(true);
    expect(input.attrs["aria-invalid"]).toBeUndefined();
  });
});

describe("提交接到字段校验与就近重试（issue #84）", () => {
  it("main 校验失败走 evaluate* 并 return，不关对话框；重试按钮接到同一提交", () => {
    const main = readFileSync(
      join(dirname(fileURLToPath(import.meta.url)), "main.ts"),
      "utf8",
    );
    expect(main).toContain("evaluateRenameSubmit(");
    expect(main).toContain("evaluateBindSubmit(");
    expect(main).not.toContain('showToast("别名不能为空"');
    expect(main).not.toContain('showToast("请先粘贴网页 token"');
    expect(main).toContain('required<HTMLButtonElement>("rename-retry-button")');
    expect(main).toContain('required<HTMLButtonElement>("bind-retry-button")');
    expect(main).toContain('required<HTMLButtonElement>("delete-retry-button")');
    expect(main).toContain('required<HTMLButtonElement>("auth-retry-button")');
    expect(main).toContain('required<HTMLButtonElement>("library-retry-button")');
    expect(main).toContain("showLibraryRetry(lastActionError, retrySwap)");
  });
});

describe("操作失败重试面板（issue #84）", () => {
  it("失败时显示就近重试文案，成功时收起", () => {
    const classes = new Set<string>(["hidden"]);
    const panel = {
      classList: {
        add(token: string) {
          classes.add(token);
        },
        remove(token: string) {
          classes.delete(token);
        },
      },
    };
    const text = { textContent: "" };
    paintActionRetry(panel, text, "切换失败");
    expect(text.textContent).toBe("切换失败");
    expect(classes.has("hidden")).toBe(false);
    paintActionRetry(panel, text, null);
    expect(text.textContent).toBe("");
    expect(classes.has("hidden")).toBe(true);
  });
});

/** 切换流测试夹具（issue #157）：记录宿主调用顺序的假宿主 + 两账号快照。 */
function swapFlowFixture(currentSnapshot: Snapshot | null) {
  const calls: string[] = [];
  const busy = { value: false };
  const host = {
    isBusy: () => busy.value,
    setBusy(next: boolean, label?: string) {
      busy.value = next;
      calls.push(`busy:${next}:${label ?? ""}`);
    },
    reserveGeneration: () => {
      calls.push("reserve");
      return 7;
    },
    currentSnapshot: () => currentSnapshot,
    applySnapshot(next: Snapshot, generation: number) {
      const active = next.accounts.find((row) => row.active);
      calls.push(`apply:${generation}:${active ? active.id : "none"}`);
    },
    showToast(text: string, isError = false) {
      calls.push(`toast:${isError}:${text}`);
    },
    reportError(message: string) {
      calls.push(`error:${message}`);
    },
    refreshInBackground() {
      calls.push("refresh");
    },
  };
  return { calls, busy, host };
}

function twoAccountSnapshot(): Snapshot {
  const row = (id: string, active: boolean) => ({
    id,
    label: id,
    active,
    membership: null,
    membership_expiry: null,
    membership_remaining_days: null,
    membership_period_days: null,
    membership_expiry_hint: false,
    quotas: [
      { window: "5小时", ratio: 0.1, text: `${id} 用量`, reset: null, tone: "ok" as const },
    ],
    error: null,
    credential_expired: false,
    cached_at: "更新于 09-12 10:00",
    display_name: id,
    official_nickname_known: false,
    auto_swap_opt_out: false,
  });
  return {
    accounts: [row("alice", true), row("bob", false)],
    current_label: "alice",
    account_count: 2,
    urgent_count: 0,
    proactive_renewal: false,
    auto_swap: false,
    auto_swap_prefer_lower_tier: false,
    auto_swap_resident_lower_tier: false,
    auto_swap_pre_reset_burn: false,
    auto_swap_pace_pre_swap: false,
    message: null,
  };
}

const swapOutcome = {
  account_id: "bob",
  display_name: "bob",
  message: { text: "已切换到 bob", tone: "ok" as const, kind: "generic" as const },
  credential_warning: null,
};

describe("手动切换与额度快照重列解耦（issue #157）", () => {
  it("切换成功立即应用当前账号标记并弹成功 toast，先于忙碌释放触发后台刷新", async () => {
    const { calls, busy, host } = swapFlowFixture(twoAccountSnapshot());
    const swapAccount = vi.fn().mockResolvedValue(swapOutcome);

    const ok = await runSwapFlow("bob", host, swapAccount);

    expect(ok).toBe(true);
    expect(swapAccount).toHaveBeenCalledWith("bob");
    expect(calls).toEqual([
      "busy:true:正在切换…",
      "reserve",
      "apply:7:bob",
      "toast:false:已切换到 bob",
      "refresh",
      "busy:false:",
    ]);
    expect(busy.value).toBe(false);
  });

  it("切到本地已确认失效的卡（issue #156）：切换照常成功，警告叠加在成功 toast 上按错误样式弹出", async () => {
    const { calls, busy, host } = swapFlowFixture(twoAccountSnapshot());
    const swapAccount = vi.fn().mockResolvedValue({
      ...swapOutcome,
      credential_warning: "该账号凭证已失效，建议重新授权",
    });

    const ok = await runSwapFlow("bob", host, swapAccount);

    expect(ok).toBe(true);
    // 成功提示不被顶掉：单条 toast 同时承载「已切换到」与「建议重新授权」。
    expect(calls).toEqual([
      "busy:true:正在切换…",
      "reserve",
      "apply:7:bob",
      "toast:true:已切换到 bob；该账号凭证已失效，建议重新授权",
      "refresh",
      "busy:false:",
    ]);
    expect(busy.value).toBe(false);
  });

  it("后台刷新未完成时再次切换正常工作：忙碌已释放，第二次流程照常进行", async () => {
    // refreshInBackground 只发通知、不持有任何闸门：模拟一个永不完成的后台刷新，
    // 第二次切换仍应完整走完并成功。
    let refreshes = 0;
    const first = swapFlowFixture(twoAccountSnapshot());
    const host = {
      ...first.host,
      refreshInBackground() {
        refreshes += 1;
      },
    };
    const ok1 = await runSwapFlow("bob", host, vi.fn().mockResolvedValue(swapOutcome));
    expect(ok1).toBe(true);
    expect(refreshes).toBe(1);
    expect(first.busy.value).toBe(false);

    const ok2 = await runSwapFlow("alice", host, vi.fn().mockResolvedValue(swapOutcome));
    expect(ok2).toBe(true);
    expect(refreshes).toBe(2);
    expect(first.busy.value).toBe(false);
  });

  it("忙碌时直接失败：不发切换命令、不占世代", async () => {
    const { calls, host } = swapFlowFixture(twoAccountSnapshot());
    host.setBusy(true, "处理中…");
    calls.length = 0;
    const swapAccount = vi.fn();

    const ok = await runSwapFlow("bob", host, swapAccount);

    expect(ok).toBe(false);
    expect(swapAccount).not.toHaveBeenCalled();
    expect(calls).toEqual([]);
  });

  it("切换失败走 reportError 并释放忙碌，不应用视图、不触发后台刷新", async () => {
    const { calls, busy, host } = swapFlowFixture(twoAccountSnapshot());
    const swapAccount = vi
      .fn()
      .mockRejectedValue({ kind: "switch_lock_timeout", message: "另一切换操作未完成，请重试" });

    const ok = await runSwapFlow("bob", host, swapAccount);

    expect(ok).toBe(false);
    expect(calls).toEqual([
      "busy:true:正在切换…",
      "reserve",
      "error:另一切换操作未完成，请重试",
      "busy:false:",
    ]);
    expect(busy.value).toBe(false);
  });

  it("无既有快照时不做本地翻转，仍弹成功 toast 并触发后台刷新", async () => {
    const { calls, host } = swapFlowFixture(null);
    const swapAccount = vi.fn().mockResolvedValue(swapOutcome);

    const ok = await runSwapFlow("bob", host, swapAccount);

    expect(ok).toBe(true);
    expect(calls).toEqual([
      "busy:true:正在切换…",
      "reserve",
      "toast:false:已切换到 bob",
      "refresh",
      "busy:false:",
    ]);
  });
});
