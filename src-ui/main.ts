import { invoke } from "@tauri-apps/api/core";

import "./styles.css";
import { setupWindowControls } from "./window-controls";

type Tone = "ok" | "warn" | "high" | "full";
type MessageTone = "ok" | "err" | "info";
type ThemeId = "classic" | "jade-night";
type Route = "overview" | "library" | "settings";

interface QuotaView {
  window: string;
  ratio: number | null;
  text: string;
  reset: string | null;
  tone: Tone;
}

interface AccountRow {
  id: string;
  label: string;
  active: boolean;
  membership: string | null;
  quotas: QuotaView[];
  error: string | null;
}

interface UiMessage {
  text: string;
  tone: MessageTone;
}

interface Snapshot {
  accounts: AccountRow[];
  current_label: string;
  account_count: number;
  urgent_count: number;
  theme: ThemeId;
  message: UiMessage | null;
}

interface AuthLink {
  url: string;
  user_code: string;
}

const routes: Route[] = ["overview", "library", "settings"];
let snapshot: Snapshot | null = null;
let busy = false;
let renameId = "";
let deleteId = "";
// 最近一次打开授权对话框的流程类型：仅用于取消时的 toast 文案区分
let authIsReauth = false;
let toastTimer = 0;

function required<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!(el instanceof HTMLElement)) {
    throw new Error(`缺少元素: ${id}`);
  }
  return el as T;
}

function applyTheme(theme: ThemeId): void {
  document.documentElement.dataset.theme = theme === "jade-night" ? "jade-night" : "classic";
  required<HTMLButtonElement>("theme-classic").classList.toggle("is-active", theme === "classic");
  required<HTMLButtonElement>("theme-jade-night").classList.toggle(
    "is-active",
    theme === "jade-night",
  );
}

function showToast(text: string, isError = false): void {
  const toast = required<HTMLDivElement>("toast");
  toast.textContent = text;
  toast.classList.toggle("is-error", isError);
  toast.classList.remove("hidden");
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => toast.classList.add("hidden"), 3200);
}

function setBusy(next: boolean, label?: string): void {
  busy = next;
  const pill = required<HTMLDivElement>("quota-monitor-label").parentElement;
  pill?.classList.toggle("is-busy", next);
  required<HTMLSpanElement>("quota-monitor-label").textContent = next
    ? (label ?? "处理中…")
    : "额度就绪";
  for (const id of [
    "add-account-button",
    "library-add-button",
    "library-refresh-button",
    "import-button",
    "refresh-button",
  ]) {
    required<HTMLButtonElement>(id).disabled = next;
  }
  // 账号卡上的操作按钮由 renderSnapshot 批量生成，不在上面的固定清单里：
  // 按 DOM 现状统一禁用/恢复，让授权、切换、重命名、删除入口与全局按钮同步进忙碌态
  for (const button of document.querySelectorAll<HTMLButtonElement>(
    ".profile-card-actions button",
  )) {
    button.disabled = next;
  }
}

function routeFromHash(): Route {
  const hash = window.location.hash.replace(/^#/, "") as Route;
  return routes.includes(hash) ? hash : "overview";
}

function applyRoute(route: Route): void {
  document.querySelectorAll<HTMLAnchorElement>(".route-tab").forEach((tab) => {
    tab.classList.toggle("active", tab.dataset.route === route);
  });
  required<HTMLElement>("page-overview").classList.toggle("active", route === "overview");
  required<HTMLElement>("page-library").classList.toggle("active", route === "library");
  required<HTMLElement>("page-settings").classList.toggle("active", route === "settings");
}

// 总览当前账号与账号库卡片共用同一套额度行渲染：
// 左侧窗口名，条上直接写用量文字（DTO 的 text），右侧仅在有重置时间时显示。
// 没有重置时间时用量文字仍在条上；查询失败/空数据的文案保持原样。
// index.html 只读，账号库页眉的「刷新」按钮在初始化时注入，
// 与「添加账号」并排；行为与总览控制面板的「刷新」一致（全局重拉）。
function ensureLibraryRefreshButton(): HTMLButtonElement {
  const header = required<HTMLElement>("page-library").querySelector<HTMLElement>(
    ".profiles-header",
  );
  if (!(header instanceof HTMLElement)) {
    throw new Error("缺少元素: .profiles-header");
  }
  const actions = document.createElement("div");
  actions.className = "profiles-header-actions";
  const refresh = document.createElement("button");
  refresh.id = "library-refresh-button";
  refresh.type = "button";
  refresh.className = "action-button";
  refresh.title = "重新拉取全部账号的额度";
  refresh.textContent = "刷新";
  // append 会把既有节点移入新容器：刷新（次级）在前，添加账号（主操作）在后
  actions.append(refresh, required<HTMLButtonElement>("library-add-button"));
  header.append(actions);
  return refresh;
}

function quotaHtml(quotas: QuotaView[], error: string | null): string {
  if (error) {
    return `<p class="quota-error">额度查询失败: ${escapeHtml(error)}</p>`;
  }
  if (quotas.length === 0) {
    return `<p class="quota-reset">额度: n/a</p>`;
  }
  return quotas
    .map((q) => {
      const width = Math.max(0, Math.min(100, (q.ratio ?? 0) * 100));
      const reset = q.reset ? `<span class="quota-reset">${escapeHtml(q.reset)}</span>` : "";
      return `<div class="quota-row">
        <span class="quota-label">${escapeHtml(q.window)}</span>
        <div class="quota-track" title="${escapeHtml(q.text)}">
          <div class="quota-fill quota-fill--${escapeHtml(q.tone)}" style="width:${width}%"></div>
          <span class="quota-value">${escapeHtml(q.text)}</span>
        </div>
        ${reset}
      </div>`;
    })
    .join("");
}

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

function renderSnapshot(next: Snapshot): void {
  snapshot = next;
  applyTheme(next.theme);
  const current = next.accounts.find((row) => row.active);
  required<HTMLHeadingElement>("current-title").textContent = current?.label ?? "--";
  required<HTMLParagraphElement>("current-plan").textContent = current?.membership ?? "--";
  required<HTMLDivElement>("current-quota-panel").innerHTML = current
    ? quotaHtml(current.quotas, current.error)
    : `<p class="quota-reset">暂无当前账号，请先切换。</p>`;
  required<HTMLElement>("metric-current").textContent = next.current_label;
  required<HTMLElement>("metric-total").textContent = String(next.account_count);
  required<HTMLElement>("metric-urgent").textContent = String(next.urgent_count);

  const list = required<HTMLDivElement>("accounts-list");
  const empty = required<HTMLDivElement>("accounts-empty");
  empty.classList.toggle("hidden", next.accounts.length > 0);
  list.innerHTML = next.accounts
    .map((row) => {
      // 按卡授权：「授权」是每张卡的常驻入口，当前账号的卡也保留；
      // 当前账号卡上没有「切换」按钮是有意为之（自己切给自己无意义）。
      const actions = row.active
        ? `<button class="profile-pill" data-reauth="${escapeHtml(row.id)}" type="button" title="给这张卡重新走浏览器授权">授权</button>
           <button class="profile-pill" data-rename="${escapeHtml(row.id)}" type="button">重命名</button>
           <button class="profile-pill profile-pill--danger" data-delete="${escapeHtml(row.id)}" type="button">删除</button>`
        : `<button class="profile-pill" data-swap="${escapeHtml(row.id)}" type="button">切换</button>
           <button class="profile-pill" data-reauth="${escapeHtml(row.id)}" type="button" title="给这张卡重新走浏览器授权">授权</button>
           <button class="profile-pill" data-rename="${escapeHtml(row.id)}" type="button">重命名</button>
           <button class="profile-pill profile-pill--danger" data-delete="${escapeHtml(row.id)}" type="button">删除</button>`;
      return `<article class="profile-card${row.active ? " is-current" : ""}">
        <div class="profile-title-wrap">
          <span class="profile-title">${escapeHtml(row.label)}${row.active ? " · 当前" : ""}</span>
          <span class="profile-plan">${escapeHtml(row.membership ?? "")}</span>
        </div>
        <div class="profile-quota-stack">${quotaHtml(row.quotas, row.error)}</div>
        <div class="profile-card-actions">${actions}</div>
      </article>`;
    })
    .join("");

  if (next.message) {
    showToast(next.message.text, next.message.tone === "err");
  }
}

async function runAction(
  label: string,
  action: () => Promise<Snapshot>,
): Promise<void> {
  if (busy) {
    return;
  }
  setBusy(true, label);
  try {
    renderSnapshot(await action());
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error), true);
  } finally {
    setBusy(false);
  }
}

// 设备码授权流程：「添加账号」与单卡「重新授权」（issue #3 按卡授权）共用同一条链路。
// `target = null` 为现行添加新账号（后端行为不变）；传账号 id 则给该卡重新授权，
// 后端返回的错误字符串（如身份对不上被拒绝）直接作为错误 toast 展示，UI 不再加工。
async function startDeviceAuth(
  target: string | null,
  displayName: string,
): Promise<void> {
  if (busy) {
    return;
  }
  authIsReauth = target !== null;
  const dialog = required<HTMLDialogElement>("auth-dialog");
  const title = required<HTMLHeadingElement>("auth-dialog-title");
  const copy = required<HTMLParagraphElement>("auth-dialog-copy");
  const urlInput = required<HTMLInputElement>("auth-url");
  const code = required<HTMLParagraphElement>("auth-user-code");
  // 打开前按流程重写标题与文案：重新授权必须一眼看出是给哪张卡换凭证
  if (target === null) {
    title.textContent = "添加账号 · 浏览器授权";
  } else {
    title.textContent = `重新授权账号：${displayName}`;
  }
  copy.textContent = "正在获取授权链接…";
  urlInput.value = "";
  code.textContent = "----";
  dialog.showModal();
  setBusy(true, "正在获取授权链接…");
  try {
    const link = await invoke<AuthLink>("start_device_auth", { target });
    // 拿到链接前用户可能已经取消（对话框已关）：补一次清理，
    // 防止请求竞态导致后台留下一个没人等待、也没人取消的授权会话
    if (!dialog.open) {
      void invoke("cancel_device_auth");
      return;
    }
    urlInput.value = link.url;
    code.textContent = link.user_code || "----";
    copy.textContent =
      target === null
        ? "复制下面的链接，到浏览器打开并登录授权。完成后会自动添加。"
        : `复制下面的链接，到浏览器打开并登录。完成后会把新凭证写回「${displayName}」，登录的官方账号对不上会被拒绝。`;
    const snapshotAfter = await invoke<Snapshot>("complete_device_auth");
    dialog.close();
    renderSnapshot(snapshotAfter);
  } catch (error) {
    const text = error instanceof Error ? error.message : String(error);
    // 链接返回前用户已关窗：迟到的失败不再弹错误 toast。
    // 对话框仍开时，取消类文案继续按原规则过滤，其它失败照常提示。
    if (
      dialog.open &&
      !text.includes("已取消") &&
      text !== "没有进行中的授权"
    ) {
      showToast(text, true);
    }
    dialog.close();
  } finally {
    setBusy(false);
  }
}

function wireDialogs(): void {
  required<HTMLButtonElement>("auth-copy-url").addEventListener("click", async () => {
    await navigator.clipboard.writeText(required<HTMLInputElement>("auth-url").value);
    showToast("已复制链接");
  });
  required<HTMLButtonElement>("auth-copy-code").addEventListener("click", async () => {
    await navigator.clipboard.writeText(required<HTMLElement>("auth-user-code").textContent ?? "");
    showToast("已复制授权码");
  });
  required<HTMLButtonElement>("auth-open").addEventListener("click", async () => {
    const url = required<HTMLInputElement>("auth-url").value;
    if (url) {
      await invoke("open_url", { url });
    }
  });
  required<HTMLButtonElement>("auth-cancel").addEventListener("click", async () => {
    await invoke("cancel_device_auth");
    required<HTMLDialogElement>("auth-dialog").close();
    showToast(authIsReauth ? "已取消重新授权" : "已取消添加账号");
  });
  required<HTMLDialogElement>("auth-dialog").addEventListener("cancel", () => {
    void invoke("cancel_device_auth");
  });

  required<HTMLButtonElement>("rename-cancel").addEventListener("click", () => {
    required<HTMLDialogElement>("rename-dialog").close();
  });
  required<HTMLButtonElement>("rename-submit").addEventListener("click", async () => {
    const label = required<HTMLInputElement>("rename-input").value;
    required<HTMLDialogElement>("rename-dialog").close();
    await runAction("正在重命名…", () =>
      invoke("rename_account", { id: renameId, label }),
    );
  });

  required<HTMLButtonElement>("delete-cancel").addEventListener("click", () => {
    required<HTMLDialogElement>("delete-dialog").close();
  });
  required<HTMLButtonElement>("delete-confirm").addEventListener("click", async () => {
    required<HTMLDialogElement>("delete-dialog").close();
    await runAction("正在删除…", () => invoke("remove_account", { id: deleteId }));
  });
}

function wireList(): void {
  required<HTMLDivElement>("accounts-list").addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof HTMLElement)) {
      return;
    }
    const swap = target.closest<HTMLElement>("[data-swap]");
    const reauth = target.closest<HTMLElement>("[data-reauth]");
    const rename = target.closest<HTMLElement>("[data-rename]");
    const remove = target.closest<HTMLElement>("[data-delete]");
    if (swap?.dataset.swap) {
      void runAction("正在切换…", () => invoke("swap_account", { id: swap.dataset.swap }));
      return;
    }
    if (reauth?.dataset.reauth) {
      const id = reauth.dataset.reauth;
      const row = snapshot?.accounts.find((item) => item.id === id);
      // 标题优先展示别名，别名缺失时退回账号 id
      void startDeviceAuth(id, row?.label ?? id);
      return;
    }
    if (rename?.dataset.rename) {
      renameId = rename.dataset.rename;
      const row = snapshot?.accounts.find((item) => item.id === renameId);
      required<HTMLParagraphElement>("rename-copy").textContent =
        `给 ${renameId} 起个别名（只影响本软件里的显示）：`;
      required<HTMLInputElement>("rename-input").value = row?.label ?? "";
      required<HTMLDialogElement>("rename-dialog").showModal();
      required<HTMLInputElement>("rename-input").focus();
      return;
    }
    if (remove?.dataset.delete) {
      deleteId = remove.dataset.delete;
      required<HTMLParagraphElement>("delete-copy").textContent =
        `确定要从账号库删除 ${deleteId} 吗？`;
      required<HTMLDialogElement>("delete-dialog").showModal();
    }
  });
}

async function init(): Promise<void> {
  await setupWindowControls();
  applyRoute(routeFromHash());
  window.addEventListener("hashchange", () => applyRoute(routeFromHash()));

  // 先注入页眉刷新按钮：setBusy 会按 id 批量禁用，注入必须发生在首次 setBusy 之前
  const libraryRefreshButton = ensureLibraryRefreshButton();

  required<HTMLButtonElement>("add-account-button").addEventListener("click", () => {
    void startDeviceAuth(null, "");
  });
  required<HTMLButtonElement>("library-add-button").addEventListener("click", () => {
    void startDeviceAuth(null, "");
  });
  required<HTMLButtonElement>("import-button").addEventListener("click", () => {
    void runAction("正在导入…", () => invoke("import_current"));
  });
  required<HTMLButtonElement>("refresh-button").addEventListener("click", () => {
    void runAction("正在刷新额度…", () => invoke("refresh_accounts"));
  });
  // 与总览控制面板同一命令、同一忙碌态；runAction 内部有 busy 守卫防连点
  libraryRefreshButton.addEventListener("click", () => {
    void runAction("正在刷新额度…", () => invoke("refresh_accounts"));
  });
  required<HTMLButtonElement>("theme-classic").addEventListener("click", async () => {
    const theme = await invoke<ThemeId>("set_theme", { theme: "classic" });
    applyTheme(theme);
  });
  required<HTMLButtonElement>("theme-jade-night").addEventListener("click", async () => {
    const theme = await invoke<ThemeId>("set_theme", { theme: "jade-night" });
    applyTheme(theme);
  });

  wireDialogs();
  wireList();

  setBusy(true, "加载中…");
  try {
    const initial = await invoke<Snapshot>("list_accounts");
    renderSnapshot(initial);
  } catch (error) {
    showToast(error instanceof Error ? error.message : String(error), true);
  } finally {
    setBusy(false);
  }
}

void init();
