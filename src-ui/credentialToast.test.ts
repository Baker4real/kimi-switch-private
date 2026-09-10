import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  credentialRecoveredToast,
  type CredentialToastAccount,
} from "./credentialToast";

function row(
  partial: Partial<CredentialToastAccount> = {},
): CredentialToastAccount {
  return {
    id: "a1",
    credential_expired: false,
    ...partial,
  };
}

describe("credentialRecoveredToast", () => {
  it("待续期行在相邻快照间消失时产生且仅一次「凭证已恢复」", () => {
    const expired = [row({ credential_expired: true })];
    const recovered = [row({ credential_expired: false })];
    expect(credentialRecoveredToast(expired, recovered)).toBe("凭证已恢复");
    expect(credentialRecoveredToast(recovered, recovered)).toBeNull();
  });

  it("新增待续期行不产生该 toast", () => {
    expect(
      credentialRecoveredToast(
        [row({ credential_expired: false })],
        [row({ credential_expired: true })],
      ),
    ).toBeNull();
    expect(
      credentialRecoveredToast(
        [row({ id: "a1", credential_expired: false })],
        [
          row({ id: "a1", credential_expired: false }),
          row({ id: "a2", credential_expired: true }),
        ],
      ),
    ).toBeNull();
  });

  it("额度数字变化等其他转换不产生该 toast", () => {
    const prev = [{ ...row(), used: 0.1 }];
    const next = [{ ...row(), used: 0.8 }];
    expect(credentialRecoveredToast(prev, next)).toBeNull();
    expect(
      credentialRecoveredToast(
        [row({ credential_expired: true })],
        [row({ credential_expired: true })],
      ),
    ).toBeNull();
    expect(credentialRecoveredToast(null, [row({ credential_expired: false })])).toBeNull();
    expect(credentialRecoveredToast(null, [row({ credential_expired: true })])).toBeNull();
    expect(
      credentialRecoveredToast(
        [row({ credential_expired: true })],
        [],
      ),
    ).toBeNull();
  });

  it("多账号时只看待续期行是否消失，仍只弹一条", () => {
    const prev = [
      row({ id: "a1", credential_expired: true }),
      row({ id: "a2", credential_expired: true }),
    ];
    const oneRecovered = [
      row({ id: "a1", credential_expired: false }),
      row({ id: "a2", credential_expired: true }),
    ];
    const bothRecovered = [
      row({ id: "a1", credential_expired: false }),
      row({ id: "a2", credential_expired: false }),
    ];
    expect(credentialRecoveredToast(prev, oneRecovered)).toBe("凭证已恢复");
    expect(credentialRecoveredToast(prev, bothRecovered)).toBe("凭证已恢复");
  });
});

describe("applySnapshot 接线（issue #105）", () => {
  it("应用快照时用上一份 vs 这一份账号行调用凭证恢复 toast", () => {
    const main = readFileSync(
      join(dirname(fileURLToPath(import.meta.url)), "main.ts"),
      "utf8",
    );
    const start = main.indexOf("function applySnapshot");
    const end = main.indexOf("function stopSilentQuotaQueries");
    expect(start).toBeGreaterThanOrEqual(0);
    expect(end).toBeGreaterThan(start);
    const applyFn = main.slice(start, end);
    expect(main).toContain('from "./credentialToast"');
    expect(applyFn).toContain("credentialRecoveredToast(");
    expect(applyFn.indexOf("snapshot?.accounts")).toBeGreaterThanOrEqual(0);
    expect(applyFn.indexOf("snapshot?.accounts")).toBeLessThan(
      applyFn.indexOf("paintSnapshot(next)"),
    );
  });
});
