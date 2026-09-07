//! 活跃账号同步：对照墓碑后对齐或入库当前 live 身份。
//!
//! CLI 与 GUI 共用这一份实现。live 不可读或墓碑命中时静默跳过，不走假路径回退。
//! `set_active` / 入库失败记 warn，便于诊断账号库 active 与 live 长期背离。

use std::path::Path;

use crate::account_registry::AccountRegistry;
use crate::error::Result;
use crate::model::{Account, AccountId};
use crate::removed::RemovedAccounts;

/// 同步本地当前账号进账号库。
///
/// `live_account_id` 失败（未登录 / live 不可读）或墓碑命中则跳过。
/// `sync_active_metadata` 负责把 live 对齐或入库；成功后再 `set_active`，
/// 保证新入库的账号会关掉同 provider 其他卡的 active 标记。
pub fn sync_local_active(
    provider_id: &str,
    removed_path: &Path,
    registry: &AccountRegistry,
    live_account_id: impl FnOnce() -> Result<AccountId>,
    sync_active_metadata: impl FnOnce() -> Result<Account>,
) {
    let Ok(id) = live_account_id() else {
        return;
    };
    let removed = RemovedAccounts::load(removed_path);
    if removed.contains(provider_id, &id.0) {
        tracing::debug!(id = %id, "skip tombstoned auto-import");
        return;
    }
    match sync_active_metadata() {
        Ok(account) => {
            if let Err(e) = registry.set_active(provider_id, &account.id) {
                tracing::warn!(err=%e, "skip active marker");
            }
        }
        Err(e) => tracing::warn!(err=%e, "skip auto-import"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::error::Error;

    fn account(id: &str, active: bool) -> Account {
        Account {
            provider: "kimi".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: Utc::now(),
            last_used_at: None,
            extra: Default::default(),
        }
    }

    fn live_ok(id: &str) -> impl FnOnce() -> Result<AccountId> {
        let id = id.to_string();
        move || Ok(AccountId(id))
    }

    fn live_err() -> impl FnOnce() -> Result<AccountId> {
        || Err(Error::provider("no live credentials"))
    }

    #[test]
    fn sync_local_active_skips_tombstoned_live_account() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        let removed_path = tmp.path().join("removed.json");
        RemovedAccounts::load(&removed_path)
            .add("kimi", "alice")
            .unwrap();

        sync_local_active("kimi", &removed_path, &registry, live_ok("alice"), || {
            panic!("tombstone 命中时不得入库")
        });

        assert!(registry.list_by_provider("kimi").unwrap().is_empty());
    }

    #[test]
    fn sync_local_active_skips_when_live_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        let removed_path = tmp.path().join("removed.json");

        sync_local_active("kimi", &removed_path, &registry, live_err(), || {
            panic!("live 不可读时不得入库")
        });

        assert!(registry.list_by_provider("kimi").unwrap().is_empty());
        assert!(!removed_path.exists());
    }

    #[test]
    fn sync_local_active_marks_live_account_active() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        registry.upsert(account("alice", false)).unwrap();
        registry.upsert(account("bob", true)).unwrap();
        let removed_path = tmp.path().join("removed.json");
        let alice = account("alice", false);

        sync_local_active("kimi", &removed_path, &registry, live_ok("alice"), || {
            Ok(alice)
        });

        let listed = registry.list_by_provider("kimi").unwrap();
        let alice = listed.iter().find(|a| a.id.0 == "alice").unwrap();
        let bob = listed.iter().find(|a| a.id.0 == "bob").unwrap();
        assert!(alice.active);
        assert!(!bob.active);
    }

    #[test]
    fn sync_local_active_swallows_metadata_error() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        let removed_path = tmp.path().join("removed.json");

        sync_local_active("kimi", &removed_path, &registry, live_ok("alice"), || {
            Err(Error::provider("read live failed"))
        });

        assert!(registry.list_by_provider("kimi").unwrap().is_empty());
    }

    #[test]
    fn sync_local_active_warns_when_active_marker_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        let removed_path = tmp.path().join("removed.json");
        let ghost = account("ghost", false);

        let events = capture_events(|| {
            sync_local_active("kimi", &removed_path, &registry, live_ok("ghost"), || {
                Ok(ghost)
            });
        });

        assert_eq!(
            event_level(&events, "skip active marker"),
            Some(tracing::Level::WARN),
            "active 标记失败应升到 warn，便于诊断 live 与账号库背离: {events:?}"
        );
        assert!(registry.list_by_provider("kimi").unwrap().is_empty());
    }

    #[test]
    fn sync_local_active_warns_when_auto_import_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        let removed_path = tmp.path().join("removed.json");

        let events = capture_events(|| {
            sync_local_active("kimi", &removed_path, &registry, live_ok("alice"), || {
                Err(Error::provider("read live failed"))
            });
        });

        assert_eq!(
            event_level(&events, "skip auto-import"),
            Some(tracing::Level::WARN),
            "自动入库失败应升到 warn: {events:?}"
        );
        assert!(registry.list_by_provider("kimi").unwrap().is_empty());
    }

    fn event_level(events: &[(tracing::Level, String)], needle: &str) -> Option<tracing::Level> {
        events
            .iter()
            .find_map(|(level, message)| message.contains(needle).then_some(*level))
    }

    fn capture_events(f: impl FnOnce()) -> Vec<(tracing::Level, String)> {
        use std::sync::{Arc, Mutex};

        use tracing::field::{Field, Visit};
        use tracing::subscriber::Interest;
        use tracing::{Event, Metadata, Subscriber};

        struct Capture(Arc<Mutex<Vec<(tracing::Level, String)>>>);

        struct Msg(String);

        impl Visit for Msg {
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "message" {
                    self.0 = value.to_string();
                }
            }

            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" && self.0.is_empty() {
                    self.0 = format!("{value:?}");
                }
            }
        }

        impl Subscriber for Capture {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }

            fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
                Interest::always()
            }

            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }

            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

            fn event(&self, event: &Event<'_>) {
                let mut msg = Msg(String::new());
                event.record(&mut msg);
                self.0
                    .lock()
                    .expect("capture")
                    .push((*event.metadata().level(), msg.0));
            }

            fn enter(&self, _: &tracing::span::Id) {}

            fn exit(&self, _: &tracing::span::Id) {}
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::with_default(Capture(events.clone()), f);
        let captured = events.lock().expect("capture").clone();
        captured
    }
}
