import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

function requiredButton(id: string): HTMLButtonElement {
  const element = document.getElementById(id);
  if (!(element instanceof HTMLButtonElement)) {
    throw new Error(`缺少窗口按钮: ${id}`);
  }
  return element;
}

/** 最大化按钮读屏名称：普通态「最大化」，已最大化「还原」。 */
export function maximizeButtonLabel(maximized: boolean): string {
  return maximized ? "还原" : "最大化";
}

/** 同步最大化按钮的视觉 class 与 aria-label。 */
export function applyMaximizePresentation(
  button: {
    classList: { toggle(token: string, force?: boolean): void; remove(token: string): void };
    setAttribute(name: string, value: string): void;
  },
  maximized: boolean,
): void {
  button.classList.toggle("is-maximized", maximized);
  button.setAttribute("aria-label", maximizeButtonLabel(maximized));
}

export async function setupWindowControls(): Promise<void> {
  const minimizeButton = requiredButton("window-minimize-button");
  const maximizeButton = requiredButton("window-maximize-button");
  const closeButton = requiredButton("window-close-button");
  const titlebarSurface = document.getElementById("window-titlebar-drag-surface");

  if (!isTauri()) {
    for (const button of [minimizeButton, maximizeButton, closeButton]) {
      button.disabled = true;
    }
    return;
  }

  const appWindow = getCurrentWindow();

  const syncMaximizeState = async (): Promise<void> => {
    try {
      const maximized = await appWindow.isMaximized();
      applyMaximizePresentation(maximizeButton, maximized);
    } catch {
      applyMaximizePresentation(maximizeButton, false);
    }
  };

  const toggleMaximize = async (): Promise<void> => {
    if (await appWindow.isMaximized()) {
      await appWindow.unmaximize();
    } else {
      await appWindow.maximize();
    }
    await syncMaximizeState();
  };

  minimizeButton.addEventListener("click", () => {
    void appWindow.minimize();
  });
  maximizeButton.addEventListener("click", () => {
    void toggleMaximize();
  });
  closeButton.addEventListener("click", () => {
    void appWindow.close();
  });

  titlebarSurface?.addEventListener("mousedown", (event) => {
    if (event.button !== 0) {
      return;
    }
    const target = event.target;
    // 徽标是内联 SVG（SVGElement 不是 HTMLElement）：用 Element 判定，
    // 否则点在方块或 K 字形上无法拖动 / 双击最大化。
    if (!(target instanceof Element) || target.closest(".window-controls")) {
      return;
    }
    if (event.detail === 2) {
      void toggleMaximize();
      return;
    }
    void appWindow.startDragging();
  });

  await appWindow.onResized(() => {
    void syncMaximizeState();
  });
  await syncMaximizeState();
}
