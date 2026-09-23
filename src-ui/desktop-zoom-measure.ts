import { createServer } from "node:http";
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { extname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { ACCEPT_WINDOWS, ACCEPT_ZOOMS, LONG_DISPLAY_NAME, parkedRow } from "./desktop-accept.fixture";
import { accountCardHtml } from "./library-card";

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

function chromePath(): string {
  const candidates = [
    "C:/Program Files/Google/Chrome/Application/chrome.exe",
    "C:/Program Files (x86)/Google/Chrome/Application/chrome.exe",
  ];
  const found = candidates.find((path) => existsSync(path));
  if (!found) {
    throw new Error("未找到 Chrome，无法在真实布局里测量缩放");
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

function killChrome(child: ChildProcess): void {
  if (process.platform === "win32" && child.pid) {
    spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
    return;
  }
  child.kill();
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
  await new Promise<void>((resolve) => {
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const port = (server.address() as { port: number }).port;
  const child = spawn(
    chromePath(),
    [
      "--headless=new",
      "--disable-gpu",
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-extensions",
      `--user-data-dir=${join(tmpdir(), `ks-zoom-measure-${port}`)}`,
      "--remote-debugging-port=0",
      "about:blank",
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let debugLog = "";
  const debugPort = await new Promise<number>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`Chrome 没有打开调试端口：${debugLog}`)), 15000);
    const onData = (chunk: Buffer) => {
      debugLog += chunk.toString();
      const match = debugLog.match(/127\.0\.0\.1:(\d+)\//);
      if (match) {
        clearTimeout(timer);
        resolve(Number(match[1]));
      }
    };
    child.stderr?.on("data", onData);
    child.stdout?.on("data", onData);
    child.on("exit", () => {
      clearTimeout(timer);
      reject(new Error(`Chrome 提前退出：${debugLog}`));
    });
  });
  const frames: ZoomFrame[] = [];
  let pageWs: WebSocket | null = null;
  try {
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
    killChrome(child);
    server.close();
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
