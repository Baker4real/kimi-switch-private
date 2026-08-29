import type { Snapshot } from "./api";

const IDLE_BUSY_LABEL = "额度就绪";

export interface AppState {
  snapshot: Snapshot | null;
  busy: boolean;
  busyLabel: string;
}

export function createAppStore(initial?: Partial<AppState>) {
  let state: AppState = {
    snapshot: null,
    busy: false,
    busyLabel: IDLE_BUSY_LABEL,
    ...initial,
  };

  return {
    get(): AppState {
      return state;
    },
    setBusy(busy: boolean, label?: string): void {
      state = {
        ...state,
        busy,
        busyLabel: busy ? (label ?? "处理中…") : IDLE_BUSY_LABEL,
      };
    },
    setSnapshot(snapshot: Snapshot | null): void {
      state = { ...state, snapshot };
    },
  };
}

export type AppStore = ReturnType<typeof createAppStore>;
