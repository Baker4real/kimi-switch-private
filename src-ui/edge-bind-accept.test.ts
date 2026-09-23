import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

import { describe, expect, it, vi } from "vitest";
import { JSDOM } from "jsdom";

vi.mock("./api", async () => {
  const actual = await vi.importActual<typeof import("./api")>("./api");
  return {
    ...actual,
    api: {
      ...actual.api,
      beginWebBind: vi.fn(),
      completeWebBind: vi.fn(),
      bindWebAccount: vi.fn(),
      cancelWebBind: vi.fn(),
    },
  };
});

const dir = dirname(fileURLToPath(import.meta.url));
const workerSrc = readFileSync(join(dir, "../edge-extension/service-worker.js"), "utf8");
const contentSrc = readFileSync(join(dir, "../edge-extension/content.js"), "utf8");
const manifest = JSON.parse(
  readFileSync(join(dir, "../edge-extension/manifest.json"), "utf8"),
) as { host_permissions: string[]; content_scripts: Array<{ matches: string[] }> };
const html = readFileSync(join(dir, "index.html"), "utf8").replace(/\r\n/g, "\n");

const FIXTURE_TOKEN = "fixture-web-token";
const FIXTURE_ACCOUNT = "fixture-account";
const NONCE = "ab".repeat(24);
const PORT = 18080;

interface FetchCall {
  url: string;
  method: string;
  body?: string;
}

interface StatusScript {
  status: number;
  body?: { result: string; reason: string };
}

function createFetch(behavior: { post: number | "throw"; statuses: StatusScript[] }) {
  const calls: FetchCall[] = [];
  let gets = 0;
  const fetchImpl = async (url: string, init?: { method?: string; body?: string }) => {
    const method = init?.method ?? "GET";
    calls.push({ url: String(url), method, body: init?.body });
    if (method === "POST") {
      if (behavior.post === "throw") {
        throw new Error("connect");
      }
      return { status: behavior.post };
    }
    const next = behavior.statuses[Math.min(gets, behavior.statuses.length - 1)];
    gets += 1;
    return {
      status: next.status,
      json: async () => next.body ?? null,
    };
  };
  return { fetchImpl, calls, getCount: () => gets };
}

interface WorkerListener {
  (message: unknown, sender: unknown, sendResponse: (value: unknown) => void): boolean | undefined;
}

interface WorkerSandbox {
  chrome: {
    runtime: {
      onMessage: { addListener: (fn: WorkerListener) => void };
    };
  };
  fetch: (url: string, init?: { method?: string; body?: string }) => Promise<{ status: number }>;
  setTimeout: (fn: () => void, ms: number) => number;
  clearTimeout: () => void;
  listeners: WorkerListener[];
  timers: Array<{ fn: () => void; ms: number }>;
  scheduled: number[];
  outcome: unknown;
  responded: boolean;
}

function loadWorker(fetchImpl: WorkerSandbox["fetch"]): WorkerSandbox {
  const sandbox: WorkerSandbox = {
    chrome: { runtime: { onMessage: { addListener: (fn) => sandbox.listeners.push(fn) } } },
    fetch: fetchImpl,
    setTimeout: (fn, ms) => {
      sandbox.timers.push({ fn, ms });
      sandbox.scheduled.push(ms);
      return sandbox.scheduled.length;
    },
    clearTimeout: () => undefined,
    listeners: [],
    timers: [],
    scheduled: [],
    outcome: undefined,
    responded: false,
  };
  vm.runInContext(workerSrc, vm.createContext(sandbox));
  return sandbox;
}

async function drain(sandbox: WorkerSandbox): Promise<void> {
  for (let step = 0; step < 50; step += 1) {
    await new Promise((resolve) => setImmediate(resolve));
    if (sandbox.responded && sandbox.timers.length === 0) {
      return;
    }
    if (sandbox.timers.length === 0) {
      await new Promise((resolve) => setImmediate(resolve));
      if (sandbox.timers.length === 0) {
        break;
      }
    }
    sandbox.timers.shift()?.fn();
  }
  if (!sandbox.responded) {
    throw new Error("扩展没有把回查结果交回页面");
  }
}

async function deliver(
  behavior: { post: number | "throw"; statuses: StatusScript[] },
  message: Record<string, unknown> = {
    type: "kimi-switch-web-bind",
    nonce: NONCE,
    port: PORT,
    token: FIXTURE_TOKEN,
  },
) {
  const transport = createFetch(behavior);
  const sandbox = loadWorker(transport.fetchImpl);
  const listener = sandbox.listeners[0];
  expect(listener).toBeTypeOf("function");
  const keepAlive = listener(message, {}, (value) => {
    sandbox.outcome = value;
    sandbox.responded = true;
  });
  if (keepAlive) {
    await drain(sandbox);
  }
  return { sandbox, transport, keepAlive };
}

function assertLoopbackOnly(calls: FetchCall[]): void {
  expect(calls.length).toBeGreaterThan(0);
  for (const call of calls) {
    expect(call.url.startsWith(`http://127.0.0.1:${PORT}/kimi-switch/web-bind`)).toBe(true);
    expect(call.url).not.toContain(FIXTURE_TOKEN);
    expect(call.url).not.toContain("https://");
    expect(call.url).not.toContain("localhost");
  }
}

interface ToastNode {
  textContent: string;
  attrs: Record<string, string>;
  style: { cssText: string };
  removed: boolean;
  setAttribute: (name: string, value: string) => void;
  remove: () => void;
}

interface ContentSandbox {
  location: { hash: string; pathname: string; search: string };
  history: { replaceState: (state: null, title: string, url: string) => void };
  localStorage: { getItem: (key: string) => string | null };
  document: {
    createElement: (tag: string) => ToastNode;
    documentElement: { appendChild: (node: ToastNode) => void };
  };
  chrome: {
    runtime: {
      lastError: undefined;
      sendMessage: (message: { nonce: string; port: number; token: string }, callback: (response: unknown) => void) => void;
    };
  };
  setTimeout: (fn: () => void, ms: number) => number;
  fetch: () => never;
  nodes: ToastNode[];
  mounted: ToastNode[];
  messages: Array<{ nonce: string; port: number; token: string }>;
  storageReads: string[];
  store: Record<string, string>;
  replaced: string;
  timers: Array<{ fn: () => void; ms: number }>;
  reply: ((response: unknown) => void) | null;
}

function runContent(hash: string, token: string | null, reply?: unknown): ContentSandbox {
  const sandbox = {
    nodes: [],
    mounted: [],
    messages: [],
    storageReads: [],
    store: {},
    replaced: "",
    timers: [],
    reply: null,
  } as unknown as ContentSandbox;
  if (token !== null) {
    sandbox.store.access_token = token;
  }
  sandbox.location = { hash, pathname: "/code/console", search: "" };
  sandbox.history = {
    replaceState: (_state, _title, url) => {
      sandbox.replaced = url;
      sandbox.location.hash = "";
    },
  };
  sandbox.localStorage = {
    getItem: (key) => {
      sandbox.storageReads.push(key);
      return sandbox.store[key] ?? null;
    },
  };
  sandbox.document = {
    createElement: () => {
      const node: ToastNode = {
        textContent: "",
        attrs: {},
        style: { cssText: "" },
        removed: false,
        setAttribute: (name, value) => {
          node.attrs[name] = value;
        },
        remove: () => {
          node.removed = true;
        },
      };
      sandbox.nodes.push(node);
      return node;
    },
    documentElement: {
      appendChild: (node) => {
        sandbox.mounted.push(node);
      },
    },
  };
  sandbox.chrome = {
    runtime: {
      lastError: undefined,
      sendMessage: (message, callback) => {
        sandbox.messages.push(message);
        sandbox.reply = callback;
      },
    },
  };
  sandbox.setTimeout = (fn, ms) => {
    sandbox.timers.push({ fn, ms });
    return sandbox.timers.length;
  };
  sandbox.fetch = () => {
    throw new Error("页面脚本不得发请求");
  };
  vm.runInContext(contentSrc, vm.createContext(sandbox));
  if (reply !== undefined) {
    expect(sandbox.reply).toBeTypeOf("function");
    sandbox.reply!(reply);
  }
  return sandbox;
}

describe("扩展回查执行（issue #176 / #180）", () => {
  it("204 后有限轮询，终止态立即停止", async () => {
    const pending = { status: 200, body: { result: "pending", reason: "等待中" } };
    const success = { status: 200, body: { result: "success", reason: "绑定成功" } };
    const done = await deliver({ post: 204, statuses: [pending, pending, success] });
    expect(done.keepAlive).toBe(true);
    expect(done.transport.getCount()).toBe(3);
    expect(done.sandbox.scheduled).toEqual([2000, 2000]);
    expect(done.sandbox.outcome).toEqual({ result: "success", reason: "绑定成功" });
    assertLoopbackOnly(done.transport.calls);
    const post = done.transport.calls.find((call) => call.method === "POST");
    expect(JSON.parse(post?.body ?? "{}")).toEqual({ nonce: NONCE, token: FIXTURE_TOKEN });
    expect(done.transport.calls.some((call) => call.url.includes("web-bind-status") && call.url.includes(FIXTURE_TOKEN))).toBe(
      false,
    );

    const forever = await deliver({ post: 204, statuses: [pending] });
    expect(forever.transport.getCount()).toBe(12);
    expect(forever.sandbox.scheduled).toEqual(Array.from({ length: 11 }, () => 2000));
    expect(forever.sandbox.outcome).toEqual({ result: "failure", reason: "等待超时" });
  });

  it("非 204 只短探测，无可用网页凭证不合成等待超时", async () => {
    const noCredential = await deliver({
      post: 403,
      statuses: [{ status: 200, body: { result: "failure", reason: "无可用网页凭证" } }],
    });
    expect(noCredential.transport.getCount()).toBe(1);
    expect(noCredential.sandbox.scheduled).toEqual([]);
    expect(noCredential.sandbox.outcome).toEqual({ result: "failure", reason: "无可用网页凭证" });
    expect(noCredential.transport.calls.filter((call) => call.method === "GET")).toHaveLength(1);

    const absent = await deliver({ post: 404, statuses: [{ status: 404 }] });
    expect(absent.transport.getCount()).toBe(3);
    expect(absent.sandbox.scheduled).toEqual([250, 250]);
    expect(absent.sandbox.outcome).toBeNull();

    const thrown = await deliver({
      post: "throw",
      statuses: [{ status: 200, body: { result: "failure", reason: "无可用网页凭证" } }],
    });
    expect(thrown.transport.getCount()).toBe(1);
    expect(thrown.sandbox.outcome).toEqual({ result: "failure", reason: "无可用网页凭证" });
  });

  it("回查只打 127.0.0.1，会员失败细节不转发", async () => {
    expect(manifest.host_permissions).toEqual(["http://127.0.0.1/*"]);
    const hidden = await deliver({
      post: 204,
      statuses: [{ status: 200, body: { result: "success", reason: "会员信息获取失败，请稍后再试" } }],
    });
    expect(hidden.sandbox.outcome).toBeNull();
    expect(JSON.stringify(hidden.sandbox.outcome)).not.toContain("会员信息获取失败");
    const forwarded = await deliver({
      post: 204,
      statuses: [
        { status: 200, body: { result: "failure", reason: "账号身份不一致" } },
      ],
    });
    expect(forwarded.sandbox.outcome).toEqual({ result: "failure", reason: "账号身份不一致" });
    assertLoopbackOnly(forwarded.transport.calls);
  });

  it("错误消息不发请求", async () => {
    const idle = await deliver(
      { post: 204, statuses: [] },
      { type: "kimi-switch-web-bind", nonce: "nope", port: PORT, token: FIXTURE_TOKEN },
    );
    expect(idle.keepAlive).toBeUndefined();
    expect(idle.transport.calls).toEqual([]);
    expect(idle.sandbox.responded).toBe(false);
  });
});

describe("挑战页 toast 执行（issue #180）", () => {
  it("只处理带本次挑战 fragment 的页面", () => {
    const challenged = runContent(`#kimi-switch-bind=v1.${NONCE}.${PORT}`, FIXTURE_TOKEN);
    expect(challenged.storageReads).toEqual(["access_token"]);
    expect(challenged.messages).toEqual([{ type: "kimi-switch-web-bind", nonce: NONCE, port: PORT, token: FIXTURE_TOKEN }]);
    expect(challenged.location.hash).toBe("");
    expect(challenged.replaced).toBe("/code/console");
    expect(challenged.nodes).toEqual([]);

    for (const hash of ["", "#/library", `#kimi-switch-bind=v1.zz.${PORT}`, `#kimi-switch-bind=v1.${NONCE}.80`]) {
      const idle = runContent(hash, FIXTURE_TOKEN);
      expect(idle.storageReads).toEqual([]);
      expect(idle.messages).toEqual([]);
      expect(idle.nodes).toEqual([]);
      expect(idle.mounted).toEqual([]);
    }
    const other = runContent("#kimi-switch-bind=v1." + "cd".repeat(24) + ".19090", "other-fixture-token");
    expect(other.messages[0]?.nonce).toBe("cd".repeat(24));
    expect(other.messages[0]?.port).toBe(19090);
    expect(other.messages[0]?.token).not.toBe(FIXTURE_TOKEN);
  });

  it("toast 文案钉死，token 与账号 id 不进 DOM", () => {
    const success = runContent(`#kimi-switch-bind=v1.${NONCE}.${PORT}`, FIXTURE_TOKEN, {
      result: "success",
      reason: "绑定成功",
      token: FIXTURE_TOKEN,
      accountId: FIXTURE_ACCOUNT,
    });
    expect(success.mounted).toHaveLength(1);
    const node = success.mounted[0];
    expect(node.textContent).toBe("绑定成功，可回到 kimi-switch 查看");
    expect(node.attrs.role).toBe("status");
    expect(node.attrs["aria-live"]).toBe("polite");
    expect(node.textContent).not.toContain(FIXTURE_TOKEN);
    expect(node.textContent).not.toContain(FIXTURE_ACCOUNT);
    expect(JSON.stringify(node.attrs)).not.toContain(FIXTURE_TOKEN);
    expect(success.timers).toEqual([{ fn: expect.any(Function), ms: 5000 }]);
    success.timers[0].fn();
    expect(node.removed).toBe(true);

    for (const reason of ["已取消", "等待超时", "无可用网页凭证", "账号身份不一致"]) {
      const failed = runContent(`#kimi-switch-bind=v1.${NONCE}.${PORT}`, null, {
        result: "failure",
        reason,
        token: FIXTURE_TOKEN,
        accountId: FIXTURE_ACCOUNT,
      });
      expect(failed.messages[0]?.token).toBe("");
      expect(failed.mounted[0]?.textContent).toBe(`绑定失败：${reason}`);
      expect(failed.mounted[0]?.textContent).not.toContain(FIXTURE_TOKEN);
      expect(failed.mounted[0]?.textContent).not.toContain(FIXTURE_ACCOUNT);
    }

    const membership = runContent(`#kimi-switch-bind=v1.${NONCE}.${PORT}`, FIXTURE_TOKEN, {
      result: "success",
      reason: "会员信息获取失败，请稍后再试",
    });
    expect(membership.mounted).toEqual([]);
    expect(manifest.content_scripts[0]?.matches).toEqual(["https://www.kimi.com/code/console*"]);
  });
});

describe("扩展缺失后的手动粘贴（issue #176）", () => {
  it("自动读取失败后仍可手动提交并调用 bindWebAccount", async () => {
    const dom = new JSDOM(html, { url: "http://127.0.0.1/", pretendToBeVisual: true });
    const win = dom.window;
    const dialog = win.document.getElementById("bind-dialog") as HTMLDialogElement;
    dialog.showModal = () => {
      dialog.setAttribute("open", "");
    };
    dialog.close = () => {
      dialog.removeAttribute("open");
    };
    const previous = {
      document: globalThis.document,
      window: globalThis.window,
      HTMLElement: globalThis.HTMLElement,
      Element: globalThis.Element,
    };
    globalThis.document = win.document;
    globalThis.window = win as unknown as Window & typeof globalThis;
    globalThis.HTMLElement = win.HTMLElement;
    globalThis.Element = win.Element;
    try {
      const { api } = await import("./api");
      const { bindFromEdge, openBindDialog, submitBind } = await import("./main");
      const accountId = "fixture-parked";
      openBindDialog(accountId);
      vi.mocked(api.beginWebBind).mockRejectedValueOnce(new Error("找不到 Microsoft Edge"));
      await expect(bindFromEdge(accountId)).rejects.toThrow(/Edge/);
      expect(vi.mocked(api.completeWebBind)).not.toHaveBeenCalled();
      expect(vi.mocked(api.bindWebAccount)).not.toHaveBeenCalled();

      const status = win.document.getElementById("bind-edge-status");
      const token = win.document.getElementById("bind-token") as HTMLTextAreaElement;
      const submit = win.document.getElementById("bind-submit") as HTMLButtonElement;
      expect(status?.textContent).toBe(
        "自动读取未完成：可点「重试」再试，或在下方手动粘贴网页 access_token。",
      );
      expect(token.disabled).toBe(false);
      expect(submit.disabled).toBe(false);

      token.value = FIXTURE_TOKEN;
      vi.mocked(api.bindWebAccount).mockResolvedValueOnce({
        accounts: [],
        current_label: "",
        account_count: 0,
        urgent_count: 0,
        proactive_renewal: false,
        auto_swap: false,
        auto_swap_prefer_lower_tier: false,
        auto_swap_resident_lower_tier: false,
        auto_swap_pre_reset_burn: false,
        auto_swap_pace_pre_swap: false,
        message: null,
      });
      submitBind();
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(vi.mocked(api.bindWebAccount)).toHaveBeenCalledWith(accountId, FIXTURE_TOKEN);
      expect(dialog.hasAttribute("open")).toBe(false);
      expect(token.disabled).toBe(false);
      expect(submit.disabled).toBe(false);
    } finally {
      globalThis.document = previous.document;
      globalThis.window = previous.window;
      globalThis.HTMLElement = previous.HTMLElement;
      globalThis.Element = previous.Element;
      dom.window.close();
    }
  });
});
