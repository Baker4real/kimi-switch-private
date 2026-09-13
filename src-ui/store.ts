import { escapeHtml } from "./escape";
import { isErrorMessageTone, type Snapshot } from "./api";

const IDLE_BUSY_LABEL = "就绪";

export interface AppState {
  snapshot: Snapshot | null;
  busy: boolean;
  busyLabel: string;
}

export function createAppStore(initial?: Partial<AppState>) {
  let state: AppState = {
    snapshot: null,
    busy: false,
    busyLabel: IDLE_BUSY_LABEL,
    ...initial,
  };

  return {
    get(): AppState {
      return state;
    },
    setBusy(busy: boolean, label?: string): void {
      state = {
        ...state,
        busy,
        busyLabel: busy ? (label ?? "处理中…") : IDLE_BUSY_LABEL,
      };
    },
    setSnapshot(snapshot: Snapshot | null): void {
      state = { ...state, snapshot };
    },
  };
}

export type AppStore = ReturnType<typeof createAppStore>;

/** 停泊账号数：账号库总数减去当前账号（若有）。 */
export function parkedAccountCount(accountCount: number, hasCurrent: boolean): number {
  return Math.max(0, accountCount - (hasCurrent ? 1 : 0));
}

/** 总览无当前账号时的空态文案：零账号先添加或导入；有停泊账号则去账号库切换。 */
export function overviewCurrentEmptyCopy(accountCount: number): string {
  return accountCount === 0 ? "请先添加或导入" : "请到「账号库」切换";
}

/** 账号库卡片上的操作 data-*，忙碌时只改这些按钮的 disabled。 */
export type ListActionAttr = "swap" | "reauth" | "rename" | "delete" | "bind" | "optout" | "overflow";

export interface ListFocusKey {
  action: ListActionAttr | null;
  accountId: string;
}

const LIST_ACTION_ATTRS: ListActionAttr[] = [
  "swap",
  "reauth",
  "rename",
  "delete",
  "bind",
  "optout",
  "overflow",
];

/** 把 data-* 选择器限定在给定根下，避免逗号选择器漏出作用域。 */
export function listedActionSelector(roots: readonly string[]): string {
  return roots
    .flatMap((root) => LIST_ACTION_ATTRS.map((attr) => `${root} [data-${attr}]`))
    .join(",");
}

/** 账号卡根节点：重建 DOM 后按账号找回等价控件。 */
export function accountFocusAttr(id: string): string {
  return `data-account="${escapeHtml(id)}"`;
}

export function listFocusKey(
  dataset: {
    swap?: string;
    reauth?: string;
    rename?: string;
    delete?: string;
    bind?: string;
    optout?: string;
    overflow?: string;
  },
  accountId?: string,
): ListFocusKey | null {
  for (const action of LIST_ACTION_ATTRS) {
    const id = dataset[action];
    if (id) {
      return { action, accountId: accountId ?? id };
    }
  }
  if (accountId) {
    return { action: null, accountId };
  }
  return null;
}

function attrSelector(name: string, id: string): string {
  const escaped = id.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
  return `[data-${name}="${escaped}"]`;
}

export function listFocusSelectors(key: ListFocusKey): string[] {
  const account = attrSelector("account", key.accountId);
  const selectors: string[] = [];
  if (key.action) {
    selectors.push(attrSelector(key.action, key.accountId));
  }
  selectors.push(`${account} .profile-card-actions button`);
  selectors.push(`${account} button`);
  return selectors;
}

/** 精确 data-* 优先；切换后该账号不再有「切换」钮时回落同卡操作列。 */
export function pickRestoredFocus<T>(
  key: ListFocusKey,
  query: (selector: string) => T | null,
): T | null {
  for (const selector of listFocusSelectors(key)) {
    const node = query(selector);
    if (node) {
      return node;
    }
  }
  return null;
}

/** 忙碌闸门：就地改 disabled，不重建列表节点。 */
export function setListedActionsBusy(
  buttons: Iterable<{ disabled: boolean }>,
  busy: boolean,
): void {
  for (const button of buttons) {
    button.disabled = busy;
  }
}

const SUCCESS_TOAST_MS = 3200;
const ERROR_TOAST_MIN_MS = 20_000;
const ERROR_TOAST_MS_PER_CHAR = 400;

export type SnapshotAnnounceSource = "refresh" | "other";

/** 把快照 message 收成一句可读公告；用户刷新且无句时用「额度已刷新」，不报裸数字。 */
export function snapshotAnnouncement(
  message: { text: string; tone: string; kind?: string } | null,
  source: SnapshotAnnounceSource,
): { text: string; isError: boolean } | null {
  if (message) {
    return { text: message.text, isError: isErrorMessageTone(message.tone) };
  }
  if (source === "refresh") {
    return { text: "额度已刷新", isError: false };
  }
  return null;
}

/** 完成 toast 数秒后收起；错误按字数给阅读时间，最短 20 秒。 */
export function toastDismissMs(text: string, isError: boolean): number {
  if (!isError) {
    return SUCCESS_TOAST_MS;
  }
  return Math.max(ERROR_TOAST_MIN_MS, text.length * ERROR_TOAST_MS_PER_CHAR);
}

/** 忙碌闸门与视图禁用取或：paint 不得在忙碌时把 disabled 清掉。 */
export function controlDisabledByBusy(busy: boolean, viewDisabled: boolean): boolean {
  return busy || viewDisabled;
}

/** 手动切换成功后的立即视图（issue #157）：据后端成功响应把所点账号标记为当前
 * 账号、其余卡取消当前标记；每行的额度数字、会员档位等沿用既有快照缓存值，
 * 界面不空白、不错配账号。message 置 null——切换成功的 toast 由切换流单独弹，
 * 不重放上一份快照的旧消息。目标账号不在既有快照（界面数据已过期等）时原样
 * 返回，当前标记交给随后的后台快照纠正。 */
export function snapshotWithSwitchedCurrent(
  snapshot: Snapshot,
  accountId: string,
): Snapshot {
  const target = snapshot.accounts.find((row) => row.id === accountId);
  if (!target) {
    return snapshot;
  }
  return {
    ...snapshot,
    accounts: snapshot.accounts.map((row) => ({ ...row, active: row.id === accountId })),
    current_label: target.display_name,
    message: null,
  };
}

/** 对话框开着时错误 toast 不自动收起；成功 toast 仍按原计时。 */
export function shouldArmToastDismiss(isError: boolean, dialogOpen: boolean): boolean {
  return !(isError && dialogOpen);
}
