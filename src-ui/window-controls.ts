import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

function requiredButton(id: string): HTMLButtonElement {
  const element = document.getElementById(id);
  if (!(element instanceof HTMLButtonElement)) {
    throw new Error(`缺少窗口按钮: ${id}`);
  }
  return element;
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
      maximizeButton.classList.toggle("is-maximized", maximized);
    } catch {
      maximizeButton.classList.remove("is-maximized");
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
    if (!(target instanceof HTMLElement) || target.closest(".window-controls")) {
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
