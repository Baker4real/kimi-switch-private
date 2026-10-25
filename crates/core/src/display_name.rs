//! 官方昵称缓存与显示名。
//!
//! extra 键 [`EXTRA_OFFICIAL_NICKNAME`] 只存官网 `nickname` 字符串；写侧只动这一键。
//! 空串视为未知。显示名规则：官方昵称 → 用户改过的别名 → 账号 id。

use serde_json::{Map, Value};

use crate::account_registry::AccountRegistry;
use crate::error::Result;
use crate::model::AccountId;

/// extra 键：官方昵称（`GET api.kimi.com/coding/v1/me` 的 `nickname`）。
pub const EXTRA_OFFICIAL_NICKNAME: &str = "official_nickname";

/// extra 键：官方会员档位（`/me` 的 `user_level_name`，或 GetSubscription 的
/// `membershipLevel`），存 [`crate::membership_expiry::normalize_membership_level`]
/// 归一后的 `LEVEL_*` 串。2026-09-14 官方 `/usages` 移除 `user` 对象后，这是
/// 档位的唯一缓存来源。
pub const EXTRA_OFFICIAL_MEMBERSHIP: &str = "official_membership";

/// 把官方昵称写入 extra：只动 [`EXTRA_OFFICIAL_NICKNAME`]，其它键原样保留。
pub fn write_official_nickname_extra(extra: &mut Map<String, Value>, nickname: &str) {
    extra.insert(
        EXTRA_OFFICIAL_NICKNAME.to_string(),
        Value::String(nickname.to_string()),
    );
}

/// 读出非空官方昵称；缺键、非字符串、空/空白都视为未知。
pub fn official_nickname_from_extra(extra: &Map<String, Value>) -> Option<&str> {
    extra
        .get(EXTRA_OFFICIAL_NICKNAME)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// 官方昵称是否已知（非空缓存）。
pub fn official_nickname_known(extra: &Map<String, Value>) -> bool {
    official_nickname_from_extra(extra).is_some()
}

/// 界面显示名：官方昵称非空 → 用之；否则别名与账号 id 不同（用户改过名）→ 用别名；否则用 id。
pub fn display_name(account: &crate::model::Account) -> String {
    if let Some(nickname) = official_nickname_from_extra(&account.extra) {
        return nickname.to_string();
    }
    if account.label != account.id.0 {
        return account.label.clone();
    }
    account.id.0.clone()
}

/// 把官方昵称写回账号 extra：账号不存在则跳过；只动昵称键，保留其它 extra。
pub fn persist_official_nickname(
    registry: &AccountRegistry,
    provider: &str,
    id: &AccountId,
    nickname: &str,
) -> Result<()> {
    registry.update_extra(provider, id, |extra| {
        write_official_nickname_extra(extra, nickname);
    })
}

/// 把官方会员档位写入 extra：先经 [`normalize_membership_level`] 归一，
/// `Some` 覆写、`None`（含空串）移除该键（降档到免费 / 无档位时旧值不残留）。
/// 只动 [`EXTRA_OFFICIAL_MEMBERSHIP`]，其它键原样保留。
pub fn write_official_membership_extra(extra: &mut Map<String, Value>, level: Option<&str>) {
    match level.and_then(crate::membership_expiry::normalize_membership_level) {
        Some(normalized) => {
            extra.insert(
                EXTRA_OFFICIAL_MEMBERSHIP.to_string(),
                Value::String(normalized),
            );
        }
        None => {
            extra.remove(EXTRA_OFFICIAL_MEMBERSHIP);
        }
    }
}

/// 读出归一后的官方会员档位；缺键、非字符串、空/空白都视为未知。
pub fn official_membership_from_extra(extra: &Map<String, Value>) -> Option<String> {
    extra
        .get(EXTRA_OFFICIAL_MEMBERSHIP)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extra_with(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), Value::String((*value).to_string())))
            .collect()
    }

    #[test]
    fn membership_extra_normalizes_overwrites_and_removes() {
        let mut extra = extra_with(&[
            ("dedup_key", "stable-id"),
            (EXTRA_OFFICIAL_NICKNAME, "Baker"),
        ]);
        // 官网名归一成 LEVEL_* 存档，只动档位键。
        write_official_membership_extra(&mut extra, Some("Allegretto"));
        assert_eq!(
            extra.get(EXTRA_OFFICIAL_MEMBERSHIP).and_then(Value::as_str),
            Some("LEVEL_INTERMEDIATE")
        );
        assert_eq!(
            extra.get("dedup_key").and_then(Value::as_str),
            Some("stable-id")
        );
        assert_eq!(
            extra.get(EXTRA_OFFICIAL_NICKNAME).and_then(Value::as_str),
            Some("Baker")
        );
        assert_eq!(
            official_membership_from_extra(&extra).as_deref(),
            Some("LEVEL_INTERMEDIATE")
        );
        // None / 空串 → 移除（降档免费不留旧值），读取回到未知。
        write_official_membership_extra(&mut extra, None);
        assert!(extra.get(EXTRA_OFFICIAL_MEMBERSHIP).is_none());
        assert_eq!(official_membership_from_extra(&extra), None);
        write_official_membership_extra(&mut extra, Some("  "));
        assert!(extra.get(EXTRA_OFFICIAL_MEMBERSHIP).is_none());
        // 缺键读取 = 未知。
        let empty = Map::new();
        assert_eq!(official_membership_from_extra(&empty), None);
    }

    #[test]
    fn write_official_nickname_overwrites_only_that_key() {
        let mut extra = extra_with(&[
            ("dedup_key", "stable-id"),
            (
                "membership_next_billing_time",
                "2027-08-27T01:25:37.930341Z",
            ),
            (EXTRA_OFFICIAL_NICKNAME, "OldName"),
        ]);
        write_official_nickname_extra(&mut extra, "Forever");
        assert_eq!(
            extra.get("dedup_key").and_then(Value::as_str),
            Some("stable-id")
        );
        assert_eq!(
            extra
                .get("membership_next_billing_time")
                .and_then(Value::as_str),
            Some("2027-08-27T01:25:37.930341Z")
        );
        assert_eq!(official_nickname_from_extra(&extra), Some("Forever"));
        assert!(official_nickname_known(&extra));
    }

    #[test]
    fn blank_official_nickname_is_unknown() {
        let extra = extra_with(&[(EXTRA_OFFICIAL_NICKNAME, "  ")]);
        assert_eq!(official_nickname_from_extra(&extra), None);
        assert!(!official_nickname_known(&extra));
        let empty = Map::new();
        assert_eq!(official_nickname_from_extra(&empty), None);
        assert!(!official_nickname_known(&empty));
    }

    #[test]
    fn persist_official_nickname_does_not_drop_membership_under_contention() {
        use std::sync::Arc;
        use std::thread;

        let tmp = tempfile::tempdir().unwrap();
        let registry = Arc::new(AccountRegistry::new(tmp.path().join("registry.toml")));
        let id = crate::model::AccountId("u1".into());
        let mut extra = extra_with(&[(EXTRA_OFFICIAL_NICKNAME, "OldName")]);
        extra.insert(
            "membership_next_billing_time".into(),
            serde_json::Value::String("2027-01-01T00:00:00Z".into()),
        );
        registry
            .upsert(crate::model::Account {
                provider: "kimi".into(),
                id: id.clone(),
                label: "u1".into(),
                active: false,
                created_at: chrono::Utc::now(),
                last_used_at: None,
                extra,
            })
            .unwrap();

        let persist_reg = Arc::clone(&registry);
        let persist_id = id.clone();
        let persist = thread::spawn(move || {
            persist_official_nickname(&persist_reg, "kimi", &persist_id, "Forever").unwrap();
        });

        let membership_reg = Arc::clone(&registry);
        let membership_id = id.clone();
        let membership = thread::spawn(move || {
            membership_reg
                .update_extra("kimi", &membership_id, |extra| {
                    extra.insert(
                        "membership_next_billing_time".into(),
                        serde_json::Value::String("2027-08-27T01:25:37.930341Z".into()),
                    );
                })
                .unwrap();
        });

        persist.join().unwrap();
        membership.join().unwrap();

        let stored = registry.find("kimi", &id).unwrap().unwrap();
        assert_eq!(official_nickname_from_extra(&stored.extra), Some("Forever"));
        assert_eq!(
            stored
                .extra
                .get("membership_next_billing_time")
                .and_then(serde_json::Value::as_str),
            Some("2027-08-27T01:25:37.930341Z"),
            "persist 整行覆盖会丢掉并发写入的会员 extra"
        );
    }

    #[test]
    fn persist_official_nickname_overwrites_cache_and_keeps_other_extra() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        let mut extra = extra_with(&[("dedup_key", "stable-id")]);
        write_official_nickname_extra(&mut extra, "OldName");
        registry
            .upsert(crate::model::Account {
                provider: "kimi".into(),
                id: crate::model::AccountId("u1".into()),
                label: "u1".into(),
                active: false,
                created_at: chrono::Utc::now(),
                last_used_at: None,
                extra,
            })
            .unwrap();

        persist_official_nickname(
            &registry,
            "kimi",
            &crate::model::AccountId("u1".into()),
            "Forever",
        )
        .unwrap();

        let stored = registry
            .find("kimi", &crate::model::AccountId("u1".into()))
            .unwrap()
            .unwrap();
        assert_eq!(official_nickname_from_extra(&stored.extra), Some("Forever"));
        assert_eq!(
            stored.extra.get("dedup_key").and_then(Value::as_str),
            Some("stable-id")
        );
        assert_eq!(stored.label, "u1");
    }

    fn account(id: &str, label: &str, nickname: Option<&str>) -> crate::model::Account {
        let mut extra = serde_json::Map::new();
        if let Some(nickname) = nickname {
            write_official_nickname_extra(&mut extra, nickname);
        }
        crate::model::Account {
            provider: "kimi".into(),
            id: crate::model::AccountId(id.into()),
            label: label.into(),
            active: false,
            created_at: chrono::Utc::now(),
            last_used_at: None,
            extra,
        }
    }

    #[test]
    fn display_name_prefers_official_nickname() {
        assert_eq!(
            display_name(&account("acc-1", "工作机", Some("Forever"))),
            "Forever"
        );
    }

    #[test]
    fn display_name_uses_alias_when_nickname_unknown() {
        assert_eq!(display_name(&account("acc-1", "工作机", None)), "工作机");
    }

    #[test]
    fn display_name_falls_back_to_id_when_no_alias() {
        assert_eq!(display_name(&account("acc-1", "acc-1", None)), "acc-1");
    }

    #[test]
    fn display_name_treats_alias_equal_to_id_as_id() {
        assert_eq!(display_name(&account("acc-1", "acc-1", None)), "acc-1");
    }
}
