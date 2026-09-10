import { describe, expect, it } from "vitest";

import { quotaHtml, type QuotaView } from "./quota";

function quota(partial: Partial<QuotaView> = {}): QuotaView {
  return {
    window: "5 小时",
    ratio: 0.42,
    text: "42 / 100",
    reset: "4h 后重置",
    tone: "ok",
    ...partial,
  };
}

describe("quotaHtml", () => {
  it("查询失败时保持原文案且不含进度条", () => {
    const html = quotaHtml([], `失败 <x>`, null, false);
    expect(html).toBe(`<p class="quota-error">额度查询失败：失败 &lt;x&gt;</p>`);
    expect(html).not.toContain("progressbar");
  });

  it("无额度窗口时输出 n/a", () => {
    expect(quotaHtml([], null, null, false)).toBe(`<p class="quota-reset">额度：n/a</p>`);
  });

  it("额度条带 progressbar 语义与窗口名、用量文字", () => {
    const html = quotaHtml([quota()], null, null, false);
    expect(html).toContain(`role="progressbar"`);
    expect(html).toContain(`aria-label="5 小时"`);
    expect(html).toContain(`aria-valuemin="0"`);
    expect(html).toContain(`aria-valuemax="100"`);
    expect(html).toContain(`aria-valuenow="42"`);
    expect(html).toContain(`aria-valuetext="42 / 100"`);
    expect(html).toContain(`quota-fill--ok`);
    expect(html).toContain(`style="width:42%"`);
    expect(html).toContain(`<span class="quota-reset">4h 后重置</span>`);
  });

  it("ratio 为空时不输出 aria-valuenow，条宽按 0", () => {
    const html = quotaHtml([quota({ ratio: null, text: "n/a", reset: null })], null, null, false);
    expect(html).not.toContain("aria-valuenow");
    expect(html).toContain(`aria-valuetext="n/a"`);
    expect(html).toContain(`style="width:0%"`);
  });

  it("四档压力 class 与 DTO tone 一致", () => {
    for (const tone of ["ok", "warn", "high", "full"] as const) {
      expect(quotaHtml([quota({ tone })], null, null, false)).toContain(`quota-fill--${tone}`);
    }
  });

  it("转义窗口名、用量文字与重置时间", () => {
    const html = quotaHtml(
      [quota({ window: `5"h`, text: `a<b>`, reset: `c&d` })],
      null,
      null,
      false,
    );
    expect(html).toContain(`aria-label="5&quot;h"`);
    expect(html).toContain(`aria-valuetext="a&lt;b&gt;"`);
    expect(html).toContain(`c&amp;d`);
    expect(html).not.toContain(`5"h`);
    expect(html).not.toContain(`a<b>`);
  });

  it("有 cached_at 时追加灰字时间", () => {
    const cached = quotaHtml([], "超时", "缓存于 12:00", false);
    expect(cached).toContain(`额度查询失败：超时`);
    expect(cached).toContain(`<p class="quota-cached">缓存于 12:00</p>`);

    const updated = quotaHtml([quota()], null, "更新于 12:00", false);
    expect(updated).toContain(`<p class="quota-cached">更新于 12:00</p>`);
    expect(updated).toContain(`role="progressbar"`);
  });

  it("凭证待续期用独立状态行而非错误红", () => {
    const html = quotaHtml(
      [],
      "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
      null,
      true,
    );
    expect(html).toBe(
      `<p class="quota-expired">凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权</p>`,
    );
    expect(html).not.toContain("凭证待续期 ·");
    expect(html).not.toContain("quota-error");
    expect(html).not.toContain("额度查询失败");
    expect(html).not.toContain("progressbar");
  });

  it("凭证待续期文案同样转义", () => {
    const html = quotaHtml([], `过期 <x>`, null, true);
    expect(html).toContain(`凭证待续期 · 过期 &lt;x&gt;`);
    expect(html).not.toContain(`<x>`);
  });

  it("credential_expired 为 false 时仍是查询失败样式", () => {
    const html = quotaHtml([], "凭证已过期", null, false);
    expect(html).toContain(`quota-error`);
    expect(html).toContain(`额度查询失败：凭证已过期`);
  });
});
