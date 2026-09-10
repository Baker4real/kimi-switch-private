import { escapeHtml } from "./escape";

/** 使用后端下发的显示名；行缺失时回退账号 id。 */
export function accountDisplayName(
  row: { display_name?: string | null } | null | undefined,
  id: string,
): string {
  const trimmed = row?.display_name?.trim() ?? "";
  return trimmed === "" ? id : trimmed;
}

/** 账号库卡片标题：显示名，当前账号加「 · 当前」。 */
export function accountCardTitle(row: { display_name: string; active: boolean }): string {
  return row.active ? `${row.display_name} · 当前` : row.display_name;
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
  return `<button class="profile-pill" data-rename="${escapeHtml(row.id)}" type="button"${disabled}>重命名</button>`;
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
