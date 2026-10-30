import { createServer } from "node:http";
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { extname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { ACCEPT_WINDOWS, ACCEPT_ZOOMS, LONG_DISPLAY_NAME, parkedRow } from "./desktop-accept.fixture";
import { accountCardHtml } from "./library-card";
import { listenOnBrowserSafePort, startOnBrowserSafePort } from "./desktop-test-port";

const uiRoot = fileURLToPath(new URL(".", import.meta.url));

export interface ZoomHit {
  ok: boolean;
  horizontallyIn: boolean;
  w: number;
  h: number;
  right: number;
  via: string;
}

export interface ZoomFrame {
  window: string;
  width: number;
  height: number;
  zoom: number;
  appliedZoom: number;
  innerWidth: number;
  innerHeight: number;
  pageOverflow: boolean;
  actions: ZoomHit;
  title: ZoomHit;
  status: ZoomHit;
  swap: ZoomHit;
  submit: ZoomHit;
  open: ZoomHit;
}

const MEASURE = `(() => {
  const page = document.querySelector("#page-library");
  const hit = (el) => {
    el.scrollIntoView({ block: "center", inline: "nearest" });
    const r = el.getBoundingClientRect();
    const horizontallyIn = r.width >= 8 && r.left >= -1 && r.right <= innerWidth + 1;
    const points = [
      [r.left + r.width / 2, r.top + r.height / 2],
      [r.left + Math.min(6, r.width / 2), r.top + Math.min(6, r.height / 2)],
      [r.right - Math.min(6, r.width / 2), r.bottom - Math.min(6, r.height / 2)],
    ];
    let via = "";
    let ok = false;
    for (const [x, y] of points) {
      if (x < 1 || y < 1 || x >= innerWidth - 1 || y >= innerHeight - 1) continue;
      const node = document.elementFromPoint(x, y);
      via = node ? String(node.id || node.className || node.tagName) : "";
      if (node && (node === el || el.contains(node))) {
        ok = true;
        break;
      }
    }
    return {
      ok: ok && r.height >= 8,
      horizontallyIn,
      w: Math.round(r.width),
      h: Math.round(r.height),
      right: Math.round(r.right),
      via,
    };
  };
  const actions = hit(document.querySelector(".profile-card-actions"));
  const title = hit(document.querySelector(".profile-title"));
  const status = hit(document.querySelector("#long-status"));
  const swap = hit(document.querySelector("[data-swap]"));
  const dialog = document.querySelector("#bind-dialog");
  dialog.showModal();
  const submit = hit(document.querySelector("#bind-submit"));
  const open = hit(document.querySelector("#bind-open"));
  dialog.close();
  return {
    zoom: Number(getComputedStyle(document.documentElement).zoom) || 1,
    innerWidth,
    innerHeight,
    pageOverflow: page.scrollWidth > page.clientWidth + 1
      || document.documentElement.scrollWidth > document.documentElement.clientWidth + 1,
    actions, title, status, swap, submit, open,
  };
})()`;

const CHROME_CANDIDATES: Record<string, string[]> = {
  win32: [
    "C:/Program Files/Google/Chrome/Application/chrome.exe",
    "C:/Program Files (x86)/Google/Chrome/Application/chrome.exe",
  ],
  linux: ["/usr/bin/google-chrome", "/usr/bin/google-chrome-stable", "/usr/bin/chromium", "/usr/bin/chromium-browser"],
};

/** Chrome 可执行文件：`CHROME_PATH` 优先（指向不存在的文件直接报错，不回退），否则按平台找常见安装位置。 */
function chromePath(): string {
  const override = process.env.CHROME_PATH;
  if (override) {
    if (!existsSync(override)) {
      throw new Error(`CHROME_PATH 指向的 Chrome 不存在：${override}`);
    }
    return override;
  }
  const candidates = CHROME_CANDIDATES[process.platform] ?? [];
  const found = candidates.find((path) => existsSync(path));
  if (!found) {
    throw new Error(
      `未找到 Chrome，无法在真实布局里测量缩放（试过：${candidates.join("、") || "无"}；可用 CHROME_PATH 指定）`,
    );
  }
  return found;
}

function pageHtml(): string {
  let html = readFileSync(join(uiRoot, "index.html"), "utf8").replace(/\r\n/g, "\n");
  const card = accountCardHtml(parkedRow(), false).replace(
    "</article>",
    `<div class="account-status-banner account-status-banner--full"><p class="account-status-copy"><strong class="account-status-title">额度查询失败</strong><span class="account-status-detail" id="long-status">这是一条用于缩放验收的超长状态说明，应当留在卡片里并可被点到，而不是被横向裁出窗口。</span></p></div></article>`,
  );
  html = html.replace("<head>", `<head><link rel="stylesheet" href="/styles.css">`);
  html = html.replace(/<script type="module"[\s\S]*?<\/script>\s*/, "");
  html = html.replace('class="page active" id="page-overview"', 'class="page" id="page-overview"');
  html = html.replace('class="page" id="page-library"', 'class="page active" id="page-library"');
  html = html.replace(
    '<div id="accounts-list" class="profiles-grid"></div>',
    `<div id="accounts-list" class="profiles-grid">${card}</div>`,
  );
  html = html.replace(
    "</body>",
    `<script>document.documentElement.style.zoom = new URLSearchParams(location.search).get("zoom") || "1";</script></body>`,
  );
  if (!html.includes(LONG_DISPLAY_NAME) || !html.includes('id="long-status"')) {
    throw new Error("缩放测量页没有装上长文或状态条");
  }
  return html;
}

async function killChrome(child: ChildProcess): Promise<void> {
  if (!child.pid || child.exitCode !== null || child.signalCode !== null) return;
  if (process.platform === "win32") {
    await new Promise<void>((resolve, reject) => {
      const killer = spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore", windowsHide: true });
      killer.once("error", reject);
      killer.once("exit", (code) => {
        if (code === 0 || child.exitCode !== null || child.signalCode !== null) resolve();
        else reject(new Error("无法结束本次无头 Chrome"));
      });
    });
  } else {
    child.kill();
  }
  if (child.exitCode !== null || child.signalCode !== null) return;
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("本次无头 Chrome 退出超时")), 5000);
    child.once("exit", () => { clearTimeout(timer); resolve(); });
  });
}

async function launchChrome(pagePort: number): Promise<{ child: ChildProcess; debugPort: number }> {
  const executable = chromePath();
  return startOnBrowserSafePort(async (debugPort) => {
    const child = spawn(executable, [
      "--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--disable-extensions",
      `--user-data-dir=${join(tmpdir(), `ks-zoom-measure-${pagePort}-${debugPort}`)}`,
      `--remote-debugging-port=${debugPort}`, "about:blank",
    ], { stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
    try {
      await new Promise<void>((resolve, reject) => {
        let debugLog = "";
        const cleanup = () => {
          clearTimeout(timer);
          child.stdout?.off("data", onData);
          child.stderr?.off("data", onData);
          child.off("error", onError);
          child.off("exit", onExit);
        };
        const fail = (error: Error) => { cleanup(); reject(error); };
        const onError = (error: Error) => fail(error);
        const onExit = () => fail(new Error("Chrome 在调试端口就绪前退出"));
        const onData = (chunk: Buffer) => {
          debugLog = (debugLog + chunk.toString()).slice(-8000);
          const match = debugLog.match(/DevTools listening on ws:\/\/127\.0\.0\.1:(\d+)\/devtools\/browser\//);
          if (match) {
            if (Number(match[1]) !== debugPort) { fail(new Error("Chrome 调试端口与预留端口不一致")); return; }
            cleanup();
            resolve();
          } else if (/bind\(\) failed|Cannot start http server for devtools/i.test(debugLog)) {
            fail(Object.assign(new Error("Chrome 调试端口被占用"), { code: "EADDRINUSE" }));
          }
        };
        const timer = setTimeout(() => fail(new Error("Chrome 没有打开预留的调试端口")), 15000);
        child.stdout?.on("data", onData);
        child.stderr?.on("data", onData);
        child.once("error", onError);
        child.once("exit", onExit);
      });
      return { child, debugPort };
    } catch (error) {
      await killChrome(child);
      throw error;
    }
  });
}

/** 用无头 Chrome 的真实布局测量窗口与页面 zoom。deviceScaleFactor 固定为 1，不改系统 DPI。 */
export async function measureDesktopZoom(): Promise<ZoomFrame[]> {
  const html = pageHtml();
  const types: Record<string, string> = {
    ".css": "text/css; charset=utf-8",
    ".ttf": "font/ttf",
  };
  const server = createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://127.0.0.1");
    if (url.pathname === "/" || url.pathname === "/index.html") {
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end(html);
      return;
    }
    try {
      const body = readFileSync(join(uiRoot, decodeURIComponent(url.pathname).replace(/^\//, "")));
      res.writeHead(200, { "content-type": types[extname(url.pathname)] ?? "application/octet-stream" });
      res.end(body);
    } catch {
      res.writeHead(404);
      res.end("missing");
    }
  });
  const port = await listenOnBrowserSafePort(server);
  const frames: ZoomFrame[] = [];
  let child: ChildProcess | null = null;
  let pageWs: WebSocket | null = null;
  try {
    const launched = await launchChrome(port);
    child = launched.child;
    const debugPort = launched.debugPort;
    const created = (await (
      await fetch(`http://127.0.0.1:${debugPort}/json/new?about:blank`, { method: "PUT" })
    ).json()) as { webSocketDebuggerUrl: string };
    pageWs = new WebSocket(created.webSocketDebuggerUrl);
    await new Promise<void>((resolve) => {
      pageWs?.addEventListener("open", () => resolve());
    });
    const pending = new Map<number, (message: CdpMessage) => void>();
    let seq = 0;
    pageWs.addEventListener("message", (ev) => {
      const message = JSON.parse(String(ev.data)) as CdpMessage & { id?: number };
      if (message.id && pending.has(message.id)) {
        pending.get(message.id)?.(message);
      }
    });
    const send = (method: string, params: Record<string, unknown> = {}) =>
      new Promise<CdpMessage>((resolve) => {
        const id = ++seq;
        pending.set(id, resolve);
        pageWs?.send(JSON.stringify({ id, method, params }));
      });
    await send("Page.enable");
    await send("Runtime.enable");
    for (const win of ACCEPT_WINDOWS) {
      for (const zoom of ACCEPT_ZOOMS) {
        await send("Emulation.setDeviceMetricsOverride", {
          width: win.width,
          height: win.height,
          deviceScaleFactor: 1,
          mobile: false,
        });
        const loaded = new Promise<void>((resolve) => {
          const onMessage = (ev: MessageEvent) => {
            const message = JSON.parse(String(ev.data)) as { method?: string };
            if (message.method === "Page.loadEventFired") {
              pageWs?.removeEventListener("message", onMessage);
              resolve();
            }
          };
          pageWs?.addEventListener("message", onMessage);
        });
        await send("Page.navigate", { url: `http://127.0.0.1:${port}/?zoom=${zoom}` });
        await loaded;
        const evaluated = await send("Runtime.evaluate", { expression: MEASURE, returnByValue: true });
        const failure = evaluated.result?.exceptionDetails?.exception?.description;
        if (failure) {
          throw new Error(failure);
        }
        const value = evaluated.result?.result?.value as Measured;
        if (!value) {
          throw new Error("布局测量没有返回结果");
        }
        frames.push({
          window: win.name,
          width: win.width,
          height: win.height,
          zoom,
          appliedZoom: value.zoom,
          innerWidth: value.innerWidth,
          innerHeight: value.innerHeight,
          pageOverflow: value.pageOverflow,
          actions: value.actions,
          title: value.title,
          status: value.status,
          swap: value.swap,
          submit: value.submit,
          open: value.open,
        });
      }
    }
  } finally {
    pageWs?.close();
    try {
      if (child) await killChrome(child);
    } finally {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  }
  return frames;
}

interface Measured {
  zoom: number;
  innerWidth: number;
  innerHeight: number;
  pageOverflow: boolean;
  actions: ZoomHit;
  title: ZoomHit;
  status: ZoomHit;
  swap: ZoomHit;
  submit: ZoomHit;
  open: ZoomHit;
}

interface CdpMessage {
  result?: {
    result?: { value?: Measured };
    exceptionDetails?: { exception?: { description?: string } };
  };
}
