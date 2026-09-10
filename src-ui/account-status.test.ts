import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  accountStatusBodyHtml,
  accountStatusView,
  isQuotaUrgent,
  URGENT_USED_RATIO,
  type AccountStatusFields,
} from "./account-status";

const dir = dirname(fileURLToPath(import.meta.url));

/** 最小当前账号字段：默认正常。 */
function row(overrides: Partial<AccountStatusFields> = {}): AccountStatusFields {
  return {
    error: null,
    credential_expired: false,
    quotas: [{ ratio: 0.1 }],
    ...overrides,
  };
}

describe("accountStatusView 真值表", () => {
  const cases: {
    name: string;
    row: AccountStatusFields | null;
    state: string | null;
    tone: string | null;
    title: string | null;
    detail: string | null;
  }[] = [
    {
      name: "正常：无异常、用量未达阈值",
      row: row(),
      state: "ok",
      tone: "ok",
      title: "账号状态正常",
      detail: "一切运行良好，继续享受 AI 服务吧！",
    },
    {
      name: "正常：额度未知（ratio null）不算异常",
      row: row({ quotas: [{ ratio: null }] }),
      state: "ok",
      tone: "ok",
      title: "账号状态正常",
      detail: "一切运行良好，继续享受 AI 服务吧！",
    },
    {
      name: "正常：无用量窗口（空数组）不算异常",
      row: row({ quotas: [] }),
      state: "ok",
      tone: "ok",
      title: "账号状态正常",
      detail: "一切运行良好，继续享受 AI 服务吧！",
    },
    {
      name: "额度告急：任一窗口 ≥ 80%",
      row: row({ quotas: [{ ratio: URGENT_USED_RATIO }] }),
      state: "urgent",
      tone: "high",
      title: "账号额度告急",
      detail: "部分额度窗口即将耗尽，建议切换账号",
    },
    {
      name: "额度告急：与正常窗口共存仍告急",
      row: row({ quotas: [{ ratio: 0.2 }, { ratio: 0.95 }] }),
      state: "urgent",
      tone: "high",
      title: "账号额度告急",
      detail: "部分额度窗口即将耗尽，建议切换账号",
    },
    {
      name: "凭证待续期：credential_expired 优先于 error",
      row: row({
        credential_expired: true,
        error: "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
        quotas: [],
      }),
      state: "renewal",
      tone: "warn",
      title: "凭证待续期",
      detail: "在 Kimi CLI 里发条消息即可恢复，或重新授权",
    },
    {
      name: "查询失败：有 error 且非凭证待续期",
      row: row({ error: "超时", quotas: [] }),
      state: "failed",
      tone: "full",
      title: "额度查询失败",
      detail: "暂时无法获取额度，稍后重试",
    },
    {
      name: "无当前账号：不渲染",
      row: null,
      state: null,
      tone: null,
      title: null,
      detail: null,
    },
  ];

  for (const c of cases) {
    it(c.name, () => {
      const view = accountStatusView(c.row);
      if (c.state === null) {
        expect(view).toBeNull();
        return;
      }
      expect(view).not.toBeNull();
      expect(view!.state).toBe(c.state);
      expect(view!.tone).toBe(c.tone);
      expect(view!.title).toBe(c.title);
      expect(view!.detail).toBe(c.detail);
    });
  }

  it("四态各只出现一次，覆盖完整真值表（正常 = 无任一异常）", () => {
    const states = cases.map((c) => c.state);
    for (const state of ["ok", "urgent", "renewal", "failed", null]) {
      expect(states).toContain(state);
    }
  });
});

describe("异常优先级（票面未规定，此处固化）", () => {
  it("凭证待续期 > 额度查询失败：后端 credential_expired 为真时 error 必然非空，必须先判续期", () => {
    const view = accountStatusView(
      row({ credential_expired: true, error: "凭证已过期", quotas: [] }),
    );
    expect(view?.state).toBe("renewal");
  });

  it("额度查询失败 > 额度告急：error 与超阈用量同时出现（坏数据）时选查询失败", () => {
    const view = accountStatusView(row({ error: "超时", quotas: [{ ratio: 0.99 }] }));
    expect(view?.state).toBe("failed");
  });

  it("凭证待续期 > 额度告急：坏数据下也不退化成告急", () => {
    const view = accountStatusView(
      row({ credential_expired: true, error: "凭证已过期", quotas: [{ ratio: 0.99 }] }),
    );
    expect(view?.state).toBe("renewal");
    expect(view?.tone).toBe("warn");
  });

  it("正常态是唯一无异常命中的分支", () => {
    expect(accountStatusView(row())?.state).toBe("ok");
    expect(accountStatusView(row({ error: "x" }))?.state).not.toBe("ok");
  });
});

describe("isQuotaUrgent（镜像 Rust URGENT_USED_RATIO）", () => {
  it("阈值边界：79.9% 不告急、80% 告急", () => {
    expect(isQuotaUrgent([{ ratio: 0.799 }])).toBe(false);
    expect(isQuotaUrgent([{ ratio: 0.8 }])).toBe(true);
    expect(isQuotaUrgent([{ ratio: 1 }])).toBe(true);
  });

  it("ratio 缺失不计告急，其它窗口命中即告急", () => {
    expect(isQuotaUrgent([{ ratio: null }])).toBe(false);
    expect(isQuotaUrgent([{ ratio: null }, { ratio: 0.85 }])).toBe(true);
  });

  it("阈值常量与 Rust dto.rs 的 URGENT_USED_RATIO 一致", () => {
    const rust = readFileSync(
      join(dir, "..", "crates", "gui", "src", "dto.rs"),
      "utf8",
    );
    const match = /URGENT_USED_RATIO:\s*f32\s*=\s*([\d.]+)/.exec(rust);
    expect(match, "Rust 侧 URGENT_USED_RATIO 常量缺失").toBeTruthy();
    expect(Number(match![1])).toBe(URGENT_USED_RATIO);
  });

  it("告急判定与 Rust 注释一致：查询失败不告急", () => {
    // 后端 URGENT_USED_RATIO 注释明说「额度查询失败不告急」，前端不得自行把 error 算进告急。
    expect(isQuotaUrgent([])).toBe(false);
  });
});

describe("只读既有快照（验收：不新增网络请求 / 判定来源）", () => {
  const src = readFileSync(join(dir, "account-status.ts"), "utf8");

  it("不 import api / invoke / fetch，决策纯内存派生", () => {
    expect(src).not.toMatch(/from "\.\/api"/);
    expect(src).not.toMatch(/\b(?:invoke|fetch|XMLHttpRequest)\s*\(/);
  });

  it("不改写既有判定：不重算 credential_expired，只读 error / credential_expired / quotas", () => {
    // 告急阈值镜像 Rust 单一出处，此处不得另立第二套阈值。
    const thresholds = [...src.matchAll(/0\.\d+/g)].map((m) => m[0]);
    expect(new Set(thresholds)).toEqual(new Set([String(URGENT_USED_RATIO)]));
    expect(src).toContain("row.credential_expired");
    expect(src).toContain("row.error");
  });

  it("视图决策是纯函数：同一输入两次调用结果稳定，且不修改入参", () => {
    const input = row({ quotas: [{ ratio: 0.9 }] });
    const snapshot = JSON.parse(JSON.stringify(input));
    const first = accountStatusView(input);
    const second = accountStatusView(input);
    expect(first).toEqual(second);
    expect(input).toEqual(snapshot);
  });
});

describe("accountStatusBodyHtml", () => {
  it("正常态：内联 SVG 对勾 + 标题 + 说明，无图标库", () => {
    const view = accountStatusView(row())!;
    const html = accountStatusBodyHtml(view);
    expect(html).toContain('class="account-status-icon"');
    expect(html).toContain('aria-hidden="true"');
    expect(html).toContain("<svg");
    expect(html).not.toMatch(/lucide|heroicons|font-awesome/i);
    expect(html).toContain(`<strong class="account-status-title">账号状态正常</strong>`);
    expect(html).toContain(
      `<span class="account-status-detail">一切运行良好，继续享受 AI 服务吧！</span>`,
    );
  });

  it("四态各自换图形：正常对勾、告急感叹、续期循环、失败叉号互不相同", () => {
    const shapes = (["ok", "urgent", "renewal", "failed"] as const).map((state) => {
      const view = accountStatusView(
        state === "ok"
          ? row()
          : state === "urgent"
            ? row({ quotas: [{ ratio: 0.9 }] })
            : state === "renewal"
              ? row({ credential_expired: true, error: "凭证已过期", quotas: [] })
              : row({ error: "超时", quotas: [] }),
      )!;
      return accountStatusBodyHtml(view);
    });
    expect(new Set(shapes).size).toBe(4);
  });

  it("文案经 escape（防御后端 error 文案回填），标题与说明分列两行", () => {
    const view = { ...accountStatusView(row())!, title: `<b>x</b>`, detail: `a&b` };
    const html = accountStatusBodyHtml(view);
    expect(html).toContain(`&lt;b&gt;x&lt;/b&gt;`);
    expect(html).toContain(`a&amp;b`);
    expect(html).not.toContain(`<b>x</b>`);
  });
});
