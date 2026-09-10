/// <reference types="vite/client" />

/**
 * 应用版本展示（issue #132）。
 * 版本号在打包时由 vite.config.ts 从 package.json 注入
 * （define 替换 `import.meta.env.VITE_APP_VERSION`），运行时不读取任何 Tauri API。
 */

/** 未注入或注入非法时的回退版本，保证展示层不出现空串 / undefined。 */
export const UNKNOWN_APP_VERSION = "0.0.0";

/** 只接受「数字点分版本号 + 可选预发布 / 构建后缀」，其余一律回退。 */
const VERSION_PATTERN = /^\d+(?:\.\d+)*(?:[-+][0-9A-Za-z.-]+)?$/;

/** 规整注入值：非法输入回退 UNKNOWN_APP_VERSION，合法则返回去空白后的原值。 */
export function normalizeAppVersion(raw: unknown): string {
  if (typeof raw !== "string") {
    return UNKNOWN_APP_VERSION;
  }
  const trimmed = raw.trim();
  return VERSION_PATTERN.test(trimmed) ? trimmed : UNKNOWN_APP_VERSION;
}

/** 状态栏右侧文案：「Kimi 换号器 v1.4.17」。 */
export function appVersionLabel(raw: unknown): string {
  return `Kimi 换号器 v${normalizeAppVersion(raw)}`;
}

/** 构建时注入值；vitest / 未注入环境下为 undefined，由兜底逻辑接管。 */
export const injectedAppVersion: unknown = import.meta.env.VITE_APP_VERSION;
