//! 配额失败被动切换（issue #162，spec #152 实现决策 14；#184 改认官方 429）核心面测试：
//! - StopFailure 事件分类纯函数：官方 429 耗尽文案与旧口径 403 配额文案命中；
//!   非耗尽 429、其他错误、用户内容字段（会话标题 / 工作目录）与不在命中文案本地的
//!   状态信号不命中；畸形静默；
//! - 权威耗尽证据合成 [`overlay_exhausted_snapshot`]；
//! - 被动门面 [`decide_auto_swap_passive_from_state`]：安顿宽限与手动保持期不阻挡、
//!   冷却照常，资格线 / 防抖边际 / 摘出等全部继承轮询路径。
//!
//! 账号夹具不落凭证文件（tempdir 内无 `credentials.json`），候选本地凭证快检
//! fail-open 为空集合，无需隔离 `KIMI_CODE_HOME`。

use chrono::{DateTime, TimeZone, Utc};
use kimi_switch_core::auto_swap::{
    classify_stop_failure, decide_auto_swap_from_state, decide_auto_swap_passive_from_state,
    overlay_exhausted_snapshot, set_manual_hold_until, write_last_auto_swap_at, AutoSwapDecision,
    AutoSwapSkipReason, StopFailureSignal,
};
use kimi_switch_core::settings::AutoSwap;
use kimi_switch_core::{
    Account, AccountId, Quota, QuotaSnapshot, QuotaState, QuotaStatus, QuotaWindow,
};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 3, 12, 0, 0).unwrap()
}

fn on() -> AutoSwap {
    AutoSwap {
        enabled: true,
        ..AutoSwap::default()
    }
}

fn acc(id: &str, active: bool, last_used_at: Option<DateTime<Utc>>) -> Account {
    Account {
        provider: "kimi".into(),
        id: AccountId(id.into()),
        label: id.into(),
        active,
        created_at: now(),
        last_used_at,
        extra: Default::default(),
    }
}

fn quota(id: &str, window: QuotaWindow, used: u64, limit: u64) -> Quota {
    Quota {
        provider: "kimi".into(),
        account_id: AccountId(id.into()),
        window,
        used,
        limit,
        reset_at: None,
        status: QuotaStatus::Ok,
        note: None,
        membership: None,
    }
}

fn ready(id: &str, quotas: Vec<Quota>) -> QuotaSnapshot {
    QuotaSnapshot {
        account_id: AccountId(id.into()),
        fetched_at: now(),
        stale: false,
        membership: None,
        state: QuotaState::Ready(quotas),
    }
}

/// 夹具工作目录（不含状态码样子的数字）。
const WORKSPACE: &str = r"D:\agents\projects\kimi-switch";

/// Kimi Code CLI 2.3.0 的 StopFailure 载荷形态：平铺对象
/// `{hook_event_name, session_id, cwd, session_title, error_type, error_message}`。
/// 形态出自 #182 评审对本机 kimi.exe 内嵌源码的阅读（withSessionFacts /
/// notifyStopFailure / titleFromPromptMetadataText），未抓过真机事件。`session_title`
/// 是首句用户提示的前 200 字，`cwd` 是会话工作目录，两者都是用户内容。
fn stop_failure_event(
    cwd: &str,
    session_title: &str,
    error_type: &str,
    error_message: &str,
) -> String {
    serde_json::json!({
        "hook_event_name": "StopFailure",
        "session_id": "0b7c2f9e-5d1a-4c3b-9e8f-1a2b3c4d5e6f",
        "cwd": cwd,
        "session_title": session_title,
        "error_type": error_type,
        "error_message": error_message,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// 分类纯函数（Seam 1）
// ---------------------------------------------------------------------------

/// 旧口径（保留）：403 + "usage limit" 文案命中；窗口可从文案辨认。
#[test]
fn classify_matches_403_usage_limit_and_extracts_window() {
    let cases = [
        (
            r#"{"hook_event_name":"StopFailure","session_id":"s1","error":{"status":403,"message":"You have reached your 5-hour usage limit"}}"#,
            Some(QuotaWindow::FiveHour),
        ),
        (
            // 2026-09-11 真机会话 WARN 原文（You've，不是 You have）。
            r#"{"hook_event_name":"StopFailure","error":{"status":403,"message":"You've reached your 5-hour usage limit. Your quota will reset when the current 5-hour window ends."}}"#,
            Some(QuotaWindow::FiveHour),
        ),
        (
            r#"{"error":{"status_code":403,"message":"7-day usage limit exceeded"}}"#,
            Some(QuotaWindow::SevenDay),
        ),
        (
            r#"{"error":{"status":403,"message":"monthly usage limit reached"}}"#,
            Some(QuotaWindow::Month),
        ),
        (
            // 403 信号内嵌在文案里（schema 未枚举，文本同样认）。
            r#"{"error":"403 Forbidden: usage limit reached"}"#,
            None,
        ),
        (
            // 大小写不敏感。
            r#"{"status":403,"message":"Usage Limit reached"}"#,
            None,
        ),
    ];
    for (raw, window) in cases {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::QuotaExhausted { window },
            "应命中 403 配额耗尽：{raw}"
        );
    }
}

/// 官方现行口径（#184）：额度耗尽归 HTTP 429，文案固定为 "You've reached your …
/// usage limit"。同一字符串含 "reached your" 与 "usage limit" 且带 429 信号即命中；
/// 窗口照旧尽力辨认，辨认不出不影响命中。
#[test]
fn classify_matches_official_429_usage_limit_and_extracts_window() {
    let cases = [
        (
            // 2026-10-24 真机会话日志 errorMessage 原文（Kimi Code CLI 2.3.0）：429 只在文案里。
            r#"{"hook_event_name":"StopFailure","session_id":"s1","error":{"name":"APIProviderQuotaExhaustedError","message":"429 You've reached your 5-hour usage limit"}}"#,
            Some(QuotaWindow::FiveHour),
        ),
        (
            // 官方错误参考的完整前缀形态。
            r#"{"hook_event_name":"StopFailure","error":{"status":429,"message":"error, status code: 429, message: You've reached your 5-hour usage limit."}}"#,
            Some(QuotaWindow::FiveHour),
        ),
        (
            r#"{"error":{"status":429,"message":"You've reached your weekly (7-day) usage limit."}}"#,
            Some(QuotaWindow::SevenDay),
        ),
        (
            r#"{"error":{"status_code":429,"message":"You've reached your monthly usage limit for this billing cycle."}}"#,
            Some(QuotaWindow::Month),
        ),
        (
            // 只认 "reached your"，不依赖 You've 的撇号写法。
            r#"{"error":{"status":429,"message":"You’ve reached your weekly (7-day) usage limit."}}"#,
            Some(QuotaWindow::SevenDay),
        ),
        (
            // 大小写不敏感。
            r#"{"status":429,"message":"YOU'VE REACHED YOUR 5-HOUR USAGE LIMIT"}"#,
            Some(QuotaWindow::FiveHour),
        ),
        (
            // 辨认不出窗口照样命中。
            r#"{"error":{"status":429,"message":"You've reached your usage limit."}}"#,
            None,
        ),
    ];
    for (raw, window) in cases {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::QuotaExhausted { window },
            "应命中官方 429 额度耗尽：{raw}"
        );
    }
}

/// 不是额度耗尽的 429 与非配额错误一律不匹配（宁漏勿误）：429 只有在同一字符串
/// 同时含官方耗尽措辞 "reached your" 与 "usage limit" 时才算额度耗尽。
#[test]
fn classify_rejects_non_exhaustion_429_and_non_quota_errors() {
    let cases = [
        r#"{"error":{"status":429,"message":"rate limited, retry later"}}"#,
        r#"{"error":{"status":429,"message":"engine overloaded"}}"#,
        // 官方另外几类 429：引擎过载、请求过多、并发上限（并发文案有 "reached your"，没有 usage limit）。
        r#"{"error":{"status":429,"message":"The engine is currently overloaded, please try again later"}}"#,
        r#"{"error":{"status":429,"message":"Too Many Requests"}}"#,
        r#"{"error":{"status":429,"message":"error, status code: 429, message: You've reached your concurrent request limit."}}"#,
        // 429 且文案提到 usage limit，但没有官方耗尽措辞 "reached your"：不算额度耗尽。
        r#"{"error":{"status":429,"message":"usage limit window busy"}}"#,
        // "reached your" 与 "usage limit" 分在两个字符串：须同一字符串同时含两者。
        r#"{"error":{"status":429,"message":"You've reached your concurrent request limit.","hint":"see the usage limit docs"}}"#,
        // 403 但非配额文案。
        r#"{"error":{"status":403,"message":"forbidden"}}"#,
        // 配额文案但无任何 403 / 429 信号：宁漏勿误。
        r#"{"error":{"status":500,"message":"usage limit lookup failed"}}"#,
        r#"{"error":{"status":500,"message":"You've reached your 5-hour usage limit."}}"#,
        // 非 StopFailure 事件。
        r#"{"hook_event_name":"Stop","session_id":"s1"}"#,
    ];
    for raw in cases {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::Other,
            "不得匹配：{raw}"
        );
    }
}

/// stdin 畸形（非 JSON / 空输入）静默归 Other，观察型放行。
#[test]
fn classify_malformed_stdin_is_silently_other() {
    for raw in ["", "not json", "{broken", "null", "[1,2,"] {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::Other,
            "{raw:?}"
        );
    }
}

/// 403 信号收紧（reviewer 补钉）：字符串里的 "403" 须是独立 token——时间戳
/// （epoch 秒）、request id、行号等数字串里的 "403" 子串不算状态信号，即使文案
/// 同时提到 usage limit 也不命中（宁漏勿误方向的反例钉住）。
#[test]
fn classify_403_signal_requires_standalone_token() {
    // 反例：epoch 时间戳 / request id / 行号 / 时分秒里的 "403" 子串 + usage limit 文案。
    let negatives = [
        r#"{"ts":1760004031,"error":{"status":500,"message":"usage limit lookup failed"}}"#,
        r#"{"request_id":"req-1403f8","error":{"status":429,"message":"usage limit window busy"}}"#,
        r#"{"error":{"status":502,"message":"failed at line 1403 while checking usage limit"}}"#,
        r#"{"error":{"message":"usage limit check failed, retry at 16:40:31"}}"#,
    ];
    for raw in negatives {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::Other,
            "数字串里的 403 子串不得当状态信号：{raw}"
        );
    }

    // 正例：独立 token 形态的 403（含标点边界 / 字符串首）照常命中。
    let positives = [
        (
            r#"{"error":{"message":"403 Forbidden: usage limit reached"}}"#,
            None,
        ),
        (
            r#"{"error":{"message":"status code 403, usage limit reached"}}"#,
            None,
        ),
        (
            r#"{"error":{"message":"(403) monthly usage limit exceeded"}}"#,
            Some(QuotaWindow::Month),
        ),
    ];
    for (raw, window) in positives {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::QuotaExhausted { window },
            "独立 token 的 403 须照常命中：{raw}"
        );
    }
}

/// 429 信号与 403 同一词边界规则（#184）：时间戳、request id、行号等数字串里的
/// "429" 子串不算状态信号，即使文案就是官方耗尽措辞也不命中。
#[test]
fn classify_429_signal_requires_standalone_token() {
    // 反例：只有数字串里的 "429" 子串，没有任何 429 状态信号。
    let negatives = [
        r#"{"ts":"1761429000","error":{"status":500,"message":"You've reached your 5-hour usage limit."}}"#,
        r#"{"ts":1761429000,"error":{"message":"You've reached your 5-hour usage limit."}}"#,
        r#"{"request_id":"req-1429f8","error":{"message":"You've reached your weekly (7-day) usage limit."}}"#,
        r#"{"error":{"status":502,"message":"failed at line 1429: You've reached your monthly usage limit for this billing cycle."}}"#,
    ];
    for raw in negatives {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::Other,
            "数字串里的 429 子串不得当状态信号：{raw}"
        );
    }

    // 正例：独立 token 形态的 429（标点边界 / 字符串首 / 整串）照常命中。
    let positives = [
        (
            r#"{"error":{"message":"(429) You've reached your weekly (7-day) usage limit."}}"#,
            Some(QuotaWindow::SevenDay),
        ),
        (
            r#"{"error":{"message":"status=429 You've reached your monthly usage limit for this billing cycle."}}"#,
            Some(QuotaWindow::Month),
        ),
        (
            r#"{"error":{"code":"429","message":"You've reached your 5-hour usage limit."}}"#,
            Some(QuotaWindow::FiveHour),
        ),
    ];
    for (raw, window) in positives {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::QuotaExhausted { window },
            "独立 token 的 429 须照常命中：{raw}"
        );
    }
}

/// 真实平铺载荷（Kimi Code CLI 2.3.0）：状态码以 "429 " / "403 " 前缀出现在
/// error_message 里。官方 429 三种文案命中并辨认窗口，旧口径 403 同样命中；
/// 标题引用别的窗口的文案不影响窗口辨认。
#[test]
fn classify_matches_real_flat_payload() {
    let cases = [
        // 2026-10-24 真机会话日志 errorMessage 原文。
        (
            "修一下 README",
            "APIProviderQuotaExhaustedError",
            "429 You've reached your 5-hour usage limit",
            Some(QuotaWindow::FiveHour),
        ),
        (
            "修一下 README",
            "APIProviderQuotaExhaustedError",
            "429 You've reached your weekly (7-day) usage limit.",
            Some(QuotaWindow::SevenDay),
        ),
        (
            "修一下 README",
            "APIProviderQuotaExhaustedError",
            "429 You've reached your monthly usage limit for this billing cycle.",
            Some(QuotaWindow::Month),
        ),
        // 旧口径：2026-10-08 真机会话日志还是 `403 … APIStatusError`。
        (
            "修一下 README",
            "APIStatusError",
            "403 You've reached your 5-hour usage limit",
            Some(QuotaWindow::FiveHour),
        ),
        // 标题引用月度文案，真实错误是 5 小时：窗口只从命中的错误文案辨认。
        (
            "You've reached your monthly usage limit for this billing cycle 是什么意思",
            "APIProviderQuotaExhaustedError",
            "429 You've reached your 5-hour usage limit",
            Some(QuotaWindow::FiveHour),
        ),
    ];
    for (title, error_type, message, window) in cases {
        let raw = stop_failure_event(WORKSPACE, title, error_type, message);
        assert_eq!(
            classify_stop_failure(&raw),
            StopFailureSignal::QuotaExhausted { window },
            "应命中额度耗尽：{raw}"
        );
    }
}

/// 承载用户内容的字段不参与分类（#182 评审）：`session_title` 是首句用户提示，
/// `cwd` 是工作目录。用户在提示里贴耗尽报错排查之后，这个会话撞上的瞬时 429、
/// 网络错误或别的 403 都不得判成耗尽。
#[test]
fn classify_ignores_user_content_fields() {
    let cases = [
        // 标题引用耗尽文案 + 过载 429。
        (
            WORKSPACE,
            "You've reached your 5-hour usage limit 之后自动切换没切，帮我查",
            "APIProviderOverloadedError",
            "429 The engine is currently overloaded, please try again later",
        ),
        // 标题引用耗尽文案 + 并发上限 429。
        (
            WORKSPACE,
            "You've reached your 5-hour usage limit 之后自动切换没切，帮我查",
            "APIProviderRateLimitError",
            "429 You've reached your concurrent request limit.",
        ),
        // 标题自带 429 与耗尽文案 + 网络错误。
        (
            WORKSPACE,
            "429 You've reached your 5-hour usage limit 是什么意思",
            "APIConnectionError",
            "Connection error.",
        ),
        // 工作目录里的 429 + 标题引用耗尽文案 + 没有耗尽文案的 500。
        (
            r"D:\work\issue-429",
            "You've reached your 5-hour usage limit 排查",
            "APIStatusError",
            "500 internal: conn closed",
        ),
        // 旧口径：标题含 usage limit + 不是额度耗尽的 403（会员等级不含所请求的模型）。
        (
            WORKSPACE,
            "被动切换的 usage limit 识别口径",
            "APIStatusError",
            "403 Your current subscription does not have access to this model.",
        ),
    ];
    for (cwd, title, error_type, message) in cases {
        let raw = stop_failure_event(cwd, title, error_type, message);
        assert_eq!(
            classify_stop_failure(&raw),
            StopFailureSignal::Other,
            "用户内容不得触发：{raw}"
        );
    }
}

/// 信号局部性（#182 评审）：状态信号须与命中文案出自同一字符串，或是与该字符串
/// 同处一个 JSON 对象的状态码（数值，或整串就是状态码的字符串）。别的对象里的数字、
/// 时间戳、request id，以及同一对象里另一句话中的 "429" / "403"，都不算。
#[test]
fn classify_status_signal_must_be_local_to_matched_text() {
    let cases = [
        r#"{"ts":"2026-10-24T12:39:42.429+08:00","error":{"status":500,"message":"You've reached your 5-hour usage limit."}}"#,
        r#"{"request_id":"req_429_ab","error":{"status":500,"message":"You've reached your 5-hour usage limit."}}"#,
        r#"{"duration_ms":429,"error":{"status":500,"message":"You've reached your 5-hour usage limit."}}"#,
        r#"{"error":{"message":"You've reached your 5-hour usage limit.","detail":"HTTP 429"}}"#,
        // 旧口径同理：403 属于会员等级权限错误，usage limit 在另一个字段。
        r#"{"error_message":"403 Your current subscription does not have access to this model.","hint":"see the usage limit docs"}"#,
    ];
    for raw in cases {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::Other,
            "不在命中文案本地的状态信号不算：{raw}"
        );
    }
}

/// 状态 token 收紧（#182 评审）：`_` 算词字符，数字之间的 `.` 算数字的一部分。
/// 同一字符串里 ISO 毫秒（`12:39:42.429`）、request id（`req_429_ab`）、小数
/// （`429.5`）里的 429 / 403 不算状态信号；句末的 `.` 仍是边界。
#[test]
fn classify_status_token_treats_underscore_and_decimal_point_as_part_of_word() {
    let negatives = [
        r#"{"error":{"message":"[2026-10-24T12:39:42.429+08:00] You've reached your 5-hour usage limit."}}"#,
        r#"{"error":{"message":"req_429_ab: You've reached your 5-hour usage limit."}}"#,
        r#"{"error":{"message":"You've reached your 5-hour usage limit. Retry after 429.5 ms"}}"#,
        r#"{"error":{"message":"[12:39:42.403] usage limit check failed"}}"#,
        r#"{"error":{"message":"req_403_ab: usage limit check failed"}}"#,
    ];
    for raw in negatives {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::Other,
            "数字或标识符里的状态码子串不算：{raw}"
        );
    }

    let positives = [
        (
            r#"{"error":{"message":"You've reached your 5-hour usage limit, status 429."}}"#,
            Some(QuotaWindow::FiveHour),
        ),
        (
            r#"{"error":{"message":"usage limit reached, status 403."}}"#,
            None,
        ),
    ];
    for (raw, window) in positives {
        assert_eq!(
            classify_stop_failure(raw),
            StopFailureSignal::QuotaExhausted { window },
            "句末的点后面没有数字，状态码照常算：{raw}"
        );
    }
}

// ---------------------------------------------------------------------------
// 权威耗尽证据合成
// ---------------------------------------------------------------------------

/// 缓存未达阈值 + 命中窗口 → 该窗口覆写为已用=上限，其余窗口保留缓存真值。
#[test]
fn overlay_overrides_matched_window_to_full() {
    let cached = ready(
        "alice",
        vec![
            quota("alice", QuotaWindow::FiveHour, 80, 100),
            quota("alice", QuotaWindow::SevenDay, 40, 100),
        ],
    );
    let got = overlay_exhausted_snapshot(
        &AccountId("alice".into()),
        Some(&cached),
        Some(QuotaWindow::FiveHour),
        now(),
    );
    let QuotaState::Cached(quotas) = got.state else {
        panic!("合成结果应为 Cached 状态");
    };
    let five = quotas
        .iter()
        .find(|q| q.window == QuotaWindow::FiveHour)
        .unwrap();
    assert_eq!(five.used, 100);
    assert_eq!(five.limit, 100);
    assert_eq!(five.usage_ratio(), Some(1.0));
    let seven = quotas
        .iter()
        .find(|q| q.window == QuotaWindow::SevenDay)
        .unwrap();
    assert_eq!((seven.used, seven.limit), (40, 100), "其余窗口保留缓存真值");
    assert_eq!(got.stale, false);
}

/// 文案辨认不出窗口 → 覆写已用比例最高的窗口。
#[test]
fn overlay_without_window_picks_max_ratio_window() {
    let cached = ready(
        "alice",
        vec![
            quota("alice", QuotaWindow::FiveHour, 30, 100),
            quota("alice", QuotaWindow::SevenDay, 60, 100),
        ],
    );
    let got = overlay_exhausted_snapshot(&AccountId("alice".into()), Some(&cached), None, now());
    let QuotaState::Cached(quotas) = got.state else {
        panic!("合成结果应为 Cached 状态");
    };
    let seven = quotas
        .iter()
        .find(|q| q.window == QuotaWindow::SevenDay)
        .unwrap();
    assert_eq!(seven.usage_ratio(), Some(1.0));
    let five = quotas
        .iter()
        .find(|q| q.window == QuotaWindow::FiveHour)
        .unwrap();
    assert_eq!(five.usage_ratio(), Some(0.3));
}

/// 命中窗口不在缓存 → 追加合成窗口（已用=上限=1）。
#[test]
fn overlay_appends_missing_window() {
    let cached = ready(
        "alice",
        vec![quota("alice", QuotaWindow::FiveHour, 10, 100)],
    );
    let got = overlay_exhausted_snapshot(
        &AccountId("alice".into()),
        Some(&cached),
        Some(QuotaWindow::Month),
        now(),
    );
    let QuotaState::Cached(quotas) = got.state else {
        panic!("合成结果应为 Cached 状态");
    };
    assert_eq!(quotas.len(), 2);
    let month = quotas
        .iter()
        .find(|q| q.window == QuotaWindow::Month)
        .unwrap();
    assert_eq!(month.usage_ratio(), Some(1.0));
}

/// 无可用缓存（含 Failed / stale）→ 整条合成，窗口缺省 5 小时。
#[test]
fn overlay_synthesizes_full_snapshot_without_cache() {
    for cached in [None, Some(&failed_snapshot("alice"))] {
        let got = overlay_exhausted_snapshot(&AccountId("alice".into()), cached, None, now());
        let QuotaState::Cached(quotas) = got.state else {
            panic!("合成结果应为 Cached 状态");
        };
        assert_eq!(quotas.len(), 1, "无缓存底时只合成一条窗口");
        assert_eq!(quotas[0].window, QuotaWindow::FiveHour);
        assert_eq!(quotas[0].usage_ratio(), Some(1.0));
        assert_eq!(got.fetched_at, now());
    }
}

fn failed_snapshot(id: &str) -> QuotaSnapshot {
    QuotaSnapshot {
        account_id: AccountId(id.into()),
        fetched_at: now(),
        stale: false,
        membership: None,
        state: QuotaState::Failed(kimi_switch_core::error::QuotaFailure {
            kind: kimi_switch_core::error::QuotaFailureKind::Other,
            status: Some(500),
            message: "boom".into(),
        }),
    }
}

// ---------------------------------------------------------------------------
// 被动门面语义（与轮询路径同一判定单点，仅安顿宽限放行）
// ---------------------------------------------------------------------------

/// 同一夹具：安顿宽限内轮询路径 Stay(SettleGrace)，被动路径 Swap——这是两个门面
/// 的唯一语义差异（spec #152 决策 14）。
#[test]
fn passive_bypasses_settle_grace_while_polling_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("auto_swap.json");
    // last_used_at = now：默认 60s 安顿宽限内。
    let accounts = vec![acc("alice", true, Some(now())), acc("bob", false, None)];
    let snapshots = vec![
        ready(
            "alice",
            vec![quota("alice", QuotaWindow::FiveHour, 100, 100)],
        ),
        ready("bob", vec![quota("bob", QuotaWindow::FiveHour, 10, 100)]),
    ];

    assert_eq!(
        decide_auto_swap_from_state(&state, &accounts, &snapshots, &on(), now(), None),
        AutoSwapDecision::Stay {
            reason: AutoSwapSkipReason::SettleGrace
        },
        "轮询路径照常被安顿宽限挡住"
    );
    assert_eq!(
        decide_auto_swap_passive_from_state(&state, &accounts, &snapshots, &on(), now(), None),
        AutoSwapDecision::Swap(kimi_switch_core::AutoSwapTarget {
            from: AccountId("alice".into()),
            to: AccountId("bob".into()),
            trigger_window: QuotaWindow::FiveHour,
            usage_ratio: 1.0,
            kind: kimi_switch_core::AutoSwapKind::Exhausted,
        }),
        "额度耗尽原文是权威耗尽证据，不受安顿宽限阻挡"
    );
}

/// 手动保持期不阻挡被动耗尽切换（保持期只约束回驻与新开抢用；spec 明文语义回归钉）。
#[test]
fn passive_ignores_manual_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("auto_swap.json");
    set_manual_hold_until(&state, now() + chrono::Duration::hours(1));
    let accounts = vec![acc("alice", true, None), acc("bob", false, None)];
    let snapshots = vec![
        ready(
            "alice",
            vec![quota("alice", QuotaWindow::FiveHour, 100, 100)],
        ),
        ready("bob", vec![quota("bob", QuotaWindow::FiveHour, 10, 100)]),
    ];
    assert!(matches!(
        decide_auto_swap_passive_from_state(&state, &accounts, &snapshots, &on(), now(), None),
        AutoSwapDecision::Swap(_)
    ));
}

/// 冷却照常：刚自动切换过 → 被动触发也落空（时刻由调用方从状态文件读出传入，
/// 与 CLI / GUI 编排同一形态）。
#[test]
fn passive_respects_cooldown() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("auto_swap.json");
    write_last_auto_swap_at(&state, now());
    let last = kimi_switch_core::auto_swap::read_last_auto_swap_at(&state);
    let accounts = vec![acc("alice", true, None), acc("bob", false, None)];
    let snapshots = vec![
        ready(
            "alice",
            vec![quota("alice", QuotaWindow::FiveHour, 100, 100)],
        ),
        ready("bob", vec![quota("bob", QuotaWindow::FiveHour, 10, 100)]),
    ];
    assert_eq!(
        decide_auto_swap_passive_from_state(&state, &accounts, &snapshots, &on(), now(), last),
        AutoSwapDecision::Stay {
            reason: AutoSwapSkipReason::Cooldown
        }
    );
}

/// 合格线与摘出继承：摘出账号不作候选；唯一候选已达阈值 → 无合格候选。
#[test]
fn passive_inherits_eligibility_line_and_opt_out() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("auto_swap.json");

    // 摘出的 bob 被跳过，切到健康的 carol。
    let mut opted_out_bob = acc("bob", false, None);
    kimi_switch_core::auto_swap_opt_out::write_auto_swap_opt_out_extra(
        &mut opted_out_bob.extra,
        true,
    );
    let accounts = vec![
        acc("alice", true, None),
        opted_out_bob,
        acc("carol", false, None),
    ];
    let snapshots = vec![
        ready(
            "alice",
            vec![quota("alice", QuotaWindow::FiveHour, 100, 100)],
        ),
        ready("bob", vec![quota("bob", QuotaWindow::FiveHour, 1, 100)]),
        ready(
            "carol",
            vec![quota("carol", QuotaWindow::FiveHour, 10, 100)],
        ),
    ];
    let got =
        decide_auto_swap_passive_from_state(&state, &accounts, &snapshots, &on(), now(), None);
    match got {
        AutoSwapDecision::Swap(target) => {
            assert_eq!(target.to.0, "carol", "摘出的 bob 不得成为候选")
        }
        other => panic!("应发生切换：{other:?}"),
    }

    // 唯一候选已达阈值（0.99）→ 无合格候选。
    let accounts = vec![acc("alice", true, None), acc("bob", false, None)];
    let snapshots = vec![
        ready(
            "alice",
            vec![quota("alice", QuotaWindow::FiveHour, 100, 100)],
        ),
        ready("bob", vec![quota("bob", QuotaWindow::FiveHour, 99, 100)]),
    ];
    assert_eq!(
        decide_auto_swap_passive_from_state(&state, &accounts, &snapshots, &on(), now(), None),
        AutoSwapDecision::Stay {
            reason: AutoSwapSkipReason::NoEligibleCandidate
        }
    );
}

/// 当前账号无快照（缓存缺失）时，overlay 合成证据后被动路径照常进入耗尽判定，
/// 不落 `CurrentUnavailable`——被动触发不依赖缓存新鲜度。
#[test]
fn passive_with_overlay_never_fails_on_missing_current_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("auto_swap.json");
    let accounts = vec![acc("alice", true, None), acc("bob", false, None)];
    // 只有 bob 有快照；alice 的快照由 overlay 合成。
    let bob = ready("bob", vec![quota("bob", QuotaWindow::FiveHour, 10, 100)]);
    let alice = overlay_exhausted_snapshot(&AccountId("alice".into()), None, None, now());
    let snapshots = vec![alice, bob];
    assert!(matches!(
        decide_auto_swap_passive_from_state(&state, &accounts, &snapshots, &on(), now(), None),
        AutoSwapDecision::Swap(_)
    ));
}

/// 总开关关闭：被动路径与轮询同闸，零副作用。
#[test]
fn passive_respects_master_switch() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("auto_swap.json");
    let accounts = vec![acc("alice", true, None), acc("bob", false, None)];
    let snapshots = vec![
        ready(
            "alice",
            vec![quota("alice", QuotaWindow::FiveHour, 100, 100)],
        ),
        ready("bob", vec![quota("bob", QuotaWindow::FiveHour, 10, 100)]),
    ];
    let mut settings = on();
    settings.enabled = false;
    assert_eq!(
        decide_auto_swap_passive_from_state(&state, &accounts, &snapshots, &settings, now(), None),
        AutoSwapDecision::Stay {
            reason: AutoSwapSkipReason::Disabled
        }
    );
    assert!(!state.exists(), "开关关闭不产生任何状态写入");
}
