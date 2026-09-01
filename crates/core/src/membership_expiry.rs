//! 会员展示共享逻辑：档位名映射、会员到期日格式化与 extra 写侧辅助（GUI / CLI 共用）。
//!
//! 「会员到期」指官方付费档位对这张账号失效的那一天（词汇见仓库 CONTEXT.md）。
//! 到期瞬间存放在账号非秘密元数据 [`crate::model::Account::extra`]，由
//! 「绑定网页登录」管道写入（issue #50，见 ADR-0004）；本模块定义两侧契约：
//! 读侧 schema 与展示格式化 + 写侧辅助 [`write_membership_extra`]，
//! 不发网络请求、不接触网页 token 本体（token 由凭证仓库按
//! [`WEB_STORE_FIELD`] 独立字段持有）。
//!
//! extra schema（写入方按此对接，本模块即读侧契约）：
//! - [`EXTRA_NEXT_BILLING_TIME`]：下次扣费瞬间，RFC3339 UTC 字符串，
//!   对应官网接口的 `nextBillingTime`（如 `"2027-08-27T01:25:37.930341Z"`）。
//! - [`EXTRA_CURRENT_END_TIME`]：当前会员周期结束瞬间，RFC3339 UTC 字符串，
//!   对应官网接口的 `currentEndTime`（如 `"2027-08-28T00:00:00Z"`）。
//!
//! 两个键都是扁平可选字符串；缺失、类型不符或解析失败一律按「无到期日」处理，
//! 展示层回退到绑定弱提示，而不是错误状态。

use chrono::{DateTime, FixedOffset, NaiveDate, SecondsFormat, Timelike, Utc};

/// extra 键：下次扣费瞬间（官网 `nextBillingTime`），RFC3339 UTC 字符串。
pub const EXTRA_NEXT_BILLING_TIME: &str = "membership_next_billing_time";
/// extra 键：当前会员周期结束瞬间（官网 `currentEndTime`），RFC3339 UTC 字符串。
pub const EXTRA_CURRENT_END_TIME: &str = "membership_current_end_time";

/// 凭证仓库中网页消费者 token 的字段名（issue #50，ADR-0004）。
///
/// 与 Code OAuth blob 的 store_field（默认 `"blob"`）分离：同账号各自一条，
/// 互不覆盖。绑定管道写入；`remove_account` 按此清理。没有绑定过的账号
/// 删除该键是幂等 no-op。
pub const WEB_STORE_FIELD: &str = "web";

/// 将 `/usages` 的 `user.membership.level` 转成官网档位名。
///
/// 已与 `/me` 的 `user_level_name` 核对：`LEVEL_INTERMEDIATE` → Allegretto，
/// `LEVEL_ADVANCED` → Allegro。未知档位仍去掉 `LEVEL_` 后首字母大写。
pub fn prettify_membership(level: &str) -> String {
    let stripped = level.strip_prefix("LEVEL_").unwrap_or(level);
    match stripped.to_ascii_uppercase().as_str() {
        "INTERMEDIATE" => "Allegretto".to_string(),
        "ADVANCED" => "Allegro".to_string(),
        _ => {
            let mut chars = stripped.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => stripped.to_string(),
            }
        }
    }
}

/// 会员到期展示日（东八区 `YYYY-MM-DD`）。
///
/// 规则（与官网展示一致，钉 2026-08-31 实抓样例）：
/// - 有 `nextBillingTime`：取它的东八区日历日；
/// - 否则有 `currentEndTime`：
///   - 恰为 UTC 零点（00:00:00.000000000）：按右开语义显示前一天的东八区日期；
///   - 带非零点钟点：直接显示它的东八区日历日，不误减一天；
/// - 两者皆无：`None`，展示层回退到绑定弱提示。
pub fn format_membership_expiry(
    current_end_time: Option<DateTime<Utc>>,
    next_billing_time: Option<DateTime<Utc>>,
) -> Option<String> {
    if let Some(next) = next_billing_time {
        return Some(calendar_day_east8(next));
    }
    let current = current_end_time?;
    if is_utc_midnight(current) {
        current
            .with_timezone(&east8())
            .date_naive()
            .pred_opt()
            .map(|day| day.format("%Y-%m-%d").to_string())
    } else {
        Some(calendar_day_east8(current))
    }
}

/// 会员剩余天数：`N = 到期展示日 − 今天`，两边都是东八区日历日。
///
/// - 正整数：还没到期
/// - `0`：今天到期
/// - 负整数：已过期 `|N|` 天
///
/// `expiry_day` 必须是 [`format_membership_expiry`] 产出的 `YYYY-MM-DD`，
/// 禁止拿原始 RFC3339 瞬间（例如 `currentEndTime=2027-08-28T00:00:00Z`）
/// 的东八区日历日去减——#49 实抓会差一天。`now` 由调用方注入（组装账号行
/// 快照时的 UTC now），测试可冻结。解析失败返回 `None`（不画剩余天数）。
pub fn membership_remaining_days(expiry_day: &str, now: DateTime<Utc>) -> Option<i32> {
    let expiry = NaiveDate::parse_from_str(expiry_day, "%Y-%m-%d").ok()?;
    let today = now.with_timezone(&east8()).date_naive();
    i32::try_from((expiry - today).num_days()).ok()
}

/// 从账号 extra 读出到期瞬间并格式化为东八区日历日。
///
/// 键缺失、值不是字符串或不是合法 RFC3339 瞬间都返回 `None`（读侧宽容：
/// 坏数据只降级为「无到期日」，不是错误状态）。
pub fn membership_expiry_from_extra(
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let parse_instant = |key: &str| -> Option<DateTime<Utc>> {
        extra
            .get(key)
            .and_then(|value| value.as_str())
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
            .map(|instant| instant.with_timezone(&Utc))
    };
    format_membership_expiry(
        parse_instant(EXTRA_CURRENT_END_TIME),
        parse_instant(EXTRA_NEXT_BILLING_TIME),
    )
}

/// 绑定网页登录的写侧辅助：把两个会员瞬间写进 extra（issue #50，ADR-0004）。
///
/// 契约（与读侧 schema 对齐）：
/// - **只动两个 membership 键**，保留 dedup / scope 等其他既有键——调用方
///   必须走「读出 Account → 改 extra → registry upsert」路径，不得整体重建 extra；
/// - `None` 的瞬间移除对应键：重新绑定后旧日期不得残留（比如会员已到期）；
/// - 值格式化为 RFC3339 UTC 字符串（AutoSi 精度：整秒无小数、亚秒保留实际位数，
///   与官网响应字面量一致），读侧 [`membership_expiry_from_extra`] 原样可读。
///
/// 展示文本（东八区日历日）由调用方用 [`format_membership_expiry`] 自行计算。
pub fn write_membership_extra(
    extra: &mut serde_json::Map<String, serde_json::Value>,
    current_end_time: Option<DateTime<Utc>>,
    next_billing_time: Option<DateTime<Utc>>,
) {
    write_instant(extra, EXTRA_CURRENT_END_TIME, current_end_time);
    write_instant(extra, EXTRA_NEXT_BILLING_TIME, next_billing_time);
}

/// 单键写入：`Some` 覆写、`None` 移除。
fn write_instant(
    extra: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    instant: Option<DateTime<Utc>>,
) {
    match instant {
        Some(instant) => {
            extra.insert(
                key.to_string(),
                serde_json::Value::String(instant.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
            );
        }
        None => {
            extra.remove(key);
        }
    }
}

/// 展示时区：固定东八区（与官网一致），不跟操作系统时区走，也不引入 chrono-tz。
fn east8() -> FixedOffset {
    // `east` 在新版 chrono 已弃用；常量偏移永不越界，`expect` 不可达。
    FixedOffset::east_opt(8 * 3600).expect("UTC+8 是合法偏移")
}

/// 瞬间的东八区日历日（`YYYY-MM-DD`）。
fn calendar_day_east8(instant: DateTime<Utc>) -> String {
    instant
        .with_timezone(&east8())
        .format("%Y-%m-%d")
        .to_string()
}

/// 时间分量是否恰为 UTC 零点（含小数秒为零）。
fn is_utc_midnight(instant: DateTime<Utc>) -> bool {
    let time = instant.time();
    time.hour() == 0 && time.minute() == 0 && time.second() == 0 && time.nanosecond() == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 2026-08-31 官网实抓样例：currentEndTime=2027-08-28T00:00:00Z、
    /// nextBillingTime=2027-08-27T01:25:37.930341Z，官网展示「有效期至 2027-08-27」。
    /// 期望值直接用实抓字面量，不在测试里重算。
    #[test]
    fn captured_sample_formats_to_2027_08_27() {
        let current_end = "2027-08-28T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let next_billing = "2027-08-27T01:25:37.930341Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        assert_eq!(
            format_membership_expiry(Some(current_end), Some(next_billing)).as_deref(),
            Some("2027-08-27")
        );
        // extra 读侧同一样例走同一结果。
        let extra = extra_with(&[
            (EXTRA_CURRENT_END_TIME, "2027-08-28T00:00:00Z"),
            (EXTRA_NEXT_BILLING_TIME, "2027-08-27T01:25:37.930341Z"),
        ]);
        assert_eq!(
            membership_expiry_from_extra(&extra).as_deref(),
            Some("2027-08-27")
        );
    }

    /// nextBillingTime 优先，且取的是东八区日历日：UTC 23 点跨日到东八区次日。
    #[test]
    fn next_billing_time_wins_and_uses_east8_calendar_day() {
        let current_end = "2027-08-28T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let next_billing = "2027-08-30T23:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            format_membership_expiry(Some(current_end), Some(next_billing)).as_deref(),
            Some("2027-08-31")
        );
    }

    /// 无 nextBillingTime 且 currentEndTime 为 UTC 零点：按右开语义显示前一天。
    #[test]
    fn utc_midnight_current_end_shows_previous_day() {
        let current_end = "2027-08-28T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            format_membership_expiry(Some(current_end), None).as_deref(),
            Some("2027-08-27")
        );
        let extra = extra_with(&[(EXTRA_CURRENT_END_TIME, "2027-08-28T00:00:00Z")]);
        assert_eq!(
            membership_expiry_from_extra(&extra).as_deref(),
            Some("2027-08-27")
        );
    }

    /// currentEndTime 带非零点钟点：直接取其东八区日历日，不误减一天。
    #[test]
    fn non_midnight_current_end_keeps_own_east8_day() {
        // 东八区 2027-08-28 09:25，仍是 08-28。
        let same_day = "2027-08-28T01:25:37Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            format_membership_expiry(Some(same_day), None).as_deref(),
            Some("2027-08-28")
        );
        // UTC 傍晚跨到东八区次日，证明回退路径也是东八区而非 UTC。
        let crosses_day = "2027-08-27T18:30:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            format_membership_expiry(Some(crosses_day), None).as_deref(),
            Some("2027-08-28")
        );
    }

    /// 两者皆无：None（展示层回退弱提示，不是错误）。
    #[test]
    fn none_when_both_instants_absent() {
        assert_eq!(format_membership_expiry(None, None), None);
        assert_eq!(membership_expiry_from_extra(&serde_json::Map::new()), None);
    }

    /// extra 值损坏（非字符串 / 非法 RFC3339）按无到期日处理。
    #[test]
    fn malformed_extra_values_degrade_to_none() {
        let mut extra = extra_with(&[(EXTRA_NEXT_BILLING_TIME, "not-a-timestamp")]);
        assert_eq!(membership_expiry_from_extra(&extra), None);
        extra.insert(
            EXTRA_CURRENT_END_TIME.into(),
            serde_json::Value::Bool(false),
        );
        assert_eq!(membership_expiry_from_extra(&extra), None);
    }

    /// 档位名映射与 GUI 现行展示一致（下沉自 gui backend，语义不变）。
    #[test]
    fn prettify_membership_uses_official_plan_names() {
        assert_eq!(prettify_membership("LEVEL_INTERMEDIATE"), "Allegretto");
        assert_eq!(prettify_membership("LEVEL_ADVANCED"), "Allegro");
        assert_eq!(prettify_membership("intermediate"), "Allegretto");
        assert_eq!(prettify_membership("LEVEL_TEAM"), "Team");
        assert_eq!(prettify_membership(""), "");
    }

    fn extra_with(pairs: &[(&str, &str)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(key, value)| {
                (
                    (*key).to_string(),
                    serde_json::Value::String((*value).to_string()),
                )
            })
            .collect()
    }

    /// 东八区固定偏移 +8（不跟系统时区走）。
    #[test]
    fn east8_offset_is_plus_8_hours() {
        let utc_noon = Utc.with_ymd_and_hms(2027, 8, 27, 12, 0, 0).unwrap();
        assert_eq!(calendar_day_east8(utc_noon), "2027-08-27");
        assert_eq!(east8().local_minus_utc(), 8 * 3600);
    }

    // --- 写侧辅助（issue #50，ADR-0004）---

    /// 写入后读回同一展示日，且值字面量与官网实抓样例一致
    /// （AutoSi：整秒无小数、亚秒保留实际位数）。
    #[test]
    fn write_membership_extra_roundtrips_captured_sample() {
        let current_end = "2027-08-28T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let next_billing = "2027-08-27T01:25:37.930341Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        let mut extra = serde_json::Map::new();
        write_membership_extra(&mut extra, Some(current_end), Some(next_billing));
        assert_eq!(
            extra.get(EXTRA_CURRENT_END_TIME).and_then(|v| v.as_str()),
            Some("2027-08-28T00:00:00Z")
        );
        assert_eq!(
            extra.get(EXTRA_NEXT_BILLING_TIME).and_then(|v| v.as_str()),
            Some("2027-08-27T01:25:37.930341Z")
        );
        assert_eq!(
            membership_expiry_from_extra(&extra).as_deref(),
            Some("2027-08-27")
        );
    }

    /// 只动两个 membership 键：既有其他键原样保留。
    #[test]
    fn write_membership_extra_preserves_unrelated_keys() {
        let mut extra = extra_with(&[("dedup_key", "stable-id"), ("scope", "kimi-code")]);
        extra.insert(
            EXTRA_NEXT_BILLING_TIME.into(),
            serde_json::Value::String("2026-01-01T00:00:00Z".into()),
        );
        let current_end = "2027-08-28T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        write_membership_extra(&mut extra, Some(current_end), None);
        assert_eq!(
            extra.get("dedup_key").and_then(|v| v.as_str()),
            Some("stable-id")
        );
        assert_eq!(
            extra.get("scope").and_then(|v| v.as_str()),
            Some("kimi-code")
        );
        assert_eq!(
            extra.get(EXTRA_CURRENT_END_TIME).and_then(|v| v.as_str()),
            Some("2027-08-28T00:00:00Z")
        );
        // nextBillingTime 传 None → 旧值被移除（重新绑定后旧日期不得残留）。
        assert!(extra.get(EXTRA_NEXT_BILLING_TIME).is_none());
        assert_eq!(
            membership_expiry_from_extra(&extra).as_deref(),
            Some("2027-08-27")
        );
    }

    /// 两个瞬间都 None：两个键一并移除（回到「无到期日」）。
    #[test]
    fn write_membership_extra_removes_both_keys_when_both_none() {
        let mut extra = extra_with(&[
            (EXTRA_CURRENT_END_TIME, "2027-08-28T00:00:00Z"),
            (EXTRA_NEXT_BILLING_TIME, "2027-08-27T01:25:37.930341Z"),
        ]);
        write_membership_extra(&mut extra, None, None);
        assert!(extra.get(EXTRA_CURRENT_END_TIME).is_none());
        assert!(extra.get(EXTRA_NEXT_BILLING_TIME).is_none());
        assert_eq!(membership_expiry_from_extra(&extra), None);
    }

    // --- 剩余天数（issue #61）：N = 到期展示日 − 今天，两边都是东八区日历日 ---

    /// #49 实抓展示日 2027-08-27，冻结东八区今天 2026-09-01 → 剩余 360 天。
    /// 期望值是独立算出的字面量，不在测试里重做日期减法。
    #[test]
    fn remaining_days_uses_display_day_not_current_end_east8_day() {
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        assert_eq!(
            membership_remaining_days("2027-08-27", now),
            Some(360),
            "必须消费展示日 2027-08-27"
        );
        // 陷阱：currentEndTime=2027-08-28T00:00:00Z 的东八区日历日是 2027-08-28，
        // 用它去减会多一天。函数禁止走这条路。
        assert_eq!(
            calendar_day_east8("2027-08-28T00:00:00Z".parse().unwrap()),
            "2027-08-28"
        );
        assert_eq!(
            membership_remaining_days("2027-08-28", now),
            Some(361),
            "若误用 currentEndTime 的东八区日会得到 361"
        );
        assert_ne!(
            membership_remaining_days("2027-08-27", now),
            membership_remaining_days("2027-08-28", now)
        );
    }

    #[test]
    fn remaining_days_zero_when_expiry_is_today_east8() {
        // UTC 零点 = 东八区当天上午，日历日仍是 2027-08-27。
        let now = Utc.with_ymd_and_hms(2027, 8, 27, 0, 0, 0).unwrap();
        assert_eq!(membership_remaining_days("2027-08-27", now), Some(0));
    }

    #[test]
    fn remaining_days_negative_when_expiry_already_passed() {
        let now = Utc.with_ymd_and_hms(2027, 8, 30, 0, 0, 0).unwrap();
        assert_eq!(membership_remaining_days("2027-08-27", now), Some(-3));
    }

    /// 时钟按东八区日历日，不是 UTC 日期：UTC 15:59 仍是东八区前一天。
    #[test]
    fn remaining_days_today_is_east8_calendar_not_utc_date() {
        let expiry = "2027-08-27";
        // UTC 2026-08-31 16:00 = 东八区 2026-09-01 00:00 → 与 09-01 零点同一天。
        let east8_new_day = Utc.with_ymd_and_hms(2026, 8, 31, 16, 0, 0).unwrap();
        assert_eq!(membership_remaining_days(expiry, east8_new_day), Some(360));
        // UTC 2026-08-31 15:59 = 东八区 2026-08-31 23:59 → 还多一天。
        let still_aug_31 = Utc.with_ymd_and_hms(2026, 8, 31, 15, 59, 59).unwrap();
        assert_eq!(membership_remaining_days(expiry, still_aug_31), Some(361));
    }

    #[test]
    fn remaining_days_none_when_display_day_malformed() {
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        assert_eq!(membership_remaining_days("not-a-date", now), None);
        assert_eq!(
            membership_remaining_days("2027-08-28T00:00:00Z", now),
            None,
            "禁止拿原始 RFC3339 瞬间当展示日"
        );
    }
}
