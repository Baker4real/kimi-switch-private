import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { parseWebToken, webBindCopy, WEB_BIND_CONSOLE_URL } from "./webBind";

const dir = dirname(fileURLToPath(import.meta.url));

describe("WEB_BIND_CONSOLE_URL（issue #51）", () => {
  it("固定指向官方控制台页（token 来源页），不是订阅页或第三方域名", () => {
    expect(WEB_BIND_CONSOLE_URL).toBe("https://www.kimi.com/code/console");
  });

  // 运行时调用方（手动兜底「打开控制台」按钮）在下方 GUI seam describe 钉住，
  // 该导出不得退回无调用方的死导出。
});

describe("parseWebToken", () => {
  it("去首尾空白与换行（粘贴常见形态）", () => {
    expect(parseWebToken("  eyJhbGciOiJ9.sig\n")).toBe("eyJhbGciOiJ9.sig");
  });

  it("空白输入返回 null，不发起绑定", () => {
    expect(parseWebToken("")).toBeNull();
    expect(parseWebToken("   \n\t ")).toBeNull();
  });
});

describe("webBindCopy", () => {
  it("含账号显示名、自动读取和手动兜底说明", () => {
    const copy = webBindCopy("工作机");
    expect(copy).toContain("工作机");
    expect(copy).toContain("access_token");
    expect(copy).toContain("Edge 控制台");
    expect(copy).toContain("扩展未安装");
    expect(copy).toContain("下方粘贴");
    expect(copy).toContain("会员到期");
    expect(copy).toContain("这张卡可显示会员到期");
    expect(copy).not.toContain("会员有效期");
    expect(copy).not.toContain("有效期");
  });

  it("保留自动读取优先和手动兜底的顺序", () => {
    const copy = webBindCopy("工作机");
    expect(copy).toBe(
      "绑定「工作机」后这张卡可显示会员到期。优先从 Edge 控制台自动读取；如果 Edge 扩展未安装或读取失败，仍可在下方粘贴网页 access_token。",
    );
  });
});

// Edge 免粘贴绑定 GUI seam（issue #173 / docs/specs/edge-web-binding.md「GUI seam」）：
// 沿仓库静态源码断言惯例，钉住一次点击的 begin→complete 编排、状态行、手动兜底、
// 关闭清理、就近重试与「token 不出 Snapshot 或错误日志」。真实 Edge 交接、未装
// 扩展/未登录/多 profile 等运行态行为属 #176 真机验收，不在本测试假定范围。
const mainSrc = readFileSync(join(dir, "main.ts"), "utf8");
const htmlSrc = readFileSync(join(dir, "index.html"), "utf8").replace(/\r\n/g, "\n");
const apiSrc = readFileSync(join(dir, "api.ts"), "utf8");

/** 取 main.ts 某顶层函数的源文本（从声明起到下一个顶层 function 声明为止）。 */
function functionSrc(name: string): string {
  const start = mainSrc.indexOf(`function ${name}(`);
  expect(start, `缺少函数: ${name}`).toBeGreaterThan(-1);
  const next = mainSrc.indexOf("\nfunction ", start + 1);
  return mainSrc.slice(start, next < 0 ? mainSrc.length : next);
}

/** 从某元素接线起点截取一段监听器源文本。 */
function wiringSrc(selector: string, length = 700): string {
  const start = mainSrc.indexOf(selector);
  expect(start, `缺少接线: ${selector}`).toBeGreaterThan(-1);
  return mainSrc.slice(start, start + length);
}

/** 绑定对话框整块 HTML。 */
function bindDialog(): string {
  const start = htmlSrc.indexOf('id="bind-dialog"');
  expect(start).toBeGreaterThanOrEqual(0);
  const open = htmlSrc.lastIndexOf("<dialog", start);
  const end = htmlSrc.indexOf("</dialog>", start);
  return htmlSrc.slice(open, end + "</dialog>".length);
}

describe("Edge 自动绑定 GUI seam（issue #173）", () => {
  it("对话框一次点击入口、状态行与手动粘贴兜底同框", () => {
    const dialog = bindDialog();
    expect(dialog).toMatch(
      /id="bind-open"[^>]*class="[^"]*action-button action-button-primary/,
    );
    expect(dialog).toContain(">从 Edge 自动读取并绑定</button>");
    // 等待/结果状态行是 polite live region，等待期间可被读屏跟进。
    expect(dialog).toMatch(/id="bind-edge-status"[^>]*aria-live="polite"/);
    // 手动粘贴兜底与错误就地重试面板都在同一对话框内。
    expect(dialog).toContain('id="bind-token"');
    expect(dialog).toContain('id="bind-action-error"');
    expect(dialog).toContain('id="bind-retry-button"');
  });

  it("手动兜底区自解释：步骤指引 + 打开控制台次按钮，位于自动区与粘贴框之间", () => {
    const dialog = bindDialog();
    // 无扩展用户从这里得知 access_token 从哪里取（#164 链回归修复）。
    const stepsIdx = dialog.indexOf('id="bind-manual-steps"');
    expect(stepsIdx).toBeGreaterThanOrEqual(0);
    const steps = dialog.slice(stepsIdx, dialog.indexOf("</p>", stepsIdx));
    expect(steps).toContain("www.kimi.com/code/console");
    expect(steps).toContain("登录同一账号");
    expect(steps).toContain("F12");
    expect(steps).toContain("Application");
    expect(steps).toContain("Local Storage");
    expect(steps).toContain("access_token");
    // 顺序：自动状态行 → 手动步骤 → 打开控制台 → 粘贴输入框（Edge 自动优先不回退）。
    expect(dialog.indexOf('id="bind-edge-status"')).toBeLessThan(stepsIdx);
    const consoleIdx = dialog.indexOf('id="bind-open-console"');
    expect(stepsIdx).toBeLessThan(consoleIdx);
    expect(consoleIdx).toBeLessThan(dialog.indexOf('id="bind-token"'));
    expect(dialog).toContain(">打开控制台</button>");
  });

  it("手动兜底可自解释：次按钮走既有 openUrl，URL 常量保持有运行时调用方", () => {
    const wiring = wiringSrc('required<HTMLButtonElement>("bind-open-console")');
    expect(wiring).toContain("api.openUrl(WEB_BIND_CONSOLE_URL)");
    // 打不开浏览器时降级为可手抄的提示，不静默失败。
    expect(wiring).toMatch(/showToast\("无法打开浏览器/);
    // 次按钮只是导航：不发起绑定、不触碰挑战与 nonce。
    expect(wiring).not.toContain("beginWebBind");
    expect(wiring).not.toContain("pendingWebBindNonce");
    // 主按钮语义不变：自动读取仍是唯一主操作（次按钮不带 primary 修饰）。
    const dialog = bindDialog();
    expect(dialog).toMatch(/id="bind-open-console"[^>]*class="action-button"/);
    expect(dialog).not.toMatch(/id="bind-open-console"[^>]*action-button-primary/);
  });

  it("一次点击先 begin 后 complete：忙碌早退，失败留在对话框就近重试", () => {
    const entry = functionSrc("runBindFromEdge");
    // 忙碌语义（裁定）：点击时 store busy 直接 return，不重复发起。
    expect(entry).toMatch(/if \(store\.get\(\)\.busy\) \{\s*return;\s*\}/);
    expect(entry).toContain('action: () => bindFromEdge(id)');
    expect(entry).toContain('label: "正在从 Edge 读取并绑定…"');
    expect(entry).toContain('dialogId: "bind-dialog"');
    const flow = functionSrc("bindFromEdge");
    // 先建挑战（Rust 随即打开 Edge），再等待扩展回传；顺序不得倒置。
    expect(flow.indexOf("api.beginWebBind(id)")).toBeLessThan(
      flow.indexOf("api.completeWebBind(id, challenge.nonce)"),
    );
  });

  it("成功沿用既有 Snapshot 与会员展示：自动路径返回 Snapshot，走统一对话框快照管道", () => {
    expect(mainSrc).toMatch(
      /async function bindFromEdge\(id: string\): Promise<Snapshot> \{/,
    );
    // 成功仍经 runDialogSnapshotAction：applySnapshot 重涂会员展示、关对话框。
    expect(functionSrc("runBindFromEdge")).toContain("runDialogSnapshotAction({");
  });

  it("状态行覆盖等待中、已打开与失败引导（未装扩展/未登录真机形态为超时）", () => {
    const flow = functionSrc("bindFromEdge");
    expect(flow).toContain('"正在打开 Edge 控制台，等待网页凭证…"');
    expect(flow).toContain('"已打开 Edge；登录目标网页账号后会自动完成绑定。"');
    // 失败改写状态行并引导转手动粘贴（或重试自动路径）。
    expect(flow).toMatch(/catch \(error\) \{\s*status\.textContent = EDGE_BIND_FAILED_STATUS;/);
    const guide = mainSrc.slice(
      mainSrc.indexOf("EDGE_BIND_FAILED_STATUS ="),
      mainSrc.indexOf(";", mainSrc.indexOf("EDGE_BIND_FAILED_STATUS =")),
    );
    expect(guide).toContain("自动读取未完成");
    expect(guide).toContain("重试");
    expect(guide).toContain("粘贴");
    expect(guide).toContain("access_token");
  });

  it("就近重试复用失败时的同一路径：自动失败重试自动，手动失败重试手动", () => {
    const retry = wiringSrc('required<HTMLButtonElement>("bind-retry-button")');
    expect(retry).toContain("if (lastBindAction === \"edge\") {");
    expect(retry).toContain("runBindFromEdge();");
    expect(retry).toContain("submitBind();");
    expect(functionSrc("submitBind")).toContain('lastBindAction = "manual";');
    // 关闭对话框即复位，重开后的重试不沿用上一次的路径记忆。
    const close = wiringSrc(
      'required<HTMLDialogElement>("bind-dialog").addEventListener("close"',
    );
    expect(close).toContain("lastBindAction = null;");
    // 全文件对 lastBindAction 只有三个赋值点：edge / manual / null。
    expect(mainSrc.match(/lastBindAction = /g)).toHaveLength(3);
  });

  it("手动粘贴路径不受自动读取影响：仍走既有 bind_web_account 管道", () => {
    const submit = functionSrc("submitBind");
    expect(submit).toContain("evaluateBindSubmit(input.value, input, errorEl)");
    expect(submit).toContain("api.bindWebAccount(id, token)");
    // 自动路径不读写手动粘贴输入框，两条路径互不纠缠。
    expect(functionSrc("bindFromEdge")).not.toContain("bind-token");
  });

  it("关闭/取消对话框（取消按钮 / Esc / close 事件）取消挑战并清输入", () => {
    const cancel = wiringSrc('required<HTMLButtonElement>("bind-cancel")');
    expect(cancel).toContain("cancelPendingWebBind().finally(");
    expect(cancel).toContain('required<HTMLDialogElement>("bind-dialog").close();');
    const close = wiringSrc(
      'required<HTMLDialogElement>("bind-dialog").addEventListener("close"',
      900,
    );
    expect(close).toContain("void cancelPendingWebBind();");
    expect(close).toMatch(/input\.value = "";/);
    expect(close).toMatch(/"bind-edge-status"\)\.textContent = ""/);
    const pending = functionSrc("cancelPendingWebBind");
    expect(pending).toContain("pendingWebBindNonce = null;");
    expect(pending).toContain("api.cancelWebBind(nonce)");
    // Esc 走原生 cancel→close：不为 bind-dialog 拦截 cancel 事件，清理只有 close 一处。
    expect(mainSrc).not.toMatch(/bind-dialog"\)\.addEventListener\("cancel"/);
  });

  it("token/JWT 不返回 WebView：api 层挑战只含元数据，complete 返回 Snapshot", () => {
    // 挑战 DTO 只有 nonce/url；nonce 只是 Rust 侧一次性会话句柄。
    const ifaceStart = apiSrc.indexOf("export interface WebBindChallenge");
    expect(ifaceStart).toBeGreaterThanOrEqual(0);
    const iface = apiSrc.slice(ifaceStart, apiSrc.indexOf("}", ifaceStart));
    expect(iface).toContain("nonce: string;");
    expect(iface).toContain("url: string;");
    expect(iface).not.toContain("token");
    // 等待回调的返回类型是 Snapshot（会员展示照旧），不是 token 或网页身份字段。
    expect(apiSrc).toContain('call<Snapshot>("complete_web_bind"');
    expect(apiSrc).toContain('call<WebBindChallenge>("begin_web_bind"');
    expect(apiSrc).toContain('call<void>("cancel_web_bind"');
    const verbs = apiSrc.slice(
      apiSrc.indexOf("beginWebBind:"),
      apiSrc.indexOf("openUrl:"),
    );
    expect(verbs).not.toContain("token");
  });

  it("token 不进 DOM 与日志：前端不持有网页凭证，不展示挑战 URL", () => {
    // 挑战 URL（含 fragment 元数据）由 Rust 直接交给 Edge，前端不展示、不引用。
    expect(mainSrc).not.toContain("challenge.url");
    expect(mainSrc).not.toContain(".url]");
    // 网页凭证不写任何文本节点或标记结构；错误文本只经 commandError 的中文 message。
    expect(mainSrc).not.toMatch(/(?:textContent|innerHTML)\s*=\s*[^;\n]*\btoken\b/i);
    expect(mainSrc).not.toMatch(/console\./);
    expect(mainSrc).not.toMatch(/showToast\(String\(/);
    // 前端唯一持有的挑战状态是一次性 nonce，只用于比较与取消，不进任何文本节点。
    expect(functionSrc("bindFromEdge")).not.toMatch(
      /textContent\s*=\s*[^;\n]*pendingWebBindNonce/,
    );
  });
});
