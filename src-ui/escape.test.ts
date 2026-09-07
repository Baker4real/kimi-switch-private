import { describe, expect, it } from "vitest";

import { escapeHtml } from "./escape";

describe("escapeHtml", () => {
  it("转义 & < > 与双引号", () => {
    expect(escapeHtml(`a&b <c> "d"`)).toBe("a&amp;b &lt;c&gt; &quot;d&quot;");
  });

  it("转义单引号", () => {
    expect(escapeHtml(`it's "ok"`)).toBe("it&#39;s &quot;ok&quot;");
  });

  it("把标签写成安全文本", () => {
    expect(escapeHtml(`<img src=x onerror=alert(1)>`)).toBe(
      "&lt;img src=x onerror=alert(1)&gt;",
    );
  });
});
