export type ThemeId = "classic" | "jade-night";

/** 须与 `index.html` 首帧内联脚本中的 key 保持一致。 */
export const THEME_STORAGE_KEY = "kimi-switch.theme";

export function resolveTheme(theme: string): ThemeId {
  return theme === "jade-night" ? "jade-night" : "classic";
}

export function parseMirroredTheme(raw: string | null | undefined): ThemeId | null {
  return raw === "classic" || raw === "jade-night" ? raw : null;
}

export interface ThemeButtonState {
  active: boolean;
  pressed: boolean;
}

export interface ThemeApplication {
  dataTheme: ThemeId;
  classic: ThemeButtonState;
  jadeNight: ThemeButtonState;
}

/** 主题应用决策：根节点 data-theme 与主题钮按下态。不碰 DOM。 */
export function themeApplication(theme: string): ThemeApplication {
  const dataTheme = resolveTheme(theme);
  const classicOn = dataTheme === "classic";
  return {
    dataTheme,
    classic: { active: classicOn, pressed: classicOn },
    jadeNight: { active: !classicOn, pressed: !classicOn },
  };
}

function defaultStorage(): Pick<Storage, "setItem"> | null {
  try {
    if (typeof localStorage === "undefined") {
      return null;
    }
    return localStorage;
  } catch {
    return null;
  }
}

/** 把已解析主题写入 localStorage 镜像；存储不可用时静默跳过。 */
export function persistThemeMirror(
  theme: string,
  storage?: Pick<Storage, "setItem"> | null,
): void {
  const resolved = resolveTheme(theme);
  try {
    const store = storage === undefined ? defaultStorage() : storage;
    store?.setItem(THEME_STORAGE_KEY, resolved);
  } catch {
    // 隐私模式或禁用存储时跳过镜像，权威主题仍在 Rust 侧
  }
}
