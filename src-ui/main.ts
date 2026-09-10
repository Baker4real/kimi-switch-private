/// <reference types="vite/client" />
import "./styles.css";
import { api, commandError, type Snapshot } from "./api";
import { runDeviceAuth, type DeviceAuthDialog } from "./deviceAuth";
import { escapeHtml } from "./escape";
import { accountCardTitle, accountDisplayName, parseAliasInput, renameButtonHtml } from "./labels";
import { credentialRecoveredToast } from "./credentialToast";
import { bindWebButtonHtml, membershipPlanHtml, quotaStackHtml } from "./membership";
import { runUserQuotaRefresh } from "./membershipToast";
import {
  autoSwapOptOutSwitchHtml,
  autoSwapPreferLowerTierSwitchView,
  autoSwapResidentLowerTierSwitchView,
  autoSwapSwitchView,
  behaviorPanelStatusText,
} from "./auto-swap";
import { createSilentQuotaScheduler } from "./silent-quota";
import { renewalSwitchView } from "./renewal";
import {
  accountFocusAttr,
  controlDisabledByBusy,
  createAppStore,
  listedActionSelector,
  listFocusKey,
  pickRestoredFocus,
  setListedActionsBusy,
  overviewCurrentEmptyCopy,
  parkedAccountCount,
  shouldArmToastDismiss,
  snapshotAnnouncement,
  toastDismissMs,
  type ListFocusKey,
  type SnapshotAnnounceSource,
} from "./store";
import { setupWindowControls } from "./window-controls";
import { parseWebToken, webBindCopy, WEB_BIND_CONSOLE_URL } from "./webBind";

type Route = "overview" | "library";

const BUSY_BUTTON_IDS = [
  "add-account-button",
  "library-add-button",
  "library-refresh-button",
  "import-button",
  "refresh-button",
  "overview-retry-button",
  "library-retry-button",
  "rename-retry-button",
  "bind-retry-button",
  "delete-retry-button",
  "auth-retry-button",
  "renewal-toggle",
  "auto-swap-toggle",
  "auto-swap-prefer-lower-tier-toggle",
  "auto-swap-resident-lower-tier-toggle",
] as const;

const routes: Route[] = ["overview", "library"];
const store = createAppStore();
let renameId = "";
let deleteId = "";
/** 忙碌禁用可能把焦点打到 body；重建后按此 data-* 身份恢复。 */
let pendingListFocus: ListFocusKey | null = null;
// 绑定网页登录（issue #51）的目标账号 id：仅对话框打开期间持有，token 不落这里。
let bindId = "";
// 最近一次打开授权对话框的流程类型：仅用于取消时的 toast 文案区分
let authIsReauth = false;
/** 最近一次操作失败的中文原因，供就近重试面板复用。 */
let lastActionError = "";
/** 账号库切换失败后的就近重试；点按钮即复用刚才那次切换。 */
let libraryRetry: (() => void) | null = null;
/** 授权失败后复用同一目标，避免回列表再找。 */
let lastAuth: { target: string | null; displayName: string } | null = null;
/** deviceAuth 失败也会调 close；为 true 时拦住，让对话框留着就近重试。 */
let authHoldOpen = false;
let toastTimer = 0;
/** 「账号行为」折叠的会话开合（issue #120）：单一布尔，默认收起，点标题取反。
 * 不因任何开关已开而自动展开；快照 / 静默列出 / 换路由不得改写；不写任何盘上存储。 */
let behaviorPanelExpanded = false;

const silentQuota = createSilentQuotaScheduler({
  clock: {
    now: () => Date.now(),
    setTimeout: (handler, delayMs) => window.setTimeout(handler, delayMs),
    clearTimeout: (id) => {
      window.clearTimeout(id);
    },
  },
  host: {
    isHidden: () => document.hidden,
    isBusy: () => store.get().busy,
    accountCount: () => {
      const snapshot = store.get().snapshot;
      return snapshot ? snapshot.accounts.length : null;
    },
    accounts: () => store.get().snapshot?.accounts ?? null,
    listAccounts: () => api.listAccounts(),
    applySnapshot: (snapshot, generation) => applySnapshot(snapshot, generation),
    toast: (text, isError) => showToast(text, isError),
  },
});

function required<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!(el instanceof HTMLElement)) {
    throw new Error(`缺少元素: ${id}`);
  }
  return el as T;
}

/** 字段控件：测试用假对象与真实 input/textarea 共用。 */
export type FieldErrorInput = {
  setAttribute(name: string, value: string): void;
  removeAttribute(name: string): void;
};

/** 字段旁错误节点：显示文案并被 aria-describedby 指向。 */
export type FieldErrorOutput = {
  id: string;
  textContent: string | null;
  classList: { add(token: string): void; remove(token: string): void };
};

/** 把校验错误画在字段旁，并用 aria-describedby / aria-invalid 关联。 */
export function paintFieldError(
  input: FieldErrorInput,
  errorEl: FieldErrorOutput,
  message: string | null,
): void {
  if (message === null) {
    errorEl.textContent = "";
    errorEl.classList.add("hidden");
    input.removeAttribute("aria-invalid");
    return;
  }
  errorEl.textContent = message;
  errorEl.classList.remove("hidden");
  input.setAttribute("aria-invalid", "true");
  input.setAttribute("aria-describedby", errorEl.id);
}

/** 别名校验：失败返回 null，调用方不得关对话框。 */
export function evaluateRenameSubmit(
  raw: string,
  input: FieldErrorInput,
  errorEl: FieldErrorOutput,
): string | null {
  const label = parseAliasInput(raw);
  paintFieldError(input, errorEl, label === null ? "别名不能为空" : null);
  return label;
}

/** 网页 token 校验：失败返回 null，调用方不得关对话框。 */
export function evaluateBindSubmit(
  raw: string,
  input: FieldErrorInput,
  errorEl: FieldErrorOutput,
): string | null {
  const token = parseWebToken(raw);
  paintFieldError(input, errorEl, token === null ? "请先粘贴网页 token" : null);
  return token;
}

/** 操作失败就近重试面板：与总览首载失败同一套「文案 + 重试」。 */
export function paintActionRetry(
  panel: { classList: { add(token: string): void; remove(token: string): void } },
  textEl: { textContent: string | null },
  message: string | null,
): void {
  if (message === null) {
    textEl.textContent = "";
    panel.classList.add("hidden");
    return;
  }
  textEl.textContent = message;
  panel.classList.remove("hidden");
}

function hideActionRetry(panelId: string, textId: string): void {
  paintActionRetry(required(panelId), required(textId), null);
}

function showActionRetry(panelId: string, textId: string, message: string): void {
  paintActionRetry(required(panelId), required(textId), message);
}

function hideLibraryRetry(): void {
  libraryRetry = null;
  hideActionRetry("library-action-error", "library-action-error-text");
}

function showLibraryRetry(message: string, retry: () => void): void {
  libraryRetry = retry;
  showActionRetry("library-action-error", "library-action-error-text", message);
}

/** 对话框内的绑定/重命名/删除：失败留在对话框里就近重试，成功才关。 */
async function runDialogSnapshotAction(opts: {
  label: string;
  action: () => Promise<Snapshot>;
  dialogId: string;
  retryPanelId: string;
  retryTextId: string;
}): Promise<void> {
  hideActionRetry(opts.retryPanelId, opts.retryTextId);
  const ok = await runAction(opts.label, opts.action);
  const dialog = required<HTMLDialogElement>(opts.dialogId);
  if (ok) {
    if (dialog.open) {
      dialog.close();
    }
    return;
  }
  if (lastActionError === "" || !dialog.open) {
    return;
  }
  showActionRetry(opts.retryPanelId, opts.retryTextId, lastActionError);
}

function submitRename(): void {
  if (store.get().busy) {
    return;
  }
  const input = required<HTMLInputElement>("rename-input");
  const errorEl = required<HTMLElement>("rename-error");
  const label = evaluateRenameSubmit(input.value, input, errorEl);
  if (label === null) {
    return;
  }
  const id = renameId;
  void runDialogSnapshotAction({
    label: "正在重命名…",
    action: () => api.renameAccount(id, label),
    dialogId: "rename-dialog",
    retryPanelId: "rename-action-error",
    retryTextId: "rename-action-error-text",
  });
}

function submitBind(): void {
  if (store.get().busy) {
    return;
  }
  const input = required<HTMLTextAreaElement>("bind-token");
  const errorEl = required<HTMLElement>("bind-token-error");
  const token = evaluateBindSubmit(input.value, input, errorEl);
  if (token === null) {
    return;
  }
  const id = bindId;
  void runDialogSnapshotAction({
    label: "正在绑定…",
    action: () => api.bindWebAccount(id, token),
    dialogId: "bind-dialog",
    retryPanelId: "bind-action-error",
    retryTextId: "bind-action-error-text",
  });
}

function submitDelete(): void {
  if (store.get().busy) {
    return;
  }
  void runDialogSnapshotAction({
    label: "正在删除…",
    action: () => api.removeAccount(deleteId),
    dialogId: "delete-dialog",
    retryPanelId: "delete-action-error",
    retryTextId: "delete-action-error-text",
  });
}

function listedActionButtons(): NodeListOf<HTMLButtonElement> {
  return document.querySelectorAll<HTMLButtonElement>(
    listedActionSelector(["#accounts-list", "#current-plan"]),
  );
}

function captureListFocus(): ListFocusKey | null {
  const active = document.activeElement;
  if (!(active instanceof HTMLElement)) {
    return null;
  }
  const account = active.closest<HTMLElement>("[data-account]");
  return listFocusKey(active.dataset, account?.dataset.account);
}

function rememberListFocus(): void {
  pendingListFocus = captureListFocus() ?? pendingListFocus;
}

function restoreListFocus(key: ListFocusKey | null): void {
  if (!key) {
    return;
  }
  pickRestoredFocus(key, (selector) => document.querySelector<HTMLElement>(selector))?.focus();
}

function syncListedActionsBusy(): void {
  setListedActionsBusy(listedActionButtons(), store.get().busy);
}

function paintProactiveRenewal(enabled: boolean): void {
  const view = renewalSwitchView(enabled);
  required<HTMLButtonElement>("renewal-toggle").setAttribute(
    "aria-checked",
    String(view.checked),
  );
}

function paintAutoSwap(enabled: boolean): void {
  const view = autoSwapSwitchView(enabled);
  required<HTMLButtonElement>("auto-swap-toggle").setAttribute(
    "aria-checked",
    String(view.checked),
  );
}

function paintAutoSwapPreferLowerTier(autoSwapEnabled: boolean, preferLowerTier: boolean): void {
  const view = autoSwapPreferLowerTierSwitchView(autoSwapEnabled, preferLowerTier);
  const toggle = required<HTMLButtonElement>("auto-swap-prefer-lower-tier-toggle");
  toggle.setAttribute("aria-checked", String(view.checked));
  toggle.disabled = controlDisabledByBusy(store.get().busy, view.disabled);
}

function paintAutoSwapResidentLowerTier(
  autoSwapEnabled: boolean,
  residentLowerTier: boolean,
): void {
  const view = autoSwapResidentLowerTierSwitchView(autoSwapEnabled, residentLowerTier);
  const toggle = required<HTMLButtonElement>("auto-swap-resident-lower-tier-toggle");
  toggle.setAttribute("aria-checked", String(view.checked));
  toggle.disabled = controlDisabledByBusy(store.get().busy, view.disabled);
}

/**
 * 账号行为折叠 paint（issue #120）：只涂 aria-expanded、面板隐藏与收起态状态行。
 * 开合只由会话布尔 behaviorPanelExpanded 决定，任何开关已开都不得自动展开。
 * 状态行只在收起且字符串非空时渲染，挂为折叠标题的描述；展开时不写；空则去掉描述。
 */
function paintBehaviorPanel(): void {
  const toggle = required<HTMLButtonElement>("behavior-panel-toggle");
  toggle.setAttribute("aria-expanded", String(behaviorPanelExpanded));
  required<HTMLElement>("behavior-panel-body").classList.toggle(
    "hidden",
    !behaviorPanelExpanded,
  );
  const checked = (id: string): boolean =>
    required<HTMLButtonElement>(id).getAttribute("aria-checked") === "true";
  const status = behaviorPanelExpanded
    ? ""
    : behaviorPanelStatusText(
        checked("renewal-toggle"),
        checked("auto-swap-toggle"),
        checked("auto-swap-prefer-lower-tier-toggle"),
        checked("auto-swap-resident-lower-tier-toggle"),
      );
  const statusEl = required<HTMLParagraphElement>("behavior-panel-status");
  statusEl.textContent = status;
  statusEl.classList.toggle("hidden", status === "");
  if (status === "") {
    toggle.removeAttribute("aria-describedby");
  } else {
    toggle.setAttribute("aria-describedby", "behavior-panel-status");
  }
}

function hasOpenDialog(): boolean {
  return document.querySelector("dialog[open]") != null;
}

function armToastDismiss(el: HTMLElement, text: string, isError: boolean): void {
  if (!shouldArmToastDismiss(isError, hasOpenDialog())) {
    return;
  }
  toastTimer = window.setTimeout(() => {
    el.classList.add("hidden");
  }, toastDismissMs(text, isError));
}

/** 对话框关掉后，若错误 toast 仍在则开始计时（开着时不在背后白跑）。 */
function armPendingErrorToast(): void {
  const alert = required<HTMLDivElement>("toast-alert");
  if (alert.classList.contains("hidden")) {
    return;
  }
  const text = required<HTMLElement>("toast-alert-text").textContent ?? "";
  window.clearTimeout(toastTimer);
  if (!shouldArmToastDismiss(true, hasOpenDialog())) {
    return;
  }
  toastTimer = window.setTimeout(() => {
    alert.classList.add("hidden");
  }, toastDismissMs(text, true));
}

function showToast(text: string, isError = false): void {
  window.clearTimeout(toastTimer);
  const status = required<HTMLDivElement>("toast-status");
  const alert = required<HTMLDivElement>("toast-alert");
  if (isError) {
    status.classList.add("hidden");
    required<HTMLElement>("toast-alert-text").textContent = text;
    alert.classList.remove("hidden");
    armToastDismiss(alert, text, true);
    return;
  }
  alert.classList.add("hidden");
  status.textContent = text;
  status.classList.remove("hidden");
  armToastDismiss(status, text, false);
}

function dismissAlertToast(): void {
  window.clearTimeout(toastTimer);
  required<HTMLDivElement>("toast-alert").classList.add("hidden");
}

function setOverviewLoadError(visible: boolean): void {
  required<HTMLDivElement>("overview-load-error").classList.toggle("hidden", !visible);
  required<HTMLElement>("overview-dashboard").classList.toggle("hidden", visible);
}

function cardActionButtons(row: Snapshot["accounts"][number], busy: boolean): string {
  const disabled = busy ? " disabled" : "";
  const reauth = `<button class="profile-pill" data-reauth="${escapeHtml(row.id)}" type="button" title="给这张卡重新走浏览器授权"${disabled}>重新授权</button>`;
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
  const list = required<HTMLDivElement>("accounts-list");
  const empty = required<HTMLDivElement>("accounts-empty");
  empty.classList.toggle("hidden", next.accounts.length > 0);
  list.innerHTML = next.accounts
    .map((row) => {
      return `<article class="profile-card${row.active ? " is-current" : ""}" ${accountFocusAttr(row.id)}>
        <div class="profile-title-wrap">
          <span class="profile-title">${escapeHtml(accountCardTitle(row))}</span>
          <span class="profile-plan">${membershipPlanHtml(row) ?? ""}${bindWebButtonHtml(row, false)}</span>
        </div>
        <div class="profile-quota-stack">${quotaStackHtml(row)}</div>
        ${autoSwapOptOutSwitchHtml(row, false)}
        <div class="profile-card-actions">${cardActionButtons(row, false)}</div>
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

/** 重画总览档位行。忙碌禁用由 syncListedActionsBusy 就地切换，避免拆掉焦点。 */
function paintCurrentPlan(snapshot: Snapshot): void {
  const current = snapshot.accounts.find((row) => row.active);
  required<HTMLParagraphElement>("current-plan").innerHTML = current
    ? currentPlanHtml(current, false)
    : "--";
}

function paintSnapshot(next: Snapshot): void {
  store.setSnapshot(next);
  setOverviewLoadError(false);
  paintProactiveRenewal(next.proactive_renewal);
  paintAutoSwap(next.auto_swap);
  paintAutoSwapPreferLowerTier(next.auto_swap, next.auto_swap_prefer_lower_tier);
  paintAutoSwapResidentLowerTier(next.auto_swap, next.auto_swap_resident_lower_tier);
  // 折叠 paint 放在四个开关之后：收起态状态行要读刚涂好的开关真值；开合不因快照改写。
  paintBehaviorPanel();
  const current = next.accounts.find((row) => row.active);
  required<HTMLHeadingElement>("current-title").textContent = current?.display_name ?? "--";
  rememberListFocus();
  const key = captureListFocus() ?? pendingListFocus;
  paintCurrentPlan(next);
  required<HTMLDivElement>("current-quota-panel").innerHTML = current
    ? quotaStackHtml(current)
    : `<p class="quota-reset">${overviewCurrentEmptyCopy(next.accounts.length)}</p>`;
  required<HTMLElement>("metric-current").textContent = String(
    parkedAccountCount(next.accounts.length, current != null),
  );
  required<HTMLElement>("metric-total").textContent = String(next.account_count);
  required<HTMLElement>("metric-urgent").textContent = String(next.urgent_count);
  paintAccountCards(next);
  restoreListFocus(key);
  syncListedActionsBusy();
  if (!store.get().busy) {
    pendingListFocus = null;
  }
}

function syncBusyChrome(): void {
  const { busy, busyLabel } = store.get();
  required<HTMLElement>("app-shell").setAttribute("aria-busy", String(busy));
  const pill = required<HTMLElement>("app-busy");
  pill.classList.toggle("is-busy", busy);
  required<HTMLSpanElement>("app-busy-label").textContent = busyLabel;
  for (const id of BUSY_BUTTON_IDS) {
    required<HTMLButtonElement>(id).disabled = busy;
  }
  const masterOn =
    required<HTMLButtonElement>("auto-swap-toggle").getAttribute("aria-checked") === "true";
  required<HTMLButtonElement>("auto-swap-prefer-lower-tier-toggle").disabled =
    controlDisabledByBusy(busy, !masterOn);
  required<HTMLButtonElement>("auto-swap-resident-lower-tier-toggle").disabled =
    controlDisabledByBusy(busy, !masterOn);
  syncListedActionsBusy();
}

function setBusy(next: boolean, label?: string): void {
  if (next) {
    rememberListFocus();
  }
  store.setBusy(next, label);
  syncBusyChrome();
  if (!next) {
    restoreListFocus(pendingListFocus);
    pendingListFocus = null;
    silentQuota.arm();
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
}

// 用户操作一开始就占住世代，已在飞的静默结果会被当成落后快照丢掉。
function reserveUserSnapshotGeneration(): number {
  return silentQuota.generation.reserveUser();
}

function applySnapshot(
  next: Snapshot,
  generation: number,
  source: SnapshotAnnounceSource = "other",
): void {
  if (!silentQuota.generation.applyIfCurrent(generation)) {
    return;
  }
  silentQuota.noteApplied();
  // paintSnapshot 会覆盖 store 快照，待续期 diff 必须用上一份。
  const previousAccounts = store.get().snapshot?.accounts ?? null;
  paintSnapshot(next);
  const announcement = snapshotAnnouncement(next.message, source);
  if (announcement) {
    showToast(announcement.text, announcement.isError);
  }
  const recovered = credentialRecoveredToast(previousAccounts, next.accounts);
  if (recovered !== null) {
    showToast(recovered);
  }
  silentQuota.arm();
}

function stopSilentQuotaQueries(): void {
  silentQuota.stop();
  document.removeEventListener("visibilitychange", onVisibilityChange);
  window.removeEventListener("pagehide", stopSilentQuotaQueries);
  window.removeEventListener("beforeunload", stopSilentQuotaQueries);
}

function onVisibilityChange(): void {
  if (document.hidden) {
    silentQuota.pause();
    return;
  }
  silentQuota.arm();
}

function wireSilentQuota(): void {
  document.addEventListener("visibilitychange", onVisibilityChange);
  window.addEventListener("pagehide", stopSilentQuotaQueries);
  window.addEventListener("beforeunload", stopSilentQuotaQueries);
}

async function runAction(label: string, action: () => Promise<Snapshot>): Promise<boolean> {
  if (store.get().busy) {
    return false;
  }
  lastActionError = "";
  setBusy(true, label);
  const generation = reserveUserSnapshotGeneration();
  try {
    applySnapshot(await action(), generation);
    hideLibraryRetry();
    return true;
  } catch (error) {
    lastActionError = commandError(error).message;
    showToast(lastActionError, true);
    return false;
  } finally {
    setBusy(false);
  }
}

/** 账号库开关类异步操作：走同一套全局忙碌指示，失败进错误 toast。 */
async function runSettingsAction(label: string, action: () => Promise<void>): Promise<void> {
  if (store.get().busy) {
    return;
  }
  setBusy(true, label);
  try {
    await action();
  } catch (error) {
    showToast(commandError(error).message, true);
  } finally {
    setBusy(false);
  }
}

/** 两个「刷新」按钮专用：列出成功后再弹会员失败 toast。启动 / 静默 / 定时不走这里。 */
async function refreshQuotas(): Promise<void> {
  if (store.get().busy) {
    return;
  }
  setBusy(true, "正在查询额度…");
  const generation = reserveUserSnapshotGeneration();
  try {
    await runUserQuotaRefresh({
      list: () => api.listAccounts(true),
      applySnapshot: (snapshot) => applySnapshot(snapshot, generation, "refresh"),
      toast: (text, isError = false) => showToast(text, isError),
    });
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
    // 启动加载不是用户点「刷新」，不弹会员到期 toast。
    applySnapshot(initial, generation);
  } catch (error) {
    showToast(commandError(error).message, true);
    setOverviewLoadError(true);
    // 首载失败不自动静默重试；成功快照后才按 90s 节奏。
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
      authHoldOpen = false;
      hideActionRetry("auth-action-error", "auth-action-error-text");
      title.textContent = input.title;
      copy.textContent = input.copy;
      urlInput.value = "";
      code.textContent = "----";
      if (!dialog.open) {
        dialog.showModal();
      }
    },
    close: () => {
      if (authHoldOpen) {
        authHoldOpen = false;
        return;
      }
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
  lastAuth = { target, displayName };
  return runDeviceAuth(target, displayName, {
    dialog: authDialogAdapter(),
    host: {
      isBusy: () => store.get().busy,
      setBusy,
      reserveGeneration: reserveUserSnapshotGeneration,
      applySnapshot,
      showError: (message) => {
        lastActionError = message;
        authHoldOpen = true;
        showActionRetry("auth-action-error", "auth-action-error-text", message);
        showToast(message, true);
      },
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
  paintFieldError(tokenInput, required("bind-token-error"), null);
  hideActionRetry("bind-action-error", "bind-action-error-text");
  required<HTMLDialogElement>("bind-dialog").showModal();
  tokenInput.focus();
}

function wireDialogs(): void {
  document.querySelectorAll("dialog").forEach((dialog) => {
    dialog.addEventListener("close", () => {
      armPendingErrorToast();
    });
  });
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
    authHoldOpen = false;
    await api.cancelDeviceAuth();
    required<HTMLDialogElement>("auth-dialog").close();
    showToast(authIsReauth ? "已取消重新授权" : "已取消添加账号");
  });
  required<HTMLButtonElement>("auth-retry-button").addEventListener("click", () => {
    if (lastAuth) {
      void startDeviceAuth(lastAuth.target, lastAuth.displayName);
    }
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
    submitBind();
  });
  required<HTMLButtonElement>("bind-retry-button").addEventListener("click", () => {
    submitBind();
  });
  required<HTMLTextAreaElement>("bind-token").addEventListener("input", () => {
    const input = required<HTMLTextAreaElement>("bind-token");
    paintFieldError(input, required("bind-token-error"), null);
  });
  // 对话框任何关闭路径（取消按钮 / Esc / 提交成功后的 close()）都清空 token 输入，
  // token 不留在 DOM。
  required<HTMLDialogElement>("bind-dialog").addEventListener("close", () => {
    const input = required<HTMLTextAreaElement>("bind-token");
    input.value = "";
    paintFieldError(input, required("bind-token-error"), null);
    hideActionRetry("bind-action-error", "bind-action-error-text");
  });

  required<HTMLButtonElement>("rename-cancel").addEventListener("click", () => {
    required<HTMLDialogElement>("rename-dialog").close();
  });
  required<HTMLFormElement>("rename-form").addEventListener("submit", (event) => {
    event.preventDefault();
    submitRename();
  });
  required<HTMLButtonElement>("rename-retry-button").addEventListener("click", () => {
    submitRename();
  });
  required<HTMLInputElement>("rename-input").addEventListener("input", () => {
    const input = required<HTMLInputElement>("rename-input");
    paintFieldError(input, required("rename-error"), null);
  });

  required<HTMLButtonElement>("delete-cancel").addEventListener("click", () => {
    required<HTMLDialogElement>("delete-dialog").close();
  });
  required<HTMLButtonElement>("delete-confirm").addEventListener("click", () => {
    submitDelete();
  });
  required<HTMLButtonElement>("delete-retry-button").addEventListener("click", () => {
    submitDelete();
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
    const optout = target.closest<HTMLElement>("[data-optout]");
    if (optout?.dataset.optout) {
      const id = optout.dataset.optout;
      const next = optout.getAttribute("aria-checked") !== "true";
      void runAction("正在保存…", () => api.setAutoSwapOptOut(id, next));
      return;
    }
    if (swap?.dataset.swap) {
      const id = swap.dataset.swap;
      const retrySwap = (): void => {
        hideLibraryRetry();
        void (async () => {
          const ok = await runAction("正在切换…", () => api.swapAccount(id));
          if (!ok && lastActionError !== "") {
            showLibraryRetry(lastActionError, retrySwap);
          }
        })();
      };
      retrySwap();
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
      const input = required<HTMLInputElement>("rename-input");
      input.value = row?.label ?? "";
      paintFieldError(input, required("rename-error"), null);
      hideActionRetry("rename-action-error", "rename-action-error-text");
      required<HTMLDialogElement>("rename-dialog").showModal();
      input.focus();
      return;
    }
    if (remove?.dataset.delete) {
      deleteId = remove.dataset.delete;
      const row = store.get().snapshot?.accounts.find((item) => item.id === deleteId);
      required<HTMLParagraphElement>("delete-copy").textContent =
        `确定要从账号库删除 ${accountDisplayName(row, deleteId)} 吗？`;
      hideActionRetry("delete-action-error", "delete-action-error-text");
      required<HTMLDialogElement>("delete-dialog").showModal();
    }
  });
}

async function init(): Promise<void> {
  await setupWindowControls();
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
    void refreshQuotas();
  });
  required<HTMLButtonElement>("library-refresh-button").addEventListener("click", () => {
    void refreshQuotas();
  });
  required<HTMLButtonElement>("overview-retry-button").addEventListener("click", () => {
    void loadInitialSnapshot();
  });
  required<HTMLButtonElement>("library-retry-button").addEventListener("click", () => {
    libraryRetry?.();
  });
  required<HTMLButtonElement>("toast-alert-dismiss").addEventListener("click", () => {
    dismissAlertToast();
  });
  required<HTMLButtonElement>("renewal-toggle").addEventListener("click", () => {
    const toggle = required<HTMLButtonElement>("renewal-toggle");
    const next = toggle.getAttribute("aria-checked") !== "true";
    void runSettingsAction("正在保存设置…", async () => {
      const saved = await api.setProactiveRenewal(next);
      paintProactiveRenewal(saved);
      // 续期也进状态行：保存成功后立刻重涂收起态文案，不等下一次快照。
      paintBehaviorPanel();
      showToast(renewalSwitchView(saved).toast);
    });
  });
  required<HTMLButtonElement>("behavior-panel-toggle").addEventListener("click", () => {
    // 开合只是会话布尔取反：busy 时仍可点、不弹 toast、不写盘。
    behaviorPanelExpanded = !behaviorPanelExpanded;
    paintBehaviorPanel();
  });
  required<HTMLButtonElement>("auto-swap-toggle").addEventListener("click", () => {
    const toggle = required<HTMLButtonElement>("auto-swap-toggle");
    const next = toggle.getAttribute("aria-checked") !== "true";
    void runSettingsAction("正在保存设置…", async () => {
      const saved = await api.setAutoSwap(next);
      paintAutoSwap(saved);
      const prefer =
        required<HTMLButtonElement>("auto-swap-prefer-lower-tier-toggle").getAttribute(
          "aria-checked",
        ) === "true";
      paintAutoSwapPreferLowerTier(saved, prefer);
      const resident =
        required<HTMLButtonElement>("auto-swap-resident-lower-tier-toggle").getAttribute(
          "aria-checked",
        ) === "true";
      paintAutoSwapResidentLowerTier(saved, resident);
      paintBehaviorPanel();
      showToast(autoSwapSwitchView(saved).toast);
    });
  });
  required<HTMLButtonElement>("auto-swap-prefer-lower-tier-toggle").addEventListener(
    "click",
    () => {
      const toggle = required<HTMLButtonElement>("auto-swap-prefer-lower-tier-toggle");
      if (toggle.disabled) {
        return;
      }
      const next = toggle.getAttribute("aria-checked") !== "true";
      void runSettingsAction("正在保存设置…", async () => {
        const saved = await api.setAutoSwapPreferLowerTier(next);
        const masterOn =
          required<HTMLButtonElement>("auto-swap-toggle").getAttribute("aria-checked") ===
          "true";
        paintAutoSwapPreferLowerTier(masterOn, saved);
        paintBehaviorPanel();
        showToast(autoSwapPreferLowerTierSwitchView(masterOn, saved).toast);
      });
    },
  );
  required<HTMLButtonElement>("auto-swap-resident-lower-tier-toggle").addEventListener(
    "click",
    () => {
      const toggle = required<HTMLButtonElement>("auto-swap-resident-lower-tier-toggle");
      if (toggle.disabled) {
        return;
      }
      const next = toggle.getAttribute("aria-checked") !== "true";
      void runSettingsAction("正在保存设置…", async () => {
        const saved = await api.setAutoSwapResidentLowerTier(next);
        const masterOn =
          required<HTMLButtonElement>("auto-swap-toggle").getAttribute("aria-checked") ===
          "true";
        paintAutoSwapResidentLowerTier(masterOn, saved);
        paintBehaviorPanel();
        showToast(autoSwapResidentLowerTierSwitchView(masterOn, saved).toast);
      });
    },
  );

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

if (!import.meta.env.VITEST) {
  void init();
}
