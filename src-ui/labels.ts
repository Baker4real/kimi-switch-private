/** 有别名用别名，空/空白别名退回账号 id。 */
export function accountDisplayName(label: string | null | undefined, id: string): string {
  const trimmed = label?.trim() ?? "";
  return trimmed === "" ? id : trimmed;
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
