import { escapeHtml } from "./escape";

/** 使用后端下发的显示名；行缺失时回退账号 id。 */
export function accountDisplayName(
  row: { display_name?: string | null } | null | undefined,
  id: string,
): string {
  const trimmed = row?.display_name?.trim() ?? "";
  return trimmed === "" ? id : trimmed;
}

/** 当前账号色片；停泊账号不渲染。 */
export function currentAccountChipHtml(active: boolean): string {
  return active ? `<span class="profile-current-chip">当前</span>` : "";
}

/** 总览当前账号卡头像圆的首字符（issue #133）。
 * 直接用 display_name 单一出处，不在前端重算昵称优先级；代理对按整字取。 */
export function displayInitial(displayName: string): string {
  const chars = Array.from(displayName.trim());
  return chars.length === 0 ? "--" : chars[0];
}

/** 官方昵称已知则隐藏重命名入口。 */
export function showRenameButton(officialNicknameKnown: boolean): boolean {
  return !officialNicknameKnown;
}

export function renameButtonHtml(
  row: { id: string; official_nickname_known: boolean },
  busy: boolean,
): string {
  if (!showRenameButton(row.official_nickname_known)) {
    return "";
  }
  const disabled = busy ? " disabled" : "";
  return `<button class="profile-overflow-item" data-rename="${escapeHtml(row.id)}" type="button"${disabled}>重命名</button>`;
}

/** 账号卡右上角「…」：只收重命名；官方昵称已知时整颗隐藏。
 * disclosure 语义：aria-expanded 表开合、aria-label 给名称，
 * 菜单项是普通按钮留在 Tab 序里，不声明 menu 键盘契约。 */
export function renameOverflowHtml(
  row: { id: string; official_nickname_known: boolean },
  busy: boolean,
): string {
  const item = renameButtonHtml(row, busy);
  if (item === "") {
    return "";
  }
  const id = escapeHtml(row.id);
  const disabled = busy ? " disabled" : "";
  return `<div class="profile-overflow">
    <button class="profile-overflow-toggle" type="button" data-overflow="${id}" aria-expanded="false" aria-label="更多操作"${disabled}>
      <span class="profile-overflow-dots" aria-hidden="true"></span>
    </button>
    <div class="profile-overflow-menu" hidden>
      ${item}
    </div>
  </div>`;
}

export type DeviceAuthPhase = "fetching-link" | "waiting-browser";

export function deviceAuthBusyLabel(phase: DeviceAuthPhase): string {
  return phase === "fetching-link" ? "正在获取授权链接…" : "等待浏览器授权…";
}

/** trim 后的别名；空串视为无效。 */
export function parseAliasInput(raw: string): string | null {
  const trimmed = raw.trim();
  return trimmed.length === 0 ? null : trimmed;
}
