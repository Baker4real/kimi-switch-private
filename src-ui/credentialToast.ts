/** 相邻快照 diff 只需账号 id 与凭证待续期。 */
export interface CredentialToastAccount {
  id: string;
  credential_expired: boolean;
}

/** 待续期行在相邻快照间消失（同账号不再待续期）时弹「额度已恢复」；其余转换返回 null。 */
export function credentialRecoveredToast(
  previous: readonly CredentialToastAccount[] | null,
  next: readonly CredentialToastAccount[],
): string | null {
  if (previous === null) {
    return null;
  }
  const nextById = new Map(next.map((row) => [row.id, row]));
  for (const row of previous) {
    if (row.credential_expired !== true) {
      continue;
    }
    const later = nextById.get(row.id);
    if (later && later.credential_expired !== true) {
      return "额度已恢复";
    }
  }
  return null;
}
