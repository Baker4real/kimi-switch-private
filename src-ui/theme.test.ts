import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  parseMirroredTheme,
  persistThemeMirror,
  resolveTheme,
  THEME_IDS,
  THEME_STORAGE_KEY,
  themeApplication,
} from "./theme";

describe("themeApplication", () => {
  it("jade-night 写入根节点并按下深色钮", () => {
    expect(themeApplication("jade-night")).toEqual({
      dataTheme: "jade-night",
      classic: { active: false, pressed: false },
      jadeNight: { active: true, pressed: true },
    });
  });

  it("其它取值都落到 classic", () => {
    expect(resolveTheme("classic")).toBe("classic");
    expect(resolveTheme("unknown")).toBe("classic");
    expect(themeApplication("classic").dataTheme).toBe("classic");
    expect(themeApplication("classic").classic.pressed).toBe(true);
  });
});

describe("主题镜像", () => {
  it("只接受 classic / jade-night", () => {
    expect(parseMirroredTheme("classic")).toBe("classic");
    expect(parseMirroredTheme("jade-night")).toBe("jade-night");
    expect(parseMirroredTheme("")).toBeNull();
    expect(parseMirroredTheme("dark")).toBeNull();
    expect(parseMirroredTheme(null)).toBeNull();
  });

  it("快照主题覆盖镜像并回写", () => {
    const written: Record<string, string> = {};
    persistThemeMirror("jade-night", {
      setItem(key, value) {
        written[key] = value;
      },
    });
    expect(written[THEME_STORAGE_KEY]).toBe("jade-night");
    persistThemeMirror("classic", {
      setItem(key, value) {
        written[key] = value;
      },
    });
    expect(written[THEME_STORAGE_KEY]).toBe("classic");
  });

  it("存储不可用时不抛错", () => {
    expect(() => persistThemeMirror("classic", null)).not.toThrow();
    expect(() =>
      persistThemeMirror("classic", {
        setItem() {
          throw new Error("quota");
        },
      }),
    ).not.toThrow();
  });
});

describe("index.html 首帧脚本", () => {
  const html = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "index.html"), "utf8");
  const head = html.slice(html.indexOf("<head>"), html.indexOf("</head>"));

  it("在 head 里读镜像预设 data-theme", () => {
    expect(head).toContain(`localStorage.getItem("${THEME_STORAGE_KEY}")`);
    expect(head).toMatch(/data-theme/);
    expect(head.indexOf("<script>")).toBeGreaterThan(-1);
    expect(head.indexOf("<script>")).toBeLessThan(head.indexOf("</script>"));
  });

  it("内联脚本不含网络请求与凭证读写", () => {
    const start = head.indexOf("<script>");
    const end = head.indexOf("</script>");
    const script = head.slice(start, end);
    expect(script).not.toMatch(/fetch\s*\(/);
    expect(script).not.toMatch(/XMLHttpRequest/);
    expect(script).not.toMatch(/WebSocket/);
    expect(script).not.toMatch(/invoke/);
    expect(script).not.toMatch(/token/i);
    expect(script).not.toMatch(/kimi-code/i);
  });

  it("主题值白名单与 THEME_IDS 同步", () => {
    const start = head.indexOf("<script>");
    const end = head.indexOf("</script>");
    const script = head.slice(start, end);
    const htmlIds = [...script.matchAll(/t === ["']([^"']+)["']/g)].map((m) => m[1]);
    expect([...htmlIds].sort()).toEqual([...THEME_IDS].sort());
    for (const id of THEME_IDS) {
      expect(parseMirroredTheme(id)).toBe(id);
    }
  });
});

function clickHandlerSource(source: string, buttonId: string): string {
  const needle = `required<HTMLButtonElement>("${buttonId}").addEventListener("click"`;
  const start = source.indexOf(needle);
  expect(start, `${buttonId} 应有 click 监听`).toBeGreaterThan(-1);
  const after = source.slice(start);
  const nextRequired = after.indexOf("required<", needle.length);
  return nextRequired === -1 ? after : after.slice(0, nextRequired);
}

describe("主题钮失败 toast", () => {
  const main = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "main.ts"), "utf8");

  it("两颗主题钮的 click 处理走 runSettingsAction", () => {
    for (const id of ["theme-classic", "theme-jade-night"] as const) {
      const handler = clickHandlerSource(main, id);
      expect(handler, id).toContain("runSettingsAction");
      expect(handler, id).toContain("api.setTheme");
    }
  });
});
