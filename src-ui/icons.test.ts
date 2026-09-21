import { describe, expect, it } from "vitest";

import { quotaHtml, type QuotaView } from "./quota";
import { REFRESH_ICON, STATUS_RENEWAL_ICON } from "./icons";

/** 本文件描边箭头是「竖线紧接横线」的 L 形拐角。 */
function strokeArrowHeads(svg: string): string[] {
  return svg.match(/[Vv]-?[\d.]+h-?[\d.]+/g) ?? [];
}

const quota: QuotaView = {
  window: "5 小时",
  ratio: 0.42,
  text: "42 / 100",
  reset: "4h 后重置",
  tone: "ok",
};

describe("REFRESH_ICON", () => {
  it("「更新于」行是单循环箭头，两弧不会对向重叠", () => {
    // 用户截图错乱来源：双向刷新（两个 L 形箭头 + 对向圆弧）在 22px / 2.1 描边上叠成一团。
    // 参考图与 STATUS_RENEWAL_ICON 同为顺时针单循环箭头。
    expect(strokeArrowHeads(REFRESH_ICON)).toHaveLength(1);
    expect(REFRESH_ICON).not.toMatch(/M3\.8 12a8\.2 8\.2 0 0 1 14-5\.8/);
    expect(REFRESH_ICON).not.toMatch(/M3\.8 19\.6v-5h5/);
    expect(strokeArrowHeads(STATUS_RENEWAL_ICON)).toHaveLength(1);
  });

  it("hero「更新于」行用的就是这一枚单循环箭头", () => {
    const html = quotaHtml([quota], null, "更新于 09-21 16:36", false, "hero");
    expect(html).toContain(REFRESH_ICON);
  });
});
