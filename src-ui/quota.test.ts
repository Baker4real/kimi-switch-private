import { describe, expect, it } from "vitest";

import { quotaHtml, splitCachedAt, type QuotaView } from "./quota";

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

describe("quotaHtml hero 变体（issue #133 总览当前账号卡）", () => {
  it("参考图排布：右列竖排「用量值 / 重置时间」，数值不再压在轨道里", () => {
    const html = quotaHtml([quota()], null, null, false, "hero");
    expect(html).toContain(`class="quota-row quota-row--hero"`);
    // 右列同时承载用量值与重置时间（参考图 6% (6/100) 与 09-17 09:25 同列）。
    expect(html).toContain(`<div class="quota-row-tail">`);
    const tail = html.slice(html.indexOf(`class="quota-row-tail"`));
    expect(tail).toContain(`<span class="quota-value">42 / 100</span>`);
    expect(tail).toContain(`<span class="quota-reset">4h 后重置</span>`);
    // 轨道内只剩下填充条，不再有绝对定位的条上徽章。
    const track = html.slice(html.indexOf(`class="quota-track"`), html.indexOf(`</div>`, html.indexOf(`class="quota-track"`)));
    expect(track).not.toContain("quota-value");
  });

  it("行首是内联 SVG 图标，无图标库依赖", () => {
    const html = quotaHtml([quota({ window: "5小时" })], null, null, false, "hero");
    expect(html).toContain(`class="quota-row-icon"`);
    expect(html).toContain("<svg");
    expect(html).toContain('aria-hidden="true"');
    // 小时窗口用时钟（圆 + 指针），天窗口用日历（矩形），未知窗口回退仪表。
    expect(quotaHtml([quota({ window: "5小时" })], null, null, false, "hero")).toContain("<circle");
    expect(quotaHtml([quota({ window: "7天" })], null, null, false, "hero")).toContain("<rect");
    expect(quotaHtml([quota({ window: "其他" })], null, null, false, "hero")).toContain("<path");
  });

  it("进度条语义与重置时间沿用 card 变体", () => {
    const hero = quotaHtml([quota()], null, null, false, "hero");
    const card = quotaHtml([quota()], null, null, false);
    for (const attrs of [
      `role="progressbar"`,
      `aria-label="5 小时"`,
      `aria-valuenow="42"`,
      `aria-valuetext="42 / 100"`,
      `quota-fill--ok`,
      `style="width:42%"`,
    ]) {
      expect(hero, attrs).toContain(attrs);
      expect(card, attrs).toContain(attrs);
    }
    expect(hero).toContain(`<span class="quota-reset">4h 后重置</span>`);
  });

  it("hero 的「更新于」时间行带循环箭头图标，并按参考图加斜杠分隔", () => {
    const html = quotaHtml([quota()], null, "更新于 12:00", false, "hero");
    expect(html).toContain(`<p class="quota-cached quota-cached--hero">`);
    expect(html).toContain(`class="quota-row-icon"`);
    // 参考图「更新于 / 12:00」：标签与时间拆成两段，中间斜杠仅作装饰。
    expect(html).toContain(`<span class="quota-cached-label">更新于</span>`);
    expect(html).toContain(`<span class="quota-cached-sep" aria-hidden="true">/</span>`);
    expect(html).toContain(`<span class="quota-cached-time">12:00</span>`);
    // 非 hero 变体不加图标、不加分隔符，原句原样。
    expect(quotaHtml([quota()], null, "更新于 12:00", false)).toContain(
      `<p class="quota-cached">更新于 12:00</p>`,
    );
  });

  it("cached_at 拆分：无空格时整句归标签、不补分隔符", () => {
    expect(splitCachedAt("更新于 09-10 11:35")).toEqual(["更新于", "09-10 11:35"]);
    expect(splitCachedAt("缓存于 09-10 11:35")).toEqual(["缓存于", "09-10 11:35"]);
    expect(splitCachedAt("更新于")).toEqual(["更新于", null]);
    const html = quotaHtml([quota()], null, "更新于", false, "hero");
    expect(html).not.toContain("quota-cached-sep");
    expect(html).toContain(`<span class="quota-cached-label">更新于</span>`);
  });

  it("空态 / 失败 / 凭证待续期文案两个变体保持一致", () => {
    for (const variant of ["card", "hero"] as const) {
      expect(quotaHtml([], null, null, false, variant)).toBe(
        `<p class="quota-reset">额度：n/a</p>`,
      );
      expect(quotaHtml([], "超时", null, false, variant)).toBe(
        `<p class="quota-error">额度查询失败：超时</p>`,
      );
      expect(quotaHtml([], "过期", null, true, variant)).toBe(
        `<p class="quota-expired">凭证待续期 · 过期</p>`,
      );
    }
  });

  it("hero 变体沿用转义", () => {
    const html = quotaHtml(
      [quota({ window: `5"h`, text: `a<b>`, reset: `c&d` })],
      null,
      null,
      false,
      "hero",
    );
    expect(html).toContain(`aria-label="5&quot;h"`);
    expect(html).toContain(`>a&lt;b&gt;</span>`);
    expect(html).toContain(`c&amp;d`);
    expect(html).not.toContain(`5"h`);
    expect(html).not.toContain(`a<b>`);
  });
});
