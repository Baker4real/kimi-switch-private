//! 删除账号：注册表 → 墓碑 → 凭证 → 额度缓存 → 审计。
//!
//! CLI 与 GUI 只调用本操作，不再各自编排。`store_field` 由 provider 自报。

use std::path::Path;

use crate::account_registry::AccountRegistry;
use crate::audit::{AuditEvent, AuditLog};
use crate::error::Result;
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
    registry.remove(provider_id, account_id)?;

    let mut removed = RemovedAccounts::load(removed_path);
    removed.add(provider_id, account_id.0.as_str())?;

    if let Err(e) = store.delete(provider_id, account_id.0.as_str(), store_field) {
        tracing::warn!(err=%e, "credential store delete failed (continuing)");
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
}
