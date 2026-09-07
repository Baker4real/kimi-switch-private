//! 删除账号：墓碑 → 账号库删除 → 凭证（Code blob + 网页会员 field）→ 额度缓存 → 审计。
//!
//! CLI 与 GUI 只调用本操作，不再各自编排。`store_field` 由 provider 自报；
//! 网页会员凭证按 core schema 常量 [`crate::membership_expiry::WEB_STORE_FIELD`]
//! 一并清理（issue #50，ADR-0004）。会员到期 extra 键随 registry 行整体消失，
//! 无需单独清理。

use std::path::Path;

use crate::account_registry::AccountRegistry;
use crate::audit::{AuditEvent, AuditLog};
use crate::error::{Error, Result};
use crate::membership_expiry::WEB_STORE_FIELD;
use crate::model::AccountId;
use crate::quota_cache::QuotaCache;
use crate::removed::RemovedAccounts;
use crate::store::CredentialStore;

/// 删除一个账号，使注册表、墓碑、凭证、额度缓存、审计一致。
///
/// `store_field` 必须是该 provider 自报的凭证字段名（如 runtime 的 `store_field()`），
/// 调用方不得硬编码。凭证删除失败只记 warn，其余步骤成功仍返回 `Ok`（与历史行为一致）。
pub fn remove_account(
    registry: &AccountRegistry,
    store: &dyn CredentialStore,
    store_field: &str,
    removed_path: &Path,
    quota_cache_path: &Path,
    audit: &AuditLog,
    provider_id: &str,
    account_id: &AccountId,
) -> Result<()> {
    // 先确认在库里，避免给幽灵账号写墓碑；写序仍是墓碑先于账号库删除。
    if registry.find(provider_id, account_id)?.is_none() {
        return Err(Error::AccountNotFound {
            provider: provider_id.into(),
            id: account_id.to_string(),
        });
    }

    let mut removed = RemovedAccounts::load(removed_path);
    removed
        .add(provider_id, account_id.0.as_str())
        .map_err(|e| anyhow::anyhow!("写入墓碑失败，账号未删除: {e}"))?;
    registry.remove(provider_id, account_id)?;

    if let Err(e) = store.delete(provider_id, account_id.0.as_str(), store_field) {
        tracing::warn!(err=%e, "credential store delete failed (continuing)");
    }
    // 网页会员凭证（独立 field）：一并清理；从未绑定过时删除幂等 no-op。
    if let Err(e) = store.delete(provider_id, account_id.0.as_str(), WEB_STORE_FIELD) {
        tracing::warn!(err=%e, "web membership credential delete failed (continuing)");
    }

    let mut cache = QuotaCache::load(quota_cache_path);
    cache.remove(provider_id, &account_id.0);
    cache.save(quota_cache_path);

    audit.append(AuditEvent::ok(
        "rm",
        provider_id,
        Some(account_id.0.as_str()),
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::error::Error;
    use crate::model::{Account, Quota, QuotaStatus, QuotaWindow};
    use crate::store::FileStore;

    fn account(id: &str) -> Account {
        Account {
            provider: "kimi".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active: false,
            created_at: Utc::now(),
            last_used_at: None,
            extra: Default::default(),
        }
    }

    fn quota(id: &str) -> Quota {
        Quota {
            provider: "kimi".into(),
            account_id: AccountId(id.into()),
            window: QuotaWindow::FiveHour,
            used: 18,
            limit: 100,
            reset_at: Some(Utc::now() + chrono::Duration::hours(4)),
            status: QuotaStatus::Ok,
            note: None,
            membership: None,
        }
    }

    struct Harness {
        _tmp: tempfile::TempDir,
        registry: AccountRegistry,
        store: FileStore,
        removed_path: std::path::PathBuf,
        cache_path: std::path::PathBuf,
        audit: AuditLog,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
            let store = FileStore::new(tmp.path().join("credentials.json"));
            let removed_path = tmp.path().join("removed.json");
            let cache_path = tmp.path().join("quota_cache.json");
            let audit = AuditLog::new(tmp.path().join("audit.log"));
            Self {
                _tmp: tmp,
                registry,
                store,
                removed_path,
                cache_path,
                audit,
            }
        }

        fn remove(&self, id: &str, store_field: &str) -> Result<()> {
            remove_account(
                &self.registry,
                &self.store,
                store_field,
                &self.removed_path,
                &self.cache_path,
                &self.audit,
                "kimi",
                &AccountId(id.into()),
            )
        }
    }

    #[test]
    fn remove_account_clears_registry_tombstone_credential_cache_and_audit() {
        let h = Harness::new();
        h.registry.upsert(account("alice")).unwrap();
        h.store
            .set("kimi", "alice", "auth_json", r#"{"token":"secret"}"#)
            .unwrap();
        let mut cache = QuotaCache::default();
        cache.set("kimi", "alice", vec![quota("alice")]);
        cache.save(&h.cache_path);

        h.remove("alice", "auth_json").unwrap();

        assert!(h
            .registry
            .find("kimi", &AccountId("alice".into()))
            .unwrap()
            .is_none());
        assert!(RemovedAccounts::load(&h.removed_path).contains("kimi", "alice"));
        assert_eq!(h.store.get("kimi", "alice", "auth_json").unwrap(), None);
        assert!(QuotaCache::load(&h.cache_path)
            .get("kimi", "alice")
            .is_none());

        let line = std::fs::read_to_string(h.audit.path()).unwrap();
        let event: AuditEvent = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(event.action, "rm");
        assert_eq!(event.provider, "kimi");
        assert_eq!(event.account.as_deref(), Some("alice"));
        assert_eq!(event.result, "ok");
    }

    #[test]
    fn remove_account_deletes_only_the_provider_reported_store_field() {
        let h = Harness::new();
        h.registry.upsert(account("alice")).unwrap();
        h.store
            .set("kimi", "alice", "auth_json", "keep-me-not")
            .unwrap();
        h.store
            .set("kimi", "alice", "other_field", "unrelated-field")
            .unwrap();

        h.remove("alice", "auth_json").unwrap();

        assert_eq!(h.store.get("kimi", "alice", "auth_json").unwrap(), None);
        assert_eq!(
            h.store
                .get("kimi", "alice", "other_field")
                .unwrap()
                .as_deref(),
            Some("unrelated-field"),
            "未自报的字段不得被删"
        );
    }

    /// issue #50：删除账号把网页会员 field 一并清掉；会员到期 extra 键随
    /// registry 行整体消失（extra 只存在于 registry.toml，无独立副本）。
    #[test]
    fn remove_account_clears_web_membership_field_and_expiry_extra() {
        use crate::membership_expiry::{
            EXTRA_CURRENT_END_TIME, EXTRA_NEXT_BILLING_TIME, WEB_STORE_FIELD,
        };

        let h = Harness::new();
        let mut bound = account("alice");
        bound.extra.insert(
            EXTRA_CURRENT_END_TIME.into(),
            serde_json::Value::String("2027-08-28T00:00:00Z".into()),
        );
        bound.extra.insert(
            EXTRA_NEXT_BILLING_TIME.into(),
            serde_json::Value::String("2027-08-27T01:25:37.930341Z".into()),
        );
        h.registry.upsert(bound).unwrap();
        h.store
            .set("kimi", "alice", "blob", r#"{"access_token":"A1"}"#)
            .unwrap();
        h.store
            .set("kimi", "alice", WEB_STORE_FIELD, "web-token-value")
            .unwrap();

        h.remove("alice", "blob").unwrap();

        assert_eq!(h.store.get("kimi", "alice", WEB_STORE_FIELD).unwrap(), None);
        assert_eq!(h.store.get("kimi", "alice", "blob").unwrap(), None);
        assert!(
            h.registry
                .find("kimi", &AccountId("alice".into()))
                .unwrap()
                .is_none(),
            "registry 行被删 → 会员到期 extra 键随行消失"
        );
    }

    #[test]
    fn remove_account_missing_leaves_other_state_untouched() {
        let h = Harness::new();
        h.store.set("kimi", "ghost", "auth_json", "orphan").unwrap();
        let mut cache = QuotaCache::default();
        cache.set("kimi", "ghost", vec![quota("ghost")]);
        cache.save(&h.cache_path);

        let err = h.remove("ghost", "auth_json").unwrap_err();
        match err {
            Error::AccountNotFound { provider, id } => {
                assert_eq!(provider, "kimi");
                assert_eq!(id, "ghost");
            }
            other => panic!("expected AccountNotFound, got {other:?}"),
        }

        assert_eq!(
            h.store
                .get("kimi", "ghost", "auth_json")
                .unwrap()
                .as_deref(),
            Some("orphan")
        );
        assert!(!RemovedAccounts::load(&h.removed_path).contains("kimi", "ghost"));
        assert!(QuotaCache::load(&h.cache_path)
            .get("kimi", "ghost")
            .is_some());
        assert!(!h.audit.path().exists());
    }

    /// issue #76：墓碑必须先于账号库删除落盘。墓碑写失败时账号仍在库里，并给出明确报错。
    #[test]
    fn remove_account_keeps_registry_when_tombstone_write_fails() {
        let h = Harness::new();
        h.registry.upsert(account("alice")).unwrap();

        // 把墓碑路径做成目录，写入必然失败。
        std::fs::create_dir_all(&h.removed_path).unwrap();

        let err = h.remove("alice", "auth_json").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("写入墓碑失败") && msg.contains("账号未删除"),
            "墓碑写失败必须有明确报错，got {msg:?}"
        );

        assert!(
            h.registry
                .find("kimi", &AccountId("alice".into()))
                .unwrap()
                .is_some(),
            "墓碑写失败时账号库必须保持不变"
        );
        assert!(!RemovedAccounts::load(&h.removed_path).contains("kimi", "alice"));
        assert!(!h.audit.path().exists());
    }

    /// issue #76：删掉的当前账号有墓碑，默认入口不得再自动导入。
    #[test]
    fn remove_account_prevents_reimport_of_deleted_live_account() {
        let h = Harness::new();
        h.registry.upsert(account("alice")).unwrap();
        h.remove("alice", "auth_json").unwrap();

        crate::sync_active::sync_local_active(
            "kimi",
            &h.removed_path,
            &h.registry,
            || Ok(AccountId("alice".into())),
            || panic!("墓碑命中时不得把已删的当前账号再导入"),
        );

        assert!(h
            .registry
            .find("kimi", &AccountId("alice".into()))
            .unwrap()
            .is_none());
        assert!(RemovedAccounts::load(&h.removed_path).contains("kimi", "alice"));
    }
}
