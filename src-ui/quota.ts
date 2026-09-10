import { escapeHtml } from "./escape";
import { REFRESH_ICON, quotaWindowIconHtml } from "./icons";

export type Tone = "ok" | "warn" | "high" | "full";

/** 渲染变体：`card` 为账号库卡片；`hero` 为总览当前账号大卡（issue #133 参考图）。
 * hero 把用量值移到条上方、重置时间移到条下方，并在行首加内联 SVG 图标。 */
export type QuotaVariant = "card" | "hero";

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

/** 总览当前账号卡的单条用量行（hero 变体，参考图排布）：
 * 左图标列 + 上排「窗口名」+ 下排满宽进度条；右列竖排「用量值 / 重置时间」，
 * 用量值与进度条同处一行、重置时间压在条端（参考图 09-17 09:25 的位置）。
 * 进度条语义（role/aria-*）与 card 变体完全一致。 */
function heroQuotaRowHtml(q: QuotaView): string {
  const width = quotaPercent(q.ratio ?? 0);
  const reset = q.reset ? `<span class="quota-reset">${escapeHtml(q.reset)}</span>` : "";
  const now =
    q.ratio === null ? "" : ` aria-valuenow="${Math.round(quotaPercent(q.ratio))}"`;
  return `<div class="quota-row quota-row--hero">
        <span class="quota-row-icon" aria-hidden="true">${quotaWindowIconHtml(q.window)}</span>
        <span class="quota-label">${escapeHtml(q.window)}</span>
        <div class="quota-track" role="progressbar" aria-label="${escapeHtml(q.window)}" aria-valuemin="0" aria-valuemax="100"${now} aria-valuetext="${escapeHtml(q.text)}" title="${escapeHtml(q.text)}">
          <div class="quota-fill quota-fill--${escapeHtml(q.tone)}" style="width:${width}%"></div>
        </div>
        <div class="quota-row-tail">
          <span class="quota-value">${escapeHtml(q.text)}</span>
          ${reset}
        </div>
      </div>`;
}

/** 总览当前账号与账号库卡片共用的额度行 HTML。无 DOM 副作用。 */
export function quotaHtml(
  quotas: QuotaView[],
  error: string | null,
  cachedAt: string | null,
  credentialExpired: boolean,
  variant: QuotaVariant = "card",
): string {
  let body: string;
  if (error && credentialExpired) {
    // 凭证待续期是凭证生命周期状态，不是查询故障：不按错误红渲染。
    // 后端引导若已以「凭证已过期」开头，不再叠「凭证待续期 · 」前缀。
    const text = error.startsWith("凭证已过期") ? error : `凭证待续期 · ${error}`;
    body = `<p class="quota-expired">${escapeHtml(text)}</p>`;
  } else if (error) {
    body = `<p class="quota-error">额度查询失败：${escapeHtml(error)}</p>`;
  } else if (quotas.length === 0) {
    body = `<p class="quota-reset">额度：n/a</p>`;
  } else if (variant === "hero") {
    body = quotas.map((q) => heroQuotaRowHtml(q)).join("");
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
  if (variant === "hero") {
    // 参考图「更新于 / 09-10 11:35」：标签与时间之间加斜杠分隔。
    // 后端把整句放在 cached_at（「更新于 09-10 11:35」/「缓存于 …」），
    // 前端口径不变，仍按首个空格切一次；切不开时整句当时间、不补分隔符。
    // 三段包在一个 inline 容器里，避免外层 flex 的 gap 把斜杠两侧撑得过宽。
    const [label, time] = splitCachedAt(cachedAt);
    const sep = time === null ? "" : `<span class="quota-cached-sep" aria-hidden="true">/</span>`;
    const timeHtml = time === null ? "" : `<span class="quota-cached-time">${escapeHtml(time)}</span>`;
    return `${body}<p class="quota-cached quota-cached--hero"><span class="quota-row-icon" aria-hidden="true">${REFRESH_ICON}</span><span class="quota-cached-text"><span class="quota-cached-label">${escapeHtml(label)}</span>${sep}${timeHtml}</span></p>`;
  }
  return `${body}<p class="quota-cached">${escapeHtml(cachedAt)}</p>`;
}

/** 快照 cached_at 的「前缀 + 空格 + 时间」拆成两段；无空格时整句归标签、时间留 null。 */
export function splitCachedAt(cachedAt: string): [string, string | null] {
  const idx = cachedAt.indexOf(" ");
  if (idx < 0) {
    return [cachedAt, null];
  }
  return [cachedAt.slice(0, idx), cachedAt.slice(idx + 1)];
}
