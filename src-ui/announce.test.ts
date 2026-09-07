import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { snapshotAnnouncement, toastDismissMs } from "./store";

const html = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "index.html"),
  "utf8",
);

const toolbar = html.slice(
  html.indexOf('class="app-toolbar"'),
  html.indexOf('id="page-overview"'),
);
const overview = html.slice(
  html.indexOf('id="page-overview"'),
  html.indexOf('id="page-library"'),
);
const library = html.slice(
  html.indexOf('id="page-library"'),
  html.indexOf('id="page-settings"'),
);
const settings = html.slice(
  html.indexOf('id="page-settings"'),
  html.indexOf("auth-dialog"),
);

describe("全局忙碌指示（issue #83）", () => {
  it("壳层工具条上有可见忙碌指示，不困在总览页", () => {
    expect(toolbar).toContain('id="app-busy"');
    expect(toolbar).toContain('id="app-busy-label"');
    expect(toolbar).toMatch(/id="app-busy"[^>]*aria-live="polite"/);
    expect(overview).not.toContain('id="app-busy"');
    expect(overview).not.toContain("quota-monitor-label");
    expect(library).not.toContain('id="app-busy"');
    expect(settings).not.toContain('id="app-busy"');
  });

  it("主壳带 aria-busy，供三个页面感知异步占用", () => {
    expect(html).toMatch(/id="app-shell"[^>]*aria-busy="false"/);
    expect(html).toContain('id="page-overview"');
    expect(html).toContain('id="page-library"');
    expect(html).toContain('id="page-settings"');
  });
});

describe("异步完成与错误公告路径（issue #83）", () => {
  it("完成走 role=status，错误走 role=alert", () => {
    expect(html).toMatch(/id="toast-status"[^>]*role="status"/);
    expect(html).toMatch(/id="toast-status"[^>]*aria-live="polite"/);
    expect(html).toMatch(/id="toast-alert"[^>]*role="alert"/);
    expect(html).toMatch(/id="toast-alert"[^>]*aria-live="assertive"/);
  });

  it("错误 toast 带关闭按钮", () => {
    const alertStart = html.indexOf('id="toast-alert"');
    const alertBlock = html.slice(alertStart, html.indexOf("</div>", html.indexOf('id="toast-alert-dismiss"')));
    expect(alertBlock).toContain('id="toast-alert-dismiss"');
    expect(alertBlock).toContain('id="toast-alert-text"');
    expect(alertBlock).toMatch(/id="toast-alert-dismiss"[^>]*type="button"/);
    expect(alertBlock).toContain("关闭");
  });

  it("额度数字不进 live region", () => {
    expect(html).not.toMatch(/id="metric-urgent"[^>]*aria-live/);
    expect(html).not.toMatch(/id="metric-total"[^>]*aria-live/);
    expect(html).not.toMatch(/id="metric-current"[^>]*aria-live/);
  });
});

describe("snapshotAnnouncement", () => {
  it("用户刷新且无快照句时公告额度已刷新，不是裸数字", () => {
    expect(snapshotAnnouncement(null, "refresh")).toEqual({
      text: "额度已刷新",
      isError: false,
    });
    expect(snapshotAnnouncement(null, "refresh")?.text).not.toMatch(/^\d+$/);
  });

  it("沿用快照里的完成句与错误句，含自动切换", () => {
    expect(
      snapshotAnnouncement(
        { text: "已自动切换到 工作机", tone: "ok", kind: "auto_swap_success" },
        "other",
      ),
    ).toEqual({ text: "已自动切换到 工作机", isError: false });
    expect(
      snapshotAnnouncement(
        { text: "自动切换失败", tone: "err", kind: "auto_swap_failed" },
        "other",
      ),
    ).toEqual({ text: "自动切换失败", isError: true });
    expect(
      snapshotAnnouncement(
        { text: "刚刚更新过", tone: "info", kind: "generic" },
        "refresh",
      ),
    ).toEqual({ text: "刚刚更新过", isError: false });
  });

  it("未知 tone 当错误公告，不当 ok/info 吞掉", () => {
    expect(
      snapshotAnnouncement(
        { text: "后端新 tone", tone: "warn", kind: "generic" },
        "other",
      ),
    ).toEqual({ text: "后端新 tone", isError: true });
  });

  it("非刷新路径无快照句则不编造公告", () => {
    expect(snapshotAnnouncement(null, "other")).toBeNull();
  });
});

describe("toastDismissMs", () => {
  it("完成 toast 数秒后收起", () => {
    expect(toastDismissMs("额度已刷新", false)).toBe(3200);
  });

  it("错误 toast 至少 20 秒，长文本更久，可被关闭按钮打断", () => {
    expect(toastDismissMs("短错误", true)).toBe(20_000);
    expect(toastDismissMs("字".repeat(80), true)).toBe(32_000);
    expect(toastDismissMs("字".repeat(80), true)).toBeGreaterThan(
      toastDismissMs("短错误", true),
    );
  });
});
