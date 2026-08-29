import { escapeHtml } from "./escape";

export type Tone = "ok" | "warn" | "high" | "full";

export interface QuotaView {
  window: string;
  ratio: number | null;
  text: string;
  reset: string | null;
  tone: Tone;
}

function quotaPercent(ratio: number): number {
  return Math.max(0, Math.min(100, ratio * 100));
}

/** 总览当前账号与账号库卡片共用的额度行 HTML。无 DOM 副作用。 */
export function quotaHtml(
  quotas: QuotaView[],
  error: string | null,
  cachedAt: string | null,
): string {
  let body: string;
  if (error) {
    body = `<p class="quota-error">额度查询失败: ${escapeHtml(error)}</p>`;
  } else if (quotas.length === 0) {
    body = `<p class="quota-reset">额度: n/a</p>`;
  } else {
    body = quotas
      .map((q) => {
        const width = quotaPercent(q.ratio ?? 0);
        const reset = q.reset ? `<span class="quota-reset">${escapeHtml(q.reset)}</span>` : "";
        const now =
          q.ratio === null ? "" : ` aria-valuenow="${Math.round(quotaPercent(q.ratio))}"`;
        return `<div class="quota-row">
        <span class="quota-label">${escapeHtml(q.window)}</span>
        <div class="quota-track" role="progressbar" aria-label="${escapeHtml(q.window)}" aria-valuemin="0" aria-valuemax="100"${now} aria-valuetext="${escapeHtml(q.text)}" title="${escapeHtml(q.text)}">
          <div class="quota-fill quota-fill--${escapeHtml(q.tone)}" style="width:${width}%"></div>
          <span class="quota-value">${escapeHtml(q.text)}</span>
        </div>
        ${reset}
      </div>`;
      })
      .join("");
  }
  if (!cachedAt) {
    return body;
  }
  return `${body}<p class="quota-cached">${escapeHtml(cachedAt)}</p>`;
}
