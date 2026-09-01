import "./styles.css";
import { api, commandError, type Snapshot } from "./api";
import { runDeviceAuth, type DeviceAuthDialog } from "./deviceAuth";
import { escapeHtml } from "./escape";
import { accountCardTitle, accountDisplayName, parseAliasInput, renameButtonHtml } from "./labels";
import { bindWebButtonHtml, membershipPlanHtml } from "./membership";
import { quotaHtml } from "./quota";
import { renewalSwitchView } from "./renewal";
import { createAppStore } from "./store";
import {
  parseMirroredTheme,
  persistThemeMirror,
  themeApplication,
  THEME_STORAGE_KEY,
  type ThemeId,
} from "./theme";
import { setupWindowControls } from "./window-controls";
import { parseWebToken, webBindCopy, WEB_BIND_CONSOLE_URL } from "./webBind";

type Route = "overview" | "library" | "settings";

const BUSY_BUTTON_IDS = [
  "add-account-button",
  "library-add-button",
  "library-refresh-button",
  "import-button",
  "refresh-button",
  "overview-retry-button",
] as const;

const routes: Route[] = ["overview", "library", "settings"];
const store = createAppStore();
let renameId = "";
let deleteId = "";
// 绑定网页登录（issue #51）的目标账号 id：仅对话框打开期间持有，token 不落这里。
let bindId = "";
// 最近一次打开授权对话框的流程类型：仅用于取消时的 toast 文案区分
let authIsReauth = false;
let toastTimer = 0;
// 与 core `QUOTA_MIN_REFRESH_INTERVAL_MS` 默认一致；前端拿不到运行时配置，不另做产品开关。
const SILENT_QUOTA_INTERVAL_MS = 90_000;
let snapshotGeneration = 0;
let appliedGeneration = 0;
let lastSnapshotAt = 0;
let silentTimer = 0;
let silentInFlight = false;
let silentStopped = false;

function required<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!(el instanceof HTMLElement)) {
    throw new Error(`缺少元素: ${id}`);
  }
  return el as T;
}

function applyTheme(theme: ThemeId): void {
  const decision = themeApplication(theme);
  document.documentElement.dataset.theme = decision.dataTheme;
  persistThemeMirror(decision.dataTheme);
  const classic = required<HTMLButtonElement>("theme-classic");
  const jade = required<HTMLButtonElement>("theme-jade-night");
  classic.classList.toggle("is-active", decision.classic.active);
  jade.classList.toggle("is-active", decision.jadeNight.active);
  classic.setAttribute("aria-pressed", String(decision.classic.pressed));
  jade.setAttribute("aria-pressed", String(decision.jadeNight.pressed));
}

function applyMirroredTheme(): void {
  try {
    const mirrored = parseMirroredTheme(localStorage.getItem(THEME_STORAGE_KEY));
    if (mirrored) {
      applyTheme(mirrored);
    }
  } catch {
    // 镜像不可读时等快照权威值
  }
}

function paintProactiveRenewal(enabled: boolean): void {
  const view = renewalSwitchView(enabled);
  required<HTMLButtonElement>("renewal-toggle").setAttribute(
    "aria-checked",
    String(view.checked),
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

function setOverviewLoadError(visible: boolean): void {
  required<HTMLDivElement>("overview-load-error").classList.toggle("hidden", !visible);
  required<HTMLElement>("overview-dashboard").classList.toggle("hidden", visible);
}

function cardActionButtons(row: Snapshot["accounts"][number], busy: boolean): string {
  const disabled = busy ? " disabled" : "";
  const reauth = `<button class="profile-pill" data-reauth="${escapeHtml(row.id)}" type="button" title="给这张卡重新走浏览器授权"${disabled}>授权</button>`;
  const rename = renameButtonHtml(row, busy);
  const remove = `<button class="profile-pill profile-pill--danger" data-delete="${escapeHtml(row.id)}" type="button"${disabled}>删除</button>`;
  const renameGap = rename ? `\n           ${rename}` : "";
  if (row.active) {
    return `${reauth}${renameGap}
           ${remove}`;
  }
  return `<button class="profile-pill" data-swap="${escapeHtml(row.id)}" type="button"${disabled}>切换</button>
           ${reauth}${renameGap}
           ${remove}`;
}

function paintAccountCards(next: Snapshot): void {
  const busy = store.get().busy;
  const list = required<HTMLDivElement>("accounts-list");
  const empty = required<HTMLDivElement>("accounts-empty");
  empty.classList.toggle("hidden", next.accounts.length > 0);
  list.innerHTML = next.accounts
    .map((row) => {
      return `<article class="profile-card${row.active ? " is-current" : ""}">
        <div class="profile-title-wrap">
          <span class="profile-title">${escapeHtml(accountCardTitle(row))}</span>
          <span class="profile-plan">${membershipPlanHtml(row) ?? ""}${bindWebButtonHtml(row, busy)}</span>
        </div>
        <div class="profile-quota-stack">${quotaHtml(row.quotas, row.error, row.cached_at, row.credential_expired)}</div>
        <div class="profile-card-actions">${cardActionButtons(row, busy)}</div>
      </article>`;
    })
    .join("");
}

/** 总览当前账号档位行：绑定入口与账号卡同一规则（issue #51），busy 时同样
 * 禁用（防与进行中操作交错）。无档位且无绑定入口时保持「--」。 */
function currentPlanHtml(
  row: Snapshot["accounts"][number],
  busy: boolean,
): string {
  const plan = membershipPlanHtml(row);
  const bind = bindWebButtonHtml(row, busy);
  return plan === null && bind === "" ? "--" : `${plan ?? ""}${bind}`;
}

/** 重画总览档位行；busy 翻转时与账号卡一同重画（syncBusyChrome），绑定按钮
 * 不残留上一次操作期间的禁用态。 */
function paintCurrentPlan(snapshot: Snapshot): void {
  const current = snapshot.accounts.find((row) => row.active);
  required<HTMLParagraphElement>("current-plan").innerHTML = current
    ? currentPlanHtml(current, store.get().busy)
    : "--";
}

function paintSnapshot(next: Snapshot): void {
  store.setSnapshot(next);
  setOverviewLoadError(false);
  applyTheme(next.theme);
  paintProactiveRenewal(next.proactive_renewal);
  const current = next.accounts.find((row) => row.active);
  required<HTMLHeadingElement>("current-title").textContent = current?.display_name ?? "--";
  paintCurrentPlan(next);
  required<HTMLDivElement>("current-quota-panel").innerHTML = current
    ? quotaHtml(current.quotas, current.error, current.cached_at, current.credential_expired)
    : `<p class="quota-reset">暂无当前账号，请先切换。</p>`;
  required<HTMLElement>("metric-current").textContent = next.current_label;
  required<HTMLElement>("metric-total").textContent = String(next.account_count);
  required<HTMLElement>("metric-urgent").textContent = String(next.urgent_count);
  paintAccountCards(next);
}

function syncBusyChrome(): void {
  const { busy, busyLabel, snapshot } = store.get();
  const pill = required<HTMLDivElement>("quota-monitor-label").parentElement;
  pill?.classList.toggle("is-busy", busy);
  required<HTMLSpanElement>("quota-monitor-label").textContent = busyLabel;
  for (const id of BUSY_BUTTON_IDS) {
    required<HTMLButtonElement>(id).disabled = busy;
  }
  if (snapshot) {
    paintAccountCards(snapshot);
    paintCurrentPlan(snapshot);
  }
}

function setBusy(next: boolean, label?: string): void {
  store.setBusy(next, label);
  syncBusyChrome();
  if (!next) {
    armSilentQuota();
  }
}

function routeFromHash(): Route {
  const hash = window.location.hash.replace(/^#/, "") as Route;
  return routes.includes(hash) ? hash : "overview";
}

function applyRoute(route: Route): void {
  document.querySelectorAll<HTMLAnchorElement>(".route-tab").forEach((tab) => {
    const active = tab.dataset.route === route;
    tab.classList.toggle("active", active);
    if (active) {
      tab.setAttribute("aria-current", "page");
    } else {
      tab.removeAttribute("aria-current");
    }
  });
  required<HTMLElement>("page-overview").classList.toggle("active", route === "overview");
  required<HTMLElement>("page-library").classList.toggle("active", route === "library");
  required<HTMLElement>("page-settings").classList.toggle("active", route === "settings");
}

function nextSnapshotGeneration(): number {
  snapshotGeneration += 1;
  return snapshotGeneration;
}

// 用户操作一开始就占住世代，已在飞的静默结果会被当成落后快照丢掉。
function reserveUserSnapshotGeneration(): number {
  const generation = nextSnapshotGeneration();
  appliedGeneration = generation;
  return generation;
}

function applySnapshot(next: Snapshot, generation: number): void {
  if (generation < appliedGeneration) {
    return;
  }
  appliedGeneration = generation;
  lastSnapshotAt = Date.now();
  paintSnapshot(next);
  if (next.message) {
    showToast(next.message.text, next.message.tone === "err");
  }
  armSilentQuota();
}

function stopSilentQuotaTimer(): void {
  window.clearTimeout(silentTimer);
  silentTimer = 0;
}

function stopSilentQuotaQueries(): void {
  if (silentStopped) {
    return;
  }
  silentStopped = true;
  stopSilentQuotaTimer();
  document.removeEventListener("visibilitychange", onVisibilityChange);
  window.removeEventListener("pagehide", stopSilentQuotaQueries);
  window.removeEventListener("beforeunload", stopSilentQuotaQueries);
}

function onVisibilityChange(): void {
  if (document.hidden) {
    stopSilentQuotaTimer();
    return;
  }
  armSilentQuota();
}

function armSilentQuota(): void {
  stopSilentQuotaTimer();
  if (silentStopped || document.hidden) {
    return;
  }
  const snapshot = store.get().snapshot;
  if (snapshot && snapshot.accounts.length === 0) {
    return;
  }
  const delay = Math.max(0, SILENT_QUOTA_INTERVAL_MS - (Date.now() - lastSnapshotAt));
  silentTimer = window.setTimeout(() => {
    void runSilentQuotaQuery();
  }, delay);
}

async function runSilentQuotaQuery(): Promise<void> {
  if (silentStopped || document.hidden || store.get().busy || silentInFlight) {
    return;
  }
  const snapshot = store.get().snapshot;
  if (snapshot && snapshot.accounts.length === 0) {
    return;
  }
  silentInFlight = true;
  const generation = nextSnapshotGeneration();
  try {
    const next = await api.listAccounts();
    if (silentStopped || store.get().busy) {
      return;
    }
    applySnapshot({ ...next, message: null }, generation);
  } catch {
    // 静默失败不 toast；有上次成功快照则条留在当前界面
  } finally {
    silentInFlight = false;
    lastSnapshotAt = Date.now();
    armSilentQuota();
  }
}

function wireSilentQuota(): void {
  document.addEventListener("visibilitychange", onVisibilityChange);
  window.addEventListener("pagehide", stopSilentQuotaQueries);
  window.addEventListener("beforeunload", stopSilentQuotaQueries);
}

async function runAction(label: string, action: () => Promise<Snapshot>): Promise<void> {
  if (store.get().busy) {
    return;
  }
  setBusy(true, label);
  const generation = reserveUserSnapshotGeneration();
  try {
    applySnapshot(await action(), generation);
  } catch (error) {
    showToast(commandError(error).message, true);
  } finally {
    setBusy(false);
  }
}

async function loadInitialSnapshot(): Promise<void> {
  if (store.get().busy) {
    return;
  }
  setBusy(true, "加载中…");
  const generation = reserveUserSnapshotGeneration();
  try {
    const initial = await api.listAccounts();
    applySnapshot(initial, generation);
  } catch (error) {
    showToast(commandError(error).message, true);
    setOverviewLoadError(true);
  } finally {
    setBusy(false);
  }
}

function selectElementText(el: HTMLElement): void {
  if (el instanceof HTMLInputElement) {
    el.focus();
    el.select();
    return;
  }
  const selection = window.getSelection();
  const range = document.createRange();
  range.selectNodeContents(el);
  selection?.removeAllRanges();
  selection?.addRange(range);
}

async function copyWithFallback(
  text: string,
  okMessage: string,
  failMessage: string,
  target: HTMLElement,
): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    showToast(okMessage);
  } catch {
    selectElementText(target);
    showToast(failMessage, true);
  }
}

function authDialogAdapter(): DeviceAuthDialog {
  const dialog = required<HTMLDialogElement>("auth-dialog");
  const title = required<HTMLHeadingElement>("auth-dialog-title");
  const copy = required<HTMLParagraphElement>("auth-dialog-copy");
  const urlInput = required<HTMLInputElement>("auth-url");
  const code = required<HTMLParagraphElement>("auth-user-code");
  return {
    isOpen: () => dialog.open,
    show(input) {
      title.textContent = input.title;
      copy.textContent = input.copy;
      urlInput.value = "";
      code.textContent = "----";
      dialog.showModal();
    },
    close: () => {
      dialog.close();
    },
    setLink(url, userCode) {
      urlInput.value = url;
      code.textContent = userCode;
    },
    setCopy(text) {
      copy.textContent = text;
    },
  };
}

function startDeviceAuth(target: string | null, displayName: string): Promise<void> {
  if (store.get().busy) {
    return Promise.resolve();
  }
  authIsReauth = target !== null;
  return runDeviceAuth(target, displayName, {
    dialog: authDialogAdapter(),
    host: {
      isBusy: () => store.get().busy,
      setBusy,
      reserveGeneration: reserveUserSnapshotGeneration,
      applySnapshot,
      showError: (message) => showToast(message, true),
    },
    commands: {
      startDeviceAuth: (id) => api.startDeviceAuth(id),
      completeDeviceAuth: () => api.completeDeviceAuth(),
      cancelDeviceAuth: () => api.cancelDeviceAuth(),
    },
  });
}

/** 打开绑定对话框（issue #51）：粘贴来源见文案；token 输入随每次关闭清空（close 事件），
 * 这里再清一次保证上次残留不出现。忙碌时不打开（防与进行中操作交错）。 */
function openBindDialog(id: string): void {
  if (store.get().busy) {
    return;
  }
  bindId = id;
  const row = store.get().snapshot?.accounts.find((item) => item.id === id);
  required<HTMLParagraphElement>("bind-copy").textContent = webBindCopy(
    accountDisplayName(row, id),
  );
  const tokenInput = required<HTMLTextAreaElement>("bind-token");
  tokenInput.value = "";
  required<HTMLDialogElement>("bind-dialog").showModal();
  tokenInput.focus();
}

function wireDialogs(): void {
  required<HTMLButtonElement>("auth-copy-url").addEventListener("click", () => {
    const input = required<HTMLInputElement>("auth-url");
    void copyWithFallback(input.value, "已复制链接", "复制失败，请手动复制链接", input);
  });
  required<HTMLButtonElement>("auth-copy-code").addEventListener("click", () => {
    const code = required<HTMLElement>("auth-user-code");
    void copyWithFallback(
      code.textContent ?? "",
      "已复制授权码",
      "复制失败，请手动复制授权码",
      code,
    );
  });
  required<HTMLButtonElement>("auth-open").addEventListener("click", async () => {
    const url = required<HTMLInputElement>("auth-url").value;
    if (url) {
      try {
        await api.openUrl(url);
      } catch {
        showToast("无法打开浏览器，请复制链接手动打开", true);
      }
    }
  });
  required<HTMLButtonElement>("auth-cancel").addEventListener("click", async () => {
    await api.cancelDeviceAuth();
    required<HTMLDialogElement>("auth-dialog").close();
    showToast(authIsReauth ? "已取消重新授权" : "已取消添加账号");
  });
  required<HTMLDialogElement>("auth-dialog").addEventListener("cancel", () => {
    void api.cancelDeviceAuth();
  });

  // 绑定网页登录（issue #51）：打开控制台 → 粘贴 token → 提交走 bind-web 管道。
  required<HTMLButtonElement>("bind-open").addEventListener("click", async () => {
    try {
      await api.openUrl(WEB_BIND_CONSOLE_URL);
    } catch {
      showToast("无法打开浏览器，请手动打开 kimi.com 控制台", true);
    }
  });
  required<HTMLButtonElement>("bind-cancel").addEventListener("click", () => {
    required<HTMLDialogElement>("bind-dialog").close();
  });
  required<HTMLButtonElement>("bind-submit").addEventListener("click", () => {
    const token = parseWebToken(required<HTMLTextAreaElement>("bind-token").value);
    if (token === null) {
      showToast("请先粘贴网页 token", true);
      return;
    }
    const id = bindId;
    // 关闭即清空输入（下方 close 事件）；token 只活在本次闭包里，随命令交给后端。
    required<HTMLDialogElement>("bind-dialog").close();
    void runAction("正在绑定…", () => api.bindWebAccount(id, token));
  });
  // 对话框任何关闭路径（取消按钮 / Esc / 提交后的 close()）都清空 token 输入，
  // token 不留在 DOM。
  required<HTMLDialogElement>("bind-dialog").addEventListener("close", () => {
    required<HTMLTextAreaElement>("bind-token").value = "";
  });

  required<HTMLButtonElement>("rename-cancel").addEventListener("click", () => {
    required<HTMLDialogElement>("rename-dialog").close();
  });
  required<HTMLFormElement>("rename-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const label = parseAliasInput(required<HTMLInputElement>("rename-input").value);
    if (label === null) {
      showToast("别名不能为空", true);
      return;
    }
    required<HTMLDialogElement>("rename-dialog").close();
    void runAction("正在重命名…", () => api.renameAccount(renameId, label));
  });

  required<HTMLButtonElement>("delete-cancel").addEventListener("click", () => {
    required<HTMLDialogElement>("delete-dialog").close();
  });
  required<HTMLButtonElement>("delete-confirm").addEventListener("click", async () => {
    required<HTMLDialogElement>("delete-dialog").close();
    await runAction("正在删除…", () => api.removeAccount(deleteId));
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
    const bind = target.closest<HTMLElement>("[data-bind]");
    if (swap?.dataset.swap) {
      void runAction("正在切换…", () => api.swapAccount(swap.dataset.swap ?? ""));
      return;
    }
    if (bind?.dataset.bind) {
      openBindDialog(bind.dataset.bind);
      return;
    }
    if (reauth?.dataset.reauth) {
      const id = reauth.dataset.reauth;
      const row = store.get().snapshot?.accounts.find((item) => item.id === id);
      void startDeviceAuth(id, accountDisplayName(row, id));
      return;
    }
    if (rename?.dataset.rename) {
      renameId = rename.dataset.rename;
      const row = store.get().snapshot?.accounts.find((item) => item.id === renameId);
      const display = accountDisplayName(row, renameId);
      required<HTMLParagraphElement>("rename-copy").textContent =
        `给 ${display} 起个别名（只影响本软件里的显示）：`;
      required<HTMLInputElement>("rename-input").value = row?.label ?? "";
      required<HTMLDialogElement>("rename-dialog").showModal();
      required<HTMLInputElement>("rename-input").focus();
      return;
    }
    if (remove?.dataset.delete) {
      deleteId = remove.dataset.delete;
      const row = store.get().snapshot?.accounts.find((item) => item.id === deleteId);
      required<HTMLParagraphElement>("delete-copy").textContent =
        `确定要从账号库删除 ${accountDisplayName(row, deleteId)} 吗？`;
      required<HTMLDialogElement>("delete-dialog").showModal();
    }
  });
}

async function init(): Promise<void> {
  await setupWindowControls();
  applyMirroredTheme();
  applyRoute(routeFromHash());
  window.addEventListener("hashchange", () => applyRoute(routeFromHash()));
  wireSilentQuota();

  required<HTMLButtonElement>("add-account-button").addEventListener("click", () => {
    void startDeviceAuth(null, "");
  });
  required<HTMLButtonElement>("library-add-button").addEventListener("click", () => {
    void startDeviceAuth(null, "");
  });
  required<HTMLButtonElement>("import-button").addEventListener("click", () => {
    void runAction("正在导入…", () => api.importCurrent());
  });
  required<HTMLButtonElement>("refresh-button").addEventListener("click", () => {
    void runAction("正在刷新额度…", () => api.listAccounts(true));
  });
  required<HTMLButtonElement>("library-refresh-button").addEventListener("click", () => {
    void runAction("正在刷新额度…", () => api.listAccounts(true));
  });
  required<HTMLButtonElement>("overview-retry-button").addEventListener("click", () => {
    void loadInitialSnapshot();
  });
  required<HTMLButtonElement>("theme-classic").addEventListener("click", async () => {
    const theme = await api.setTheme("classic");
    applyTheme(theme);
  });
  required<HTMLButtonElement>("theme-jade-night").addEventListener("click", async () => {
    const theme = await api.setTheme("jade-night");
    applyTheme(theme);
  });
  required<HTMLButtonElement>("renewal-toggle").addEventListener("click", async () => {
    const toggle = required<HTMLButtonElement>("renewal-toggle");
    const next = toggle.getAttribute("aria-checked") !== "true";
    try {
      const saved = await api.setProactiveRenewal(next);
      paintProactiveRenewal(saved);
      showToast(renewalSwitchView(saved).toast);
    } catch (error) {
      showToast(commandError(error).message, true);
    }
  });

  wireDialogs();
  wireList();
  // 总览当前账号档位行的绑定入口（issue #51）：与账号卡同一对话框。
  required<HTMLParagraphElement>("current-plan").addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof HTMLElement)) {
      return;
    }
    const bind = target.closest<HTMLElement>("[data-bind]");
    if (bind?.dataset.bind) {
      openBindDialog(bind.dataset.bind);
    }
  });

  await loadInitialSnapshot();
}

void init();
