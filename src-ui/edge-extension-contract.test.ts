import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

// Edge 扩展静态契约（issue #172 / docs/specs/edge-web-binding.md「扩展静态契约」）：
// 不加载扩展运行时，只对源码内容做正/负断言，钉住权限面、读取门槛、
// fragment 清除时机与 service worker 的固定 loopback 目标。
// 真实 Edge 交接、Local Network Access 等运行态行为属 #176 验收，不在本测试假定范围。
const dir = dirname(fileURLToPath(import.meta.url));
const extensionDir = join(dir, "..", "edge-extension");
const manifestText = readFileSync(join(extensionDir, "manifest.json"), "utf8");
const contentSrc = readFileSync(join(extensionDir, "content.js"), "utf8").replace(/\r\n/g, "\n");
const workerSrc = readFileSync(join(extensionDir, "service-worker.js"), "utf8").replace(/\r\n/g, "\n");
// Rust 回调协议已随 #171 冻结；读取契约真源钉住「扩展 ↔ broker」字面量不漂移。
const brokerSrc = readFileSync(join(dir, "..", "src-tauri", "src", "web_bind.rs"), "utf8");

interface ManifestLike {
  manifest_version?: number;
  permissions?: string[];
  host_permissions?: string[];
  content_scripts?: {
    matches?: string[];
    js?: string[];
    run_at?: string;
    all_frames?: boolean;
  }[];
  background?: { service_worker?: string };
  web_accessible_resources?: unknown;
  externally_connectable?: unknown;
}

const manifest = JSON.parse(manifestText) as ManifestLike;

describe("扩展 manifest 权限面（issue #172）", () => {
  it("Manifest V3 且不申请任何扩展 API 权限", () => {
    expect(manifest.manifest_version).toBe(3);
    expect(manifest.permissions ?? []).toEqual([]);
  });

  it("host permission 只声明 127.0.0.1 loopback，不含任何远端域名", () => {
    const hosts = manifest.host_permissions?.join(" ") ?? "";
    expect(manifest.host_permissions).toEqual(["http://127.0.0.1/*"]);
    expect(hosts).not.toContain("https://");
    expect(hosts).not.toContain("localhost");
  });

  it("content script 只注入 Kimi 控制台路径，不进 iframe", () => {
    expect(manifest.content_scripts).toHaveLength(1);
    const script = manifest.content_scripts?.[0];
    expect(script?.matches).toEqual(["https://www.kimi.com/code/console*"]);
    expect(script?.js).toEqual(["content.js"]);
    expect(script?.all_frames).not.toBe(true);
  });

  it("后台只有 service worker，不暴露可被网页访问的资源或消息入口", () => {
    expect(manifest.background?.service_worker).toBe("service-worker.js");
    expect(manifest.web_accessible_resources).toBeUndefined();
    expect(manifest.externally_connectable).toBeUndefined();
  });
});

describe("content script 读取门槛（issue #172）", () => {
  it("没有挑战 fragment 时提前返回，storage 访问都发生在挑战校验之后", () => {
    expect(contentSrc).toContain('const prefix = "#kimi-switch-bind=v1.";');
    // 守卫块必须提前 return：普通控制台页面不执行任何后续逻辑。
    expect(contentSrc).toMatch(/if \(!location\.hash\.startsWith\(prefix\)\) \{\s*return;\s*\}/);
    const guardIdx = contentSrc.indexOf("location.hash.startsWith(prefix)");
    const nonceCheckIdx = contentSrc.indexOf("/^[0-9a-f]{48}$/");
    const storageIdx = contentSrc.indexOf("localStorage.getItem");
    expect(guardIdx).toBeGreaterThanOrEqual(0);
    expect(nonceCheckIdx).toBeGreaterThan(guardIdx);
    expect(storageIdx).toBeGreaterThan(nonceCheckIdx);
    // 全文件只有这一处 storage 访问，且只读 access_token，不写不删。
    expect(contentSrc.match(/localStorage\./g)).toHaveLength(1);
    expect(contentSrc).toContain('localStorage.getItem("access_token")');
    expect(contentSrc).not.toContain("setItem");
    expect(contentSrc).not.toContain("removeItem");
    expect(contentSrc).not.toContain("sessionStorage");
    expect(contentSrc).not.toContain("indexedDB");
    expect(contentSrc).not.toContain("document.cookie");
  });

  it("nonce 为 48 位小写 hex、端口 1024–65535，与 broker 挑战格式一致", () => {
    expect(contentSrc).toContain("/^[0-9a-f]{48}$/");
    expect(contentSrc).toContain("port < 1024");
    expect(contentSrc).toContain("port > 65535");
    expect(contentSrc).toContain("Number.isInteger(port)");
  });

  it("读取 token 后立即清除 fragment，且不产生新历史或写回 URL", () => {
    const readIdx = contentSrc.indexOf('localStorage.getItem("access_token")');
    const clearIdx = contentSrc.indexOf("history.replaceState");
    expect(clearIdx).toBeGreaterThan(readIdx);
    expect(contentSrc).toContain(
      'history.replaceState(null, "", `${location.pathname}${location.search}`)',
    );
    expect(contentSrc).not.toContain("pushState");
    expect(contentSrc).not.toMatch(/location\.(hash|href|assign|replace)\s*=/);
  });

  it("未登录/无 token 时仍发送空 token，由 Rust 侧拒绝并给出中文失败", () => {
    expect(contentSrc).toContain('localStorage.getItem("access_token") || ""');
    expect(contentSrc).toMatch(
      /chrome\.runtime\.sendMessage\(\{\s*type:\s*"kimi-switch-web-bind",\s*nonce,\s*port,\s*token,?\s*\}, \(response\) =>/,
    );
  });

  it("token 不进 console/DOM，页面脚本不直接发起任何网络请求", () => {
    expect(contentSrc).not.toMatch(/console\./);
    expect(contentSrc).not.toMatch(/innerHTML|document\.write/);
    expect(contentSrc).not.toContain("fetch(");
    expect(contentSrc).not.toContain("XMLHttpRequest");
    expect(contentSrc).not.toContain("sendBeacon");
    expect(contentSrc).not.toContain("chrome.storage");
  });
});

function sourceOf(src: string, name: string): string {
  const marker = `async function ${name}`;
  const start = src.indexOf(marker);
  expect(start, name).toBeGreaterThanOrEqual(0);
  const rest = src.slice(start + marker.length);
  const next = rest.search(/\nasync function |\nfunction /);
  expect(next, name).toBeGreaterThan(0);
  return rest.slice(0, next);
}

describe("service worker 固定 loopback 目标（issue #172）", () => {
  it("只接受固定消息类型，校验 nonce/端口/token 字段通过后才发送", () => {
    expect(workerSrc).toContain('message.type !== "kimi-switch-web-bind"');
    expect(workerSrc).toContain("/^[0-9a-f]{48}$/");
    expect(workerSrc).toContain("port < 1024");
    expect(workerSrc).toContain("port > 65535");
    expect(workerSrc).toContain('typeof token !== "string"');
    const validateIdx = workerSrc.indexOf('typeof token !== "string"');
    const fetchIdx = workerSrc.indexOf("fetch(");
    expect(validateIdx).toBeGreaterThanOrEqual(0);
    expect(fetchIdx).toBeGreaterThan(validateIdx);
  });

  it("204 后长轮询；POST 失败只短探测终止态，不合成等待超时", () => {
    expect(workerSrc.match(/fetch\(/g)).toHaveLength(2);
    expect(workerSrc).toContain("`http://127.0.0.1:${port}/kimi-switch/web-bind`");
    expect(workerSrc).toContain('method: "POST"');
    expect(workerSrc).toContain('method: "GET"');
    // 要读到 204 与 JSON 正文，不能再用 no-cors 把响应变成不透明。
    expect(workerSrc).not.toContain("no-cors");
    expect(workerSrc).not.toContain("https://");
    expect(workerSrc).not.toContain("localhost");
    expect(workerSrc).not.toMatch(/new URL\(/);
    const postTemplate = "`http://127.0.0.1:${port}/kimi-switch/web-bind`";
    const postIdx = workerSrc.indexOf(postTemplate);
    expect(postIdx).toBeGreaterThanOrEqual(0);
    expect(
      workerSrc.slice(postIdx, postIdx + postTemplate.length).match(/\$\{[^}]+\}/g),
    ).toEqual(["${port}"]);
    const statusTemplate =
      "`http://127.0.0.1:${port}/kimi-switch/web-bind-status?nonce=${nonce}`";
    expect(workerSrc.split(statusTemplate).length - 1).toBe(1);
    expect(statusTemplate).not.toContain("token");
    expect(statusTemplate.match(/\$\{[^}]+\}/g)).toEqual(["${port}", "${nonce}"]);

    const postBody = sourceOf(workerSrc, "postAcceptedThenPoll");
    const pollBody = sourceOf(workerSrc, "pollBindStatus");
    const probeBody = sourceOf(workerSrc, "probeTerminalStatus");
    expect(postBody.match(/return probeTerminalStatus\(port, nonce\)/g)).toHaveLength(2);
    expect(postBody).toMatch(/catch \{\s*return probeTerminalStatus\(port, nonce\);\s*\}/);
    expect(postBody).toMatch(
      /status !== 204\) \{\s*return probeTerminalStatus\(port, nonce\);\s*\}/,
    );
    expect(postBody.match(/return pollBindStatus\(port, nonce\)/g)).toHaveLength(1);
    expect(postBody.indexOf("return pollBindStatus(port, nonce)")).toBeGreaterThan(
      postBody.indexOf("status !== 204"),
    );

    expect(pollBody).toContain("readBindStatus(port, nonce)");
    expect(pollBody).toContain("attempt < STATUS_POLL_MAX_ATTEMPTS");
    expect(pollBody).toContain('{ result: "failure", reason: "等待超时" }');
    expect(pollBody).not.toContain("probeTerminalStatus");

    expect(probeBody).toContain("readBindStatus(port, nonce)");
    expect(probeBody).toContain("attempt < STATUS_PROBE_MAX_ATTEMPTS");
    expect(probeBody).toMatch(/result === "success" \|\| result === "failure"[\s\S]{0,80}return/);
    expect(probeBody.trimEnd().endsWith("return null;\n}")).toBe(true);
    expect(probeBody).not.toContain("pollBindStatus");
    expect(probeBody).not.toContain("等待超时");
    expect(probeBody).not.toContain("STATUS_POLL_");
    const probeInterval = Number(
      workerSrc.match(/const STATUS_PROBE_INTERVAL_MS = (\d+);/)?.[1],
    );
    const probeAttempts = Number(
      workerSrc.match(/const STATUS_PROBE_MAX_ATTEMPTS = (\d+);/)?.[1],
    );
    expect(probeAttempts).toBeGreaterThan(1);
    expect(probeAttempts).toBeLessThanOrEqual(4);
    expect((probeAttempts - 1) * probeInterval).toBeGreaterThan(0);
    expect((probeAttempts - 1) * probeInterval).toBeLessThan(3_000);
  });

  it("token 只进入 POST JSON；回查有限次，pending 会重试，终止态立即停止", () => {
    expect(workerSrc).toContain("body: JSON.stringify({ nonce, token })");
    expect(workerSrc).not.toContain("chrome.storage");
    expect(workerSrc).not.toMatch(/console\./);
    expect(workerSrc).not.toContain("setInterval");
    expect(workerSrc).not.toMatch(/while\s*\(\s*true\s*\)/);
    expect(workerSrc).not.toContain("indexedDB");
    expect(workerSrc).not.toContain("document.cookie");
    expect(workerSrc).toMatch(/\.catch\(\(\) => undefined\)/);
    expect(workerSrc).toContain("attempt < STATUS_POLL_MAX_ATTEMPTS");
    expect(workerSrc).toContain("if (attempt > 0)");
    expect(workerSrc).toMatch(/result === "pending"[\s\S]{0,80}continue/);
    expect(workerSrc).toMatch(
      /result === "success" \|\| result === "failure"[\s\S]{0,160}return/,
    );
    expect(workerSrc).toContain('{ result: "failure", reason: "等待超时" }');
    expect(workerSrc).not.toContain("会员信息获取失败");
    const interval = Number(workerSrc.match(/const STATUS_POLL_INTERVAL_MS = (\d+);/)?.[1]);
    const attempts = Number(workerSrc.match(/const STATUS_POLL_MAX_ATTEMPTS = (\d+);/)?.[1]);
    expect(attempts).toBeGreaterThan(1);
    expect(attempts).toBeLessThanOrEqual(40);
    // 第一次立即查，之后每次间隔 interval。总等待要盖住收下后最多 20 秒的「等待中」，
    // 并且不能只撑过终止态那 10 秒保留窗。
    expect((attempts - 1) * interval).toBeGreaterThanOrEqual(20_000);
    expect((attempts - 1) * interval).toBeLessThan(40_000);
  });
});

describe("与 Rust broker 冻结协议对齐（#171 契约真源）", () => {
  it("fragment 前缀与回调路径逐字一致", () => {
    expect(brokerSrc).toContain("#kimi-switch-bind=v1.");
    expect(brokerSrc).toContain('const CALLBACK_PATH: &str = "/kimi-switch/web-bind";');
    expect(brokerSrc).toContain('const STATUS_PATH: &str = "/kimi-switch/web-bind-status";');
    expect(contentSrc).toContain('"#kimi-switch-bind=v1."');
    expect(workerSrc).toContain("/kimi-switch/web-bind");
    expect(workerSrc).toContain("/kimi-switch/web-bind-status?nonce=");
  });
});

describe("挑战页结果 toast（issue #180）", () => {
  it("toast 只注入通过挑战校验的页面，无挑战分支零 DOM", () => {
    const guardIdx = contentSrc.indexOf("if (!location.hash.startsWith(prefix))");
    const invalidIdx = contentSrc.indexOf("/^[0-9a-f]{48}$/");
    const createIdx = contentSrc.indexOf("document.createElement");
    const liveIdx = contentSrc.indexOf('setAttribute("aria-live", "polite")');
    const sendIdx = contentSrc.indexOf("chrome.runtime.sendMessage");
    expect(guardIdx).toBeGreaterThanOrEqual(0);
    expect(invalidIdx).toBeGreaterThan(guardIdx);
    expect(createIdx).toBeGreaterThan(invalidIdx);
    expect(liveIdx).toBeGreaterThan(invalidIdx);
    expect(sendIdx).toBeGreaterThan(invalidIdx);
    expect(contentSrc).toContain('setAttribute("role", "status")');
    expect(contentSrc).toMatch(/setTimeout\(\(\) => node\.remove\(\), 5000\)/);
    expect(contentSrc).not.toContain("fetch(");
  });

  it("成功与失败文案钉死，不把 reason「绑定成功」当成整句 toast", () => {
    expect(contentSrc).toContain('return "绑定成功，可回到 kimi-switch 查看"');
    expect(contentSrc).not.toContain('textContent = "绑定成功"');
    expect(contentSrc).not.toContain("textContent = reason");
    expect(contentSrc).toContain('return "绑定失败：" + reason');
    for (const reason of ["已取消", "等待超时", "无可用网页凭证", "账号身份不一致"]) {
      expect(contentSrc).toContain(`"${reason}"`);
    }
    expect(contentSrc).not.toContain("会员信息获取失败");
  });

  it("toast 文本不接收 token、账号 id 或 JWT", () => {
    expect(contentSrc).not.toMatch(/textContent\s*=\s*[^;\n]*\btoken\b/);
    expect(contentSrc).not.toMatch(/innerHTML|document\.write/);
    expect(contentSrc).not.toMatch(/\b(account_id|accountId|user_id|userId)\b/);
    expect(contentSrc).not.toContain("eyJ");
    expect(contentSrc).not.toMatch(/response\.token|response\.account/);
    expect(workerSrc).not.toMatch(/web-bind-status\?[^`]*token/);
    expect(workerSrc).not.toMatch(/sendResponse\(\s*\{[^}]*\btoken\b/);
  });
});
