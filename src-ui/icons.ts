/** 手绘内联 SVG 图标（issue #133）：不引入任何图标库。
 *
 * 行图标统一 24×24 viewBox、`currentColor` 描边，颜色由使用处的 CSS 决定；
 * 静态卡片的图标（徽标、统计卡、操作卡）直接写在 index.html 里，此处只放
 * 需要按数据动态选择的图标（用量窗口、会员轨道、更新时间）。 */
/** 额度行图标统一描边；`currentColor` 由使用处的 CSS 决定颜色。 */
const STROKE_ATTRS =
  'viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false"';

function svg(inner: string): string {
  return `<svg ${STROKE_ATTRS}>${inner}</svg>`;
}

/** 日历：7 天 / 30 天等按天计的用量窗口。 */
export const CALENDAR_ICON = svg(
  `<rect x="3.4" y="4.6" width="17.2" height="16" rx="3"/><path d="M3.4 10.2h17.2M8.4 2.6v3.6M15.6 2.6v3.6"/>`,
);

/** 时钟：5 小时等按小时计的用量窗口。 */
export const CLOCK_ICON = svg(`<circle cx="12" cy="12" r="8.6"/><path d="M12 7.2V12l3.3 2"/>`);

/** 循环箭头：额度快照「更新于」时间行；操作卡的「刷新」按钮另有同形图标。 */
export const REFRESH_ICON = svg(
  `<path d="M20.2 12a8.2 8.2 0 0 1-14 5.8"/><path d="M3.8 12a8.2 8.2 0 0 1 14-5.8"/><path d="M20.2 4.4V9.4h-5M3.8 19.6v-5h5"/>`,
);

/** 仪表盘：未知窗口的兜底图标。 */
export const GAUGE_ICON = svg(`<path d="M3.6 16.8a8.4 8.4 0 1 1 16.8 0"/><path d="M12 16.8 16 10.8"/>`);

/** 皇冠（会员轨道行首）：实心金冠，与「会员账号」徽章同一造型语言。 */
export const CROWN_ICON = `<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true" focusable="false"><path d="M2.9 6.9 7 11l5-7 5 7 4.1-4.1-2 12.1H4.9z"/><path d="M6.2 19.6h11.6v2.2H6.2z"/></svg>`;

/** 用量窗口名 → 行图标：小时窗口用时钟、天窗口用日历，未知窗口退回仪表。 */
export function quotaWindowIconHtml(windowName: string): string {
  if (windowName.includes("时")) {
    return CLOCK_ICON;
  }
  if (windowName.includes("天")) {
    return CALENDAR_ICON;
  }
  return GAUGE_ICON;
}

/** 「账号状态」汇总条图标（issue #134）：实心圆底 + 白色描边图形，与参考图绿色对勾同型。
 * 颜色全部由 CSS 决定（`.account-status-plate` 取 `currentColor`、`.account-status-glyph`
 * 取 `--text-inverse`），四个状态只换内部图形，本文件不写死任何颜色。 */
function statusBadgeIcon(glyph: string): string {
  return `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><circle class="account-status-plate" cx="12" cy="12" r="10"/>${glyph}</svg>`;
}

function statusGlyph(inner: string): string {
  return `<g class="account-status-glyph">${inner}</g>`;
}

/** 正常：对勾。 */
export const STATUS_OK_ICON = statusBadgeIcon(
  statusGlyph(`<path d="M7.8 12.3 10.6 15.1 16.2 9.3"/>`),
);

/** 额度告急：感叹号。 */
export const STATUS_URGENT_ICON = statusBadgeIcon(
  statusGlyph(`<path d="M12 7.4v5.2"/><path d="M12 16.4h.01"/>`),
);

/** 凭证待续期：顺时针循环箭头（续期 / 恢复）。 */
export const STATUS_RENEWAL_ICON = statusBadgeIcon(
  statusGlyph(`<path d="M18.1 12a6.1 6.1 0 1 1-1.79-4.31"/><path d="M16.6 4.7v3.3h-3.3"/>`),
);

/** 额度查询失败：叉号。 */
export const STATUS_FAILED_ICON = statusBadgeIcon(
  statusGlyph(`<path d="M8.8 8.8l6.4 6.4"/><path d="M15.2 8.8l-6.4 6.4"/>`),
);
