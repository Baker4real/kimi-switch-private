//! GUI 命令层：给 Tauri webview 提供不含凭证的展示 DTO。
//!
//! webview 不得接触 access_token / refresh_token / 凭证 blob。

mod backend;
mod dto;

pub use backend::Backend;
pub use dto::{
    AccountRow, AuthLink, CommandError, QuotaTone, QuotaView, Snapshot, ThemeId, UiMessage,
    URGENT_USED_RATIO,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use chrono::{Local, TimeZone, Utc};
    use kimi_switch_core::{
        Account, AccountId, Provider, Quota, QuotaSnapshot, QuotaState, QuotaStatus, QuotaWindow,
    };

    use crate::backend::{account_row, collect_account_rows};

    #[test]
    fn quota_tone_follows_pressure_thresholds() {
        assert_eq!(QuotaView::tone(Some(0.0)), QuotaTone::Ok);
        assert_eq!(QuotaView::tone(Some(0.49)), QuotaTone::Ok);
        assert_eq!(QuotaView::tone(Some(0.5)), QuotaTone::Warn);
        assert_eq!(QuotaView::tone(Some(0.79)), QuotaTone::Warn);
        assert_eq!(QuotaView::tone(Some(0.8)), QuotaTone::High);
        assert_eq!(QuotaView::tone(Some(0.94)), QuotaTone::High);
        assert_eq!(QuotaView::tone(Some(0.95)), QuotaTone::Full);
        assert_eq!(QuotaView::tone(None), QuotaTone::Ok);
    }

    #[test]
    fn quota_tone_serializes_as_wire_string() {
        assert_eq!(serde_json::to_value(QuotaTone::Ok).unwrap(), "ok");
        assert_eq!(serde_json::to_value(QuotaTone::Warn).unwrap(), "warn");
        assert_eq!(serde_json::to_value(QuotaTone::High).unwrap(), "high");
        assert_eq!(serde_json::to_value(QuotaTone::Full).unwrap(), "full");
    }

    /// 告急新语义（issue #43）：只有用量窗口真实达到 ≥80% 才告急；
    /// 任一额度查询失败（含凭证过期型 401）都不告急。
    #[test]
    fn urgent_only_when_usage_hits_threshold() {
        let quiet = AccountRow {
            id: "a".into(),
            label: "a".into(),
            active: false,
            membership: None,
            quotas: vec![QuotaView {
                window: "5小时".into(),
                ratio: Some(0.79),
                text: "79%".into(),
                reset: None,
                tone: QuotaTone::Warn,
            }],
            error: None,
            credential_expired: false,
            cached_at: None,
        };
        assert!(!quiet.is_urgent());

        let hot = AccountRow {
            quotas: vec![QuotaView {
                window: "7天".into(),
                ratio: Some(0.8),
                text: "80%".into(),
                reset: None,
                tone: QuotaTone::High,
            }],
            ..quiet.clone()
        };
        assert!(hot.is_urgent());

        // 一般查询失败：不告急。
        let failed = AccountRow {
            quotas: vec![],
            error: Some("额度查询失败".into()),
            ..quiet.clone()
        };
        assert!(!failed.is_urgent());

        // 凭证过期型 401 → 凭证待续期：同样不告急。
        let pending = AccountRow {
            quotas: vec![],
            error: Some("凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权".into()),
            credential_expired: true,
            ..quiet
        };
        assert!(!pending.is_urgent());
    }

    /// 凭证过期型 401 经类型化 kind 映射为「凭证待续期」行，不靠嗅探文案（issue #43）。
    #[test]
    fn credential_expired_maps_to_pending_renewal_row() {
        let account = sample_account("alice");
        let snap = QuotaSnapshot {
            account_id: account.id.clone(),
            fetched_at: Utc::now(),
            stale: false,
            membership: None,
            state: QuotaState::Failed(kimi_switch_core::QuotaFailure::credential_expired(
                "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
            )),
        };

        let row = account_row(&account, &snap);

        assert!(row.credential_expired);
        assert_eq!(
            row.error.as_deref(),
            Some("凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权")
        );
        assert!(row.quotas.is_empty());
        assert!(!row.is_urgent());

        // 一般失败（如 429）只是查询失败，不进入凭证待续期。
        let generic = QuotaSnapshot {
            state: QuotaState::Failed(kimi_switch_core::QuotaFailure::http(
                429,
                "429 rate limited",
            )),
            ..snap
        };
        let row = account_row(&account, &generic);
        assert!(!row.credential_expired);
        assert_eq!(row.error.as_deref(), Some("429 rate limited"));
        assert!(!row.is_urgent());
    }

    #[test]
    fn theme_id_only_classic_or_jade_night() {
        assert_eq!(ThemeId::parse("classic"), ThemeId::Classic);
        assert_eq!(ThemeId::parse("jade-night"), ThemeId::JadeNight);
        assert_eq!(ThemeId::parse("mica"), ThemeId::Classic);
        assert_eq!(ThemeId::parse(""), ThemeId::Classic);
    }

    #[test]
    fn command_error_is_serde_tagged_kind_and_message() {
        let json = serde_json::to_value(CommandError::Cancelled("授权已取消".into())).unwrap();
        assert_eq!(json["kind"], "cancelled");
        assert_eq!(json["message"], "授权已取消");
    }

    #[test]
    fn command_error_kind_comes_from_core_error_not_message_text() {
        let err = anyhow::Error::from(kimi_switch_core::Error::cancelled(
            "wording that never mentions cancel",
        ))
        .context("授权未完成");
        let cmd = CommandError::from_anyhow(err);
        assert!(matches!(cmd, CommandError::Cancelled(_)));
        assert!(cmd.message().contains("授权未完成"));
    }

    #[test]
    fn command_error_maps_no_pending_auth_kind() {
        let cmd = CommandError::from_anyhow(kimi_switch_core::Error::no_pending_auth().into());
        assert!(matches!(cmd, CommandError::NoPendingAuth(_)));
    }

    #[test]
    fn command_error_maps_reauth_identity_mismatch_from_enum() {
        let cmd = CommandError::from_anyhow(
            kimi_switch_core::Error::reauth_identity_mismatch("u9", "u1").into(),
        );
        assert!(matches!(cmd, CommandError::ReauthIdentityMismatch(_)));
        assert_eq!(
            cmd.message(),
            "登录的是账号 u9，不是这张卡（u1），已拒绝更新凭证"
        );
    }

    #[test]
    fn command_error_maps_reauth_unidentifiable_from_enum() {
        let cmd =
            CommandError::from_anyhow(kimi_switch_core::Error::reauth_unidentifiable().into());
        assert!(matches!(cmd, CommandError::ReauthUnidentifiable(_)));
        assert_eq!(cmd.message(), "无法从授权结果辨认官方账号，未更新这张卡");
    }

    #[test]
    fn command_error_maps_reauth_live_write_failed_from_enum() {
        let cmd = CommandError::from_anyhow(
            kimi_switch_core::Error::reauth_live_write_failed("os error 5").into(),
        );
        assert!(matches!(cmd, CommandError::ReauthLiveWriteFailed(_)));
        assert!(cmd.message().contains("已保存到账号库"));
        assert!(cmd.message().contains("旧凭证"));
        assert!(cmd.message().contains("重试重新授权"));
        assert!(cmd.message().contains("os error 5"));
        assert!(!cmd.message().contains("reauth:"));
    }

    #[test]
    fn stale_row_carries_fetched_at() {
        let fetched = Utc.with_ymd_and_hms(2026, 8, 28, 6, 32, 0).unwrap();
        let account = sample_account("alice");
        let snap = QuotaSnapshot {
            account_id: account.id.clone(),
            fetched_at: fetched,
            stale: true,
            membership: None,
            state: QuotaState::Cached(vec![sample_quota("alice", None, None)]),
        };

        let row = account_row(&account, &snap);

        assert!(row.error.is_none());
        assert!(!row.quotas.is_empty());
        assert_eq!(
            row.cached_at.as_deref(),
            Some(
                format!(
                    "缓存于 {}",
                    fetched.with_timezone(&Local).format("%m-%d %H:%M")
                )
                .as_str()
            )
        );

        let fresh = QuotaSnapshot {
            stale: false,
            ..snap
        };
        assert!(account_row(&account, &fresh).cached_at.is_none());
    }

    #[test]
    fn membership_comes_from_first_class_field() {
        let account = sample_account("alice");
        let snap = QuotaSnapshot {
            account_id: account.id.clone(),
            fetched_at: Utc::now(),
            stale: false,
            membership: Some("LEVEL_ADVANCED".into()),
            state: QuotaState::Ready(vec![sample_quota(
                "alice",
                None,
                Some("LEVEL_INTERMEDIATE"),
            )]),
        };

        let row = account_row(&account, &snap);

        assert_eq!(row.membership.as_deref(), Some("Allegro"));
        assert!(row.cached_at.is_none());
    }

    #[test]
    fn membership_uses_official_kimi_plan_names() {
        let account = sample_account("alice");
        let snap = |level: &str| QuotaSnapshot {
            account_id: account.id.clone(),
            fetched_at: Utc::now(),
            stale: false,
            membership: Some(level.into()),
            state: QuotaState::Ready(vec![sample_quota("alice", None, None)]),
        };

        assert_eq!(
            account_row(&account, &snap("LEVEL_INTERMEDIATE"))
                .membership
                .as_deref(),
            Some("Allegretto")
        );
        assert_eq!(
            account_row(&account, &snap("LEVEL_ADVANCED"))
                .membership
                .as_deref(),
            Some("Allegro")
        );
    }

    #[tokio::test]
    async fn throttle_window_second_collect_does_not_query() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let accounts = vec![sample_account("alice"), sample_account("bob")];

        let first = collect_account_rows(provider.clone(), &accounts, &cache_path).await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(first.len(), 2);
        assert!(first
            .iter()
            .all(|row| row.error.is_none() && !row.quotas.is_empty()));
        assert_eq!(first[0].membership.as_deref(), Some("Allegro"));
        assert!(first.iter().all(|row| row.cached_at.is_none()));

        let second = collect_account_rows(provider.clone(), &accounts, &cache_path).await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(second[0].quotas[0].text, first[0].quotas[0].text);
        assert_eq!(second[0].membership.as_deref(), Some("Allegro"));
        assert!(second.iter().all(|row| row.cached_at.is_none()));
    }

    fn sample_account(id: &str) -> Account {
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

    fn sample_quota(id: &str, membership: Option<&str>, note: Option<&str>) -> Quota {
        Quota {
            provider: "kimi".into(),
            account_id: AccountId(id.into()),
            window: QuotaWindow::FiveHour,
            used: 1,
            limit: 100,
            reset_at: Some(Utc::now() + chrono::Duration::hours(4)),
            status: QuotaStatus::Ok,
            note: note.map(str::to_string),
            membership: membership.map(str::to_string),
        }
    }

    struct CountingProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for CountingProvider {
        async fn activate(&self, _id: &AccountId) -> kimi_switch_core::Result<()> {
            Ok(())
        }

        async fn query_quota(&self, id: &AccountId) -> kimi_switch_core::Result<Vec<Quota>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![sample_quota(
                &id.0,
                Some("LEVEL_ADVANCED"),
                Some("LEVEL_INTERMEDIATE"),
            )])
        }
    }
}
