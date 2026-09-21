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
      /chrome\.runtime\.sendMessage\(\{\s*type:\s*"kimi-switch-web-bind",\s*nonce,\s*port,\s*token,?\s*\}\)/,
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

  it("全文件只有一处 fetch，只 POST 到固定模板 URL，不接受任意 URL", () => {
    expect(workerSrc.match(/fetch\(/g)).toHaveLength(1);
    expect(workerSrc).toContain("`http://127.0.0.1:${port}/kimi-switch/web-bind`");
    expect(workerSrc).toContain('method: "POST"');
    expect(workerSrc).not.toContain("https://");
    expect(workerSrc).not.toContain("localhost");
    expect(workerSrc).not.toMatch(/new URL\(/);
    // URL 模板里唯一的插值是校验过的 port，不拼接消息里的其他字段。
    const urlTemplate = "`http://127.0.0.1:${port}/kimi-switch/web-bind`";
    const urlIdx = workerSrc.indexOf(urlTemplate);
    expect(urlIdx).toBeGreaterThanOrEqual(0);
    const interpolations = workerSrc
      .slice(urlIdx, urlIdx + urlTemplate.length)
      .match(/\$\{[^}]+\}/g);
    expect(interpolations).toHaveLength(1);
    expect(interpolations?.[0]).toBe("${port}");
  });

  it("token 只进入 JSON 正文；不写扩展 storage、不进 console、不轮询不重试", () => {
    expect(workerSrc).toContain("body: JSON.stringify({ nonce, token })");
    expect(workerSrc).not.toContain("chrome.storage");
    expect(workerSrc).not.toMatch(/console\./);
    expect(workerSrc).not.toMatch(/setInterval|setTimeout/);
    expect(workerSrc).not.toContain("indexedDB");
    expect(workerSrc).not.toContain("document.cookie");
    expect(workerSrc).toMatch(/\.catch\(\(\) => undefined\)/);
  });
});

describe("与 Rust broker 冻结协议对齐（#171 契约真源）", () => {
  it("fragment 前缀与回调路径逐字一致", () => {
    expect(brokerSrc).toContain("#kimi-switch-bind=v1.");
    expect(brokerSrc).toContain("/kimi-switch/web-bind");
    expect(contentSrc).toContain('"#kimi-switch-bind=v1."');
    expect(workerSrc).toContain("/kimi-switch/web-bind");
  });
});
