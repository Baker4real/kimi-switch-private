//! 账号摘出自动切换。
//!
//! extra 键 [`EXTRA_AUTO_SWAP_OPT_OUT`] 存 JSON bool：真=摘出，不作为耗尽/回驻候选。
//! 缺键、非 bool、false 均视为未摘出。写侧只动这一键。

use serde_json::{Map, Value};

use crate::account_registry::AccountRegistry;
use crate::error::Result;
use crate::model::{Account, AccountId};

/// extra 键：摘出自动切换（真=不作为耗尽/回驻候选）。
pub const EXTRA_AUTO_SWAP_OPT_OUT: &str = "auto_swap_opt_out";

/// 把摘出标志写入 extra：只动 [`EXTRA_AUTO_SWAP_OPT_OUT`]，其它键原样保留。
pub fn write_auto_swap_opt_out_extra(extra: &mut Map<String, Value>, opted_out: bool) {
    extra.insert(EXTRA_AUTO_SWAP_OPT_OUT.to_string(), Value::Bool(opted_out));
}

/// 读出摘出标志；缺键、非 bool 视为未摘出。
pub fn auto_swap_opt_out_from_extra(extra: &Map<String, Value>) -> bool {
    extra
        .get(EXTRA_AUTO_SWAP_OPT_OUT)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 账号是否已摘出自动切换。
pub fn account_auto_swap_opt_out(account: &Account) -> bool {
    auto_swap_opt_out_from_extra(&account.extra)
}

/// 把摘出标志写回账号 extra：账号不存在则跳过；只动这一键，保留其它 extra。
pub fn persist_auto_swap_opt_out(
    registry: &AccountRegistry,
    provider: &str,
    id: &AccountId,
    opted_out: bool,
) -> Result<()> {
    registry.update_extra(provider, id, |extra| {
        write_auto_swap_opt_out_extra(extra, opted_out);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Account;

    fn extra_with_str(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), Value::String((*value).to_string())))
            .collect()
    }

    fn sample_account(id: &str, extra: Map<String, Value>) -> Account {
        Account {
            provider: "kimi".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active: false,
            created_at: chrono::Utc::now(),
            last_used_at: None,
            extra,
        }
    }

    #[test]
    fn write_only_touches_opt_out_key() {
        let mut extra = extra_with_str(&[
            ("dedup_key", "stable-id"),
            (
                "membership_next_billing_time",
                "2027-08-27T01:25:37.930341Z",
            ),
        ]);
        write_auto_swap_opt_out_extra(&mut extra, true);
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
        assert!(auto_swap_opt_out_from_extra(&extra));
        write_auto_swap_opt_out_extra(&mut extra, false);
        assert!(!auto_swap_opt_out_from_extra(&extra));
        assert_eq!(
            extra.get("dedup_key").and_then(Value::as_str),
            Some("stable-id")
        );
    }

    #[test]
    fn missing_or_non_bool_extra_is_not_opted_out() {
        assert!(!auto_swap_opt_out_from_extra(&Map::new()));
        let mut extra = Map::new();
        extra.insert(EXTRA_AUTO_SWAP_OPT_OUT.into(), Value::String("true".into()));
        assert!(!auto_swap_opt_out_from_extra(&extra));
        extra.insert(EXTRA_AUTO_SWAP_OPT_OUT.into(), Value::Bool(false));
        assert!(!auto_swap_opt_out_from_extra(&extra));
        extra.insert(EXTRA_AUTO_SWAP_OPT_OUT.into(), Value::Bool(true));
        assert!(auto_swap_opt_out_from_extra(&extra));
    }

    #[test]
    fn persist_roundtrips_through_registry_and_keeps_other_extra() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.toml");
        let registry = AccountRegistry::new(path.clone());
        let mut extra = extra_with_str(&[("dedup_key", "stable-id")]);
        write_auto_swap_opt_out_extra(&mut extra, false);
        registry.upsert(sample_account("u1", extra)).unwrap();

        persist_auto_swap_opt_out(&registry, "kimi", &AccountId("u1".into()), true).unwrap();

        let stored = registry
            .find("kimi", &AccountId("u1".into()))
            .unwrap()
            .unwrap();
        assert!(auto_swap_opt_out_from_extra(&stored.extra));
        assert_eq!(
            stored.extra.get("dedup_key").and_then(Value::as_str),
            Some("stable-id")
        );
        assert_eq!(stored.label, "u1");

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("auto_swap_opt_out"),
            "摘出标志应随 registry 落盘：{raw}"
        );

        let reloaded = AccountRegistry::new(path);
        let loaded = reloaded
            .find("kimi", &AccountId("u1".into()))
            .unwrap()
            .unwrap();
        assert!(auto_swap_opt_out_from_extra(&loaded.extra));
        assert_eq!(
            loaded.extra.get("dedup_key").and_then(Value::as_str),
            Some("stable-id")
        );
    }

    /// 手动切换效应：摘出账号仍可被标为当前；extra 原样保留。
    #[test]
    fn set_active_accepts_opted_out_account_and_keeps_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        registry
            .upsert(sample_account("alice", Map::new()))
            .unwrap();
        let mut bob_extra = Map::new();
        write_auto_swap_opt_out_extra(&mut bob_extra, true);
        let mut bob = sample_account("bob", bob_extra);
        bob.active = false;
        registry.upsert(bob).unwrap();

        registry
            .set_active("kimi", &AccountId("bob".into()))
            .unwrap();

        let bob = registry
            .find("kimi", &AccountId("bob".into()))
            .unwrap()
            .unwrap();
        let alice = registry
            .find("kimi", &AccountId("alice".into()))
            .unwrap()
            .unwrap();
        assert!(bob.active);
        assert!(!alice.active);
        assert!(auto_swap_opt_out_from_extra(&bob.extra));
        assert!(account_auto_swap_opt_out(&bob));
    }
}
