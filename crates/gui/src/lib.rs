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
pub use kimi_switch_core::QuotaCollectReason;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use chrono::{Local, TimeZone, Utc};
    use kimi_switch_core::{
        Account, AccountId, Provider, Quota, QuotaCache, QuotaCollectReason, QuotaCollectStats,
        QuotaSnapshot, QuotaState, QuotaStatus, QuotaWindow,
    };
    use kimi_switch_kimi::web_membership::WebBinding;

    use crate::backend::{
        account_for_notice, account_row, account_row_at, attach_skip_message, auth_success_message,
        bind_web_success_message, bind_web_user_message, cached_membership_plan,
        collect_account_rows, delete_success_message, import_success_message,
        reauth_success_message, rename_success_message, swap_failure_message, swap_success_message,
        WebMembershipRefresh,
    };

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
            membership_expiry: None,
            membership_remaining_days: None,
            membership_period_days: None,
            membership_expiry_hint: false,
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
            display_name: "a".into(),
            official_nickname_known: false,
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

        let row = account_row(&account, &snap, None);

        assert!(row.credential_expired);
        assert_eq!(
            row.error.as_deref(),
            Some("凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权")
        );
        assert!(row.quotas.is_empty());
        assert!(!row.is_urgent());
        // 查询失败且无缓存回退：没有额度取得时间，不显示「更新于」误导用户。
        assert!(row.cached_at.is_none());

        // 一般失败（如 429）只是查询失败，不进入凭证待续期。
        let generic = QuotaSnapshot {
            state: QuotaState::Failed(kimi_switch_core::QuotaFailure::http(
                429,
                "429 rate limited",
            )),
            ..snap
        };
        let row = account_row(&account, &generic, None);
        assert!(!row.credential_expired);
        assert_eq!(row.error.as_deref(), Some("429 rate limited"));
        assert!(!row.is_urgent());
        assert!(row.cached_at.is_none());
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

        let row = account_row(&account, &snap, None);

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
        assert_eq!(
            account_row(&account, &fresh, None).cached_at.as_deref(),
            Some(
                format!(
                    "更新于 {}",
                    fetched.with_timezone(&Local).format("%m-%d %H:%M")
                )
                .as_str()
            )
        );
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

        let row = account_row(&account, &snap, None);

        assert_eq!(row.membership.as_deref(), Some("Allegro"));
        assert!(row
            .cached_at
            .as_deref()
            .is_some_and(|text| text.starts_with("更新于 ")));
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
            account_row(&account, &snap("LEVEL_INTERMEDIATE"), None)
                .membership
                .as_deref(),
            Some("Allegretto")
        );
        assert_eq!(
            account_row(&account, &snap("LEVEL_ADVANCED"), None)
                .membership
                .as_deref(),
            Some("Allegro")
        );
    }

    /// 会员到期展示（issue #49）：extra 有到期瞬间 → 行带已格式化的东八区日期；
    /// 档位在但无到期日 → 弱提示；档位缺失 → 保持现状不强加提示。
    #[test]
    fn membership_expiry_display_follows_extra_and_membership() {
        let ready_snap = |level: &str| QuotaSnapshot {
            account_id: AccountId("alice".into()),
            fetched_at: Utc::now(),
            stale: false,
            membership: Some(level.into()),
            state: QuotaState::Ready(vec![sample_quota("alice", None, None)]),
        };

        // extra 带实抓样例瞬间 → 展示 2027-08-27，不给绑定提示。
        let mut bound = sample_account("alice");
        bound.extra.insert(
            "membership_current_end_time".into(),
            serde_json::Value::String("2027-08-28T00:00:00Z".into()),
        );
        bound.extra.insert(
            "membership_next_billing_time".into(),
            serde_json::Value::String("2027-08-27T01:25:37.930341Z".into()),
        );
        let row = account_row(&bound, &ready_snap("LEVEL_INTERMEDIATE"), None);
        assert_eq!(row.membership.as_deref(), Some("Allegretto"));
        assert_eq!(row.membership_expiry.as_deref(), Some("2027-08-27"));
        assert!(!row.membership_expiry_hint);
        assert!(!row.is_urgent());
        assert!(!row.credential_expired);

        // 无到期数据：档位仍来自额度查询，只给弱提示，不产生错误语义。
        let unbound = sample_account("alice");
        let row = account_row(&unbound, &ready_snap("LEVEL_ADVANCED"), None);
        assert_eq!(row.membership.as_deref(), Some("Allegro"));
        assert_eq!(row.membership_expiry, None);
        assert!(row.membership_expiry_hint);
        assert!(!row.is_urgent());
        assert!(row.error.is_none());

        // 档位缺失（额度查询失败）：不强加提示，保持现状。
        let failed = QuotaSnapshot {
            membership: None,
            state: QuotaState::Failed(kimi_switch_core::QuotaFailure::http(
                429,
                "429 rate limited",
            )),
            ..ready_snap("LEVEL_INTERMEDIATE")
        };
        let row = account_row(&unbound, &failed, None);
        assert_eq!(row.membership, None);
        assert_eq!(row.membership_expiry, None);
        assert!(!row.membership_expiry_hint);
    }

    /// 绑定成功后的行变化（issue #51 GUI 接线）：extra 写入到期键 → 该卡到期日
    /// 出现、弱提示消失；额度 / 告急 / 待续期字段不受绑定影响（绑定不是额度错误）。
    #[test]
    fn bound_account_row_shows_expiry_without_error_semantics() {
        let mut bound = sample_account("alice");
        bound.extra.insert(
            "membership_current_end_time".into(),
            serde_json::Value::String("2027-08-28T00:00:00Z".into()),
        );
        bound.extra.insert(
            "membership_next_billing_time".into(),
            serde_json::Value::String("2027-08-27T01:25:37.930341Z".into()),
        );
        let snap = QuotaSnapshot {
            account_id: bound.id.clone(),
            fetched_at: Utc::now(),
            stale: false,
            membership: Some("LEVEL_INTERMEDIATE".into()),
            state: QuotaState::Ready(vec![sample_quota("alice", None, None)]),
        };

        let row = account_row(&bound, &snap, None);

        assert_eq!(row.membership_expiry.as_deref(), Some("2027-08-27"));
        assert!(!row.membership_expiry_hint);
        assert!(row.error.is_none());
        assert!(!row.credential_expired);
        assert!(!row.is_urgent());
        // 未绑定的其他卡不受影响：同快照下的空 extra 行保持弱提示。
        let other = account_row(&sample_account("bob"), &snap, None);
        assert_eq!(other.membership_expiry, None);
        assert!(other.membership_expiry_hint);
    }

    /// bind-web 成功文案（issue #51）：有到期日带日期；订阅无到期瞬间说明
    /// 「暂无到期信息」（卡面回退弱提示，不是失败）。
    #[test]
    fn bind_web_success_message_follows_expiry_text() {
        let with_expiry = bind_web_success_message(&kimi_switch_kimi::web_membership::WebBinding {
            expiry_text: Some("2027-08-27".into()),
            period_days: None,
        });
        assert_eq!(with_expiry.text, "绑定成功，有效期至 2027-08-27");
        assert_eq!(with_expiry.tone, "ok");

        let without = bind_web_success_message(&kimi_switch_kimi::web_membership::WebBinding {
            expiry_text: None,
            period_days: None,
        });
        assert_eq!(without.text, "绑定成功，暂无会员到期信息");
        assert_eq!(without.tone, "ok");
    }

    /// bind-web 失败文案（issue #51）：身份门两类失败按结构化载荷映射为简洁中文，
    /// 不透出英文管道文案（账号 id 对用户无益）。
    #[test]
    fn bind_web_user_message_maps_identity_gate_failures() {
        let mismatch = bind_web_user_message(&kimi_switch_core::Error::bind_web_identity_mismatch(
            "u9", "u1",
        ));
        assert_eq!(mismatch, "网页身份与该账号不一致，未绑定");
        assert!(!mismatch.contains("nothing written"));

        let unidentifiable =
            bind_web_user_message(&kimi_switch_core::Error::bind_web_unidentifiable());
        assert_eq!(unidentifiable, "无法从该 token 辨认网页身份，未绑定");
    }

    /// GetSubscription 的网络 / HTTP / 解析失败（#50 管道里都是 Provider 载荷）
    /// 整条替换为固定中文：即使管道文案意外夹带 token，也不会透到前端。
    #[test]
    fn bind_web_user_message_replaces_membership_fetch_failures() {
        let http = bind_web_user_message(&kimi_switch_core::Error::provider_http(
            500,
            "membership subscription HTTP 500 token=LEAK-MARKER",
        ));
        assert_eq!(http, "会员信息获取失败，请稍后再试");
        assert!(!http.contains("LEAK-MARKER"));

        let network = bind_web_user_message(&kimi_switch_core::Error::provider(
            "membership subscription request failed: tcp connect error",
        ));
        assert_eq!(network, "会员信息获取失败，请稍后再试");
    }

    /// 意外失败（账号已被删 / 库存 IO）：账号不存在给引导文案，其余保留细节。
    #[test]
    fn bind_web_user_message_keeps_detail_for_unexpected_failures() {
        let missing = bind_web_user_message(&kimi_switch_core::Error::AccountNotFound {
            provider: "kimi".into(),
            id: "ghost".into(),
        });
        assert_eq!(missing, "账号不存在，请刷新后再试");

        let store = bind_web_user_message(&kimi_switch_core::Error::Credential(
            "disk unavailable".into(),
        ));
        assert_eq!(store, "绑定失败：credential store: disk unavailable");
    }

    #[tokio::test]
    async fn throttle_window_second_collect_does_not_query() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let accounts = vec![sample_account("alice"), sample_account("bob")];

        let (first, _) = collect_account_rows(
            provider.clone(),
            None,
            &accounts,
            &cache_path,
            QuotaCollectReason::Scheduled,
        )
        .await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(first.len(), 2);
        assert!(first
            .iter()
            .all(|row| row.error.is_none() && !row.quotas.is_empty()));
        assert_eq!(first[0].membership.as_deref(), Some("Allegro"));
        assert!(first.iter().all(|row| {
            row.cached_at
                .as_deref()
                .is_some_and(|text| text.starts_with("更新于 "))
        }));

        let (second, _) = collect_account_rows(
            provider.clone(),
            None,
            &accounts,
            &cache_path,
            QuotaCollectReason::Scheduled,
        )
        .await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(second[0].quotas[0].text, first[0].quotas[0].text);
        assert_eq!(second[0].membership.as_deref(), Some("Allegro"));
        assert!(second.iter().all(|row| {
            row.cached_at
                .as_deref()
                .is_some_and(|text| text.starts_with("更新于 "))
        }));
    }

    #[tokio::test]
    async fn user_initiated_collect_queries_when_success_cache_is_10s_old() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let mut cache = QuotaCache::default();
        cache.set("kimi", "alice", vec![sample_quota("alice", None, None)]);
        cache.age_success("kimi", "alice", chrono::Duration::seconds(10));
        cache.save(&cache_path);

        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let accounts = vec![sample_account("alice")];

        let (rows, _) = collect_account_rows(
            provider.clone(),
            None,
            &accounts,
            &cache_path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(rows[0].error.is_none());
        assert!(!rows[0].quotas.is_empty());
        assert_eq!(rows[0].membership.as_deref(), Some("Allegro"));
    }

    #[tokio::test]
    async fn user_initiated_collect_skips_when_success_cache_is_1s_old() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample_quota("alice", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "alice", chrono::Duration::seconds(1));
        cache.save(&cache_path);

        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let accounts = vec![sample_account("alice")];

        let (rows, _) = collect_account_rows(
            provider.clone(),
            None,
            &accounts,
            &cache_path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(rows[0].error.is_none());
        assert_eq!(rows[0].membership.as_deref(), Some("Allegretto"));
    }

    // --- 会员到期刷新与缓存降级（issue #52）---

    /// 给 extra 放一个到期瞬间（nextBillingTime 优先展示）。
    fn account_with_expiry(id: &str, next_billing: &str) -> Account {
        let mut account = sample_account(id);
        account.extra.insert(
            "membership_next_billing_time".into(),
            serde_json::Value::String(next_billing.into()),
        );
        account
    }

    /// Scheduled（启动加载 / 静默收集 / swap 等操作后再列）两次收集：网页会员
    /// 请求 0 次（#52 验收 1 与 swap 红线——swap / import / 绑定后的再列出
    /// 全部走 Scheduled reason，网页路径只在 UserInitiated 打开）。
    #[tokio::test]
    async fn scheduled_collect_makes_no_web_membership_request() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let membership = Arc::new(MembershipDouble::new([(
            "alice",
            MembershipScript::Fresh(Some("2030-01-02")),
        )]));
        // alice 带缓存到期数据：Scheduled 下行内只有缓存日期，没有刷新语义。
        let accounts = vec![
            account_with_expiry("alice", "2030-06-01T00:00:00Z"),
            sample_account("bob"),
        ];

        for _ in 0..2 {
            let (rows, _) = collect_account_rows(
                provider.clone(),
                Some(membership.clone()),
                &accounts,
                &cache_path,
                QuotaCollectReason::Scheduled,
            )
            .await;
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].membership_expiry.as_deref(), Some("2030-06-01"));
            assert!(!rows[0].membership_expiry_hint);
        }

        assert_eq!(
            membership.calls.load(Ordering::SeqCst),
            0,
            "Scheduled 收集不得发起网页会员请求"
        );
    }

    /// 用户点「刷新」：对已绑定账号请求一次会员到期——缓存日历日未过 / 已过
    /// 两种夹具都请求（触发口径），成功后行内日期用新值且不给提示；未绑定
    /// 账号 0 次请求、维持原弱提示。
    #[tokio::test]
    async fn user_initiated_refreshes_bound_accounts_regardless_of_cached_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        // alice：缓存日期未过；carol：缓存日历日已过；bob：未绑定。
        let accounts = vec![
            account_with_expiry("alice", "2030-06-01T00:00:00Z"),
            sample_account("bob"),
            account_with_expiry("carol", "2026-01-01T00:00:00Z"),
        ];
        let membership = Arc::new(MembershipDouble::new([
            ("alice", MembershipScript::Fresh(Some("2030-01-02"))),
            ("carol", MembershipScript::Fresh(Some("2030-03-04"))),
        ]));

        let (rows, _) = collect_account_rows(
            provider.clone(),
            Some(membership.clone()),
            &accounts,
            &cache_path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        // 只有已绑定的 alice / carol 各请求一次；未绑定的 bob 不请求。
        assert_eq!(membership.calls.load(Ordering::SeqCst), 2);
        assert_eq!(rows[0].membership_expiry.as_deref(), Some("2030-01-02"));
        assert!(!rows[0].membership_expiry_hint);
        assert_eq!(rows[2].membership_expiry.as_deref(), Some("2030-03-04"));
        assert!(!rows[2].membership_expiry_hint);
        // bob：未绑定、无缓存日期 → 原绑定弱提示（档位在）。
        assert_eq!(rows[1].membership_expiry, None);
        assert!(rows[1].membership_expiry_hint);
        // 额度行照旧。
        assert!(rows
            .iter()
            .all(|row| row.error.is_none() && !row.quotas.is_empty()));
    }

    /// 刷新失败（网页 token 401 类）：行内保留 extra 缓存旧日期且再现绑定提示
    /// （日期 + 提示共存的缓存降级），不产生错误语义——不计告急、不置凭证
    /// 待续期、额度行不受影响（#52 验收 3 / 4）。
    #[tokio::test]
    async fn user_initiated_refresh_failure_keeps_cached_expiry_with_hint() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let bound = account_with_expiry("alice", "2027-08-27T01:25:37.930341Z");
        let membership = Arc::new(MembershipDouble::new([(
            "alice",
            MembershipScript::Fail(401),
        )]));

        let (rows, _) = collect_account_rows(
            provider.clone(),
            Some(membership.clone()),
            &[bound],
            &cache_path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(membership.calls.load(Ordering::SeqCst), 1);
        let row = &rows[0];
        assert_eq!(
            row.membership_expiry.as_deref(),
            Some("2027-08-27"),
            "刷新失败保留缓存日期（网页 token 失效不把日期藏起来）"
        );
        assert!(row.membership_expiry_hint, "日期与提示共存");
        assert!(row.error.is_none(), "网页失败不是额度错误");
        assert!(!row.credential_expired);
        assert!(!row.is_urgent());
        assert!(!row.quotas.is_empty(), "额度行不受影响");
    }

    /// 会员刷新挂起（模拟 www.kimi.com 不可达，请求发出后远超 join 预算才
    /// 失败）：collect 必须在 join 预算内返回——行保留 extra 缓存旧日期且
    /// 提示再现（刷新失败降级）、额度行不受影响；且网页请求确实发出过
    /// （挂起也是已绑定的真实请求路径，后台任务不 cancel）。
    #[tokio::test]
    async fn user_initiated_collect_returns_within_join_budget_when_membership_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let bound = account_with_expiry("alice", "2027-08-27T01:25:37.930341Z");
        let membership = Arc::new(MembershipDouble::new([("alice", MembershipScript::Hang)]));

        let started = std::time::Instant::now();
        let (rows, _) = collect_account_rows(
            provider.clone(),
            Some(membership.clone()),
            &[bound],
            &cache_path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        // 预算 2s + 充足余量，且远小于替身的 30s 挂起：证明由预算放行，
        // 而不是等底层请求自己完成 / 超时。
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "collect 应在 join 预算内返回，实际耗时 {elapsed:?}"
        );
        // 请求确实发出（未绑定会是 0 次，挂起不算未发请求）。
        assert_eq!(
            membership.calls.load(Ordering::SeqCst),
            1,
            "挂起的会员请求也应计入网页请求数"
        );
        let row = &rows[0];
        assert_eq!(
            row.membership_expiry.as_deref(),
            Some("2027-08-27"),
            "挂起按刷新失败降级，保留 extra 缓存旧日期"
        );
        assert!(row.membership_expiry_hint, "日期与提示共存");
        assert!(row.error.is_none(), "网页挂起不是额度错误");
        assert!(!row.credential_expired);
        assert!(!row.is_urgent());
        assert!(!row.quotas.is_empty(), "额度行不受影响");
    }

    /// 行级刷新结果映射（account_row 直测）：成功但订阅无到期数据 → 日期清空
    /// + 弱提示（与绑定语义一致）；成功有日期 → 不给提示。
    #[test]
    fn account_row_maps_membership_refresh_outcomes() {
        let mut bound = sample_account("alice");
        bound.extra.insert(
            "membership_next_billing_time".into(),
            serde_json::Value::String("2027-08-27T01:25:37.930341Z".into()),
        );
        let snap = QuotaSnapshot {
            account_id: bound.id.clone(),
            fetched_at: Utc::now(),
            stale: false,
            membership: Some("LEVEL_INTERMEDIATE".into()),
            state: QuotaState::Ready(vec![sample_quota("alice", None, None)]),
        };

        // 刷新成功、订阅无到期数据：extra 旧日期被清，行内无日期 + 弱提示。
        let ok_without_expiry = Ok(WebBinding {
            expiry_text: None,
            period_days: None,
        });
        let row = account_row(&bound, &snap, Some(&ok_without_expiry));
        assert_eq!(row.membership_expiry, None);
        assert!(row.membership_expiry_hint);
        assert!(!row.credential_expired);
        assert!(!row.is_urgent());

        // 刷新成功、有新日期：用新值且不给提示（不等下次列出）。
        let ok_with_expiry = Ok(WebBinding {
            expiry_text: Some("2030-01-02".into()),
            period_days: None,
        });
        let row = account_row(&bound, &snap, Some(&ok_with_expiry));
        assert_eq!(row.membership_expiry.as_deref(), Some("2030-01-02"));
        assert!(!row.membership_expiry_hint);
    }

    #[test]
    fn account_row_carries_display_name_and_nickname_known() {
        let snap = QuotaSnapshot {
            account_id: AccountId("acc-1".into()),
            fetched_at: Utc::now(),
            stale: false,
            membership: None,
            state: QuotaState::Failed(kimi_switch_core::QuotaFailure::other("n/a")),
        };

        let row = account_row(&nicknamed("acc-1", "工作机", "Forever"), &snap, None);
        assert_eq!(row.display_name, "Forever");
        assert!(row.official_nickname_known);
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["display_name"], "Forever");
        assert_eq!(json["official_nickname_known"], true);

        let mut aliased = sample_account("acc-1");
        aliased.label = "工作机".into();
        let row = account_row(&aliased, &snap, None);
        assert_eq!(row.display_name, "工作机");
        assert!(!row.official_nickname_known);
        assert_eq!(
            serde_json::to_value(&row).unwrap()["official_nickname_known"],
            false
        );

        let row = account_row(&sample_account("acc-1"), &snap, None);
        assert_eq!(row.display_name, "acc-1");
        assert!(!row.official_nickname_known);
    }

    /// 剩余天数跟展示日走（#61）：有到期日才有值；刷新失败跟着缓存日期；
    /// 会员到期不改变额度告急。hint 布尔语义保持现测。
    #[test]
    fn account_row_membership_remaining_days_follows_display_day() {
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let ready_snap = |level: &str, used: u64, limit: u64| QuotaSnapshot {
            account_id: AccountId("alice".into()),
            fetched_at: Utc::now(),
            stale: false,
            membership: Some(level.into()),
            state: QuotaState::Ready(vec![Quota {
                used,
                limit,
                ..sample_quota("alice", None, None)
            }]),
        };

        let mut bound = sample_account("alice");
        bound.extra.insert(
            "membership_current_end_time".into(),
            serde_json::Value::String("2027-08-28T00:00:00Z".into()),
        );
        bound.extra.insert(
            "membership_next_billing_time".into(),
            serde_json::Value::String("2027-08-27T01:25:37.930341Z".into()),
        );

        let row = account_row_at(&bound, &ready_snap("LEVEL_INTERMEDIATE", 1, 100), None, now);
        assert_eq!(row.membership_expiry.as_deref(), Some("2027-08-27"));
        assert_eq!(row.membership_remaining_days, Some(360));
        assert!(!row.membership_expiry_hint);
        assert!(!row.is_urgent());

        let unbound = sample_account("alice");
        let row = account_row_at(&unbound, &ready_snap("LEVEL_ADVANCED", 1, 100), None, now);
        assert_eq!(row.membership_expiry, None);
        assert_eq!(row.membership_remaining_days, None);
        assert!(row.membership_expiry_hint);

        let failed: kimi_switch_core::Result<WebBinding> =
            Err(kimi_switch_core::Error::provider("web down"));
        let row = account_row_at(
            &bound,
            &ready_snap("LEVEL_INTERMEDIATE", 1, 100),
            Some(&failed),
            now,
        );
        assert_eq!(row.membership_expiry.as_deref(), Some("2027-08-27"));
        assert_eq!(row.membership_remaining_days, Some(360));
        assert!(row.membership_expiry_hint, "刷新失败 hint 仍亮");
        assert!(!row.is_urgent());

        let expired_now = Utc.with_ymd_and_hms(2027, 8, 30, 0, 0, 0).unwrap();
        let row = account_row_at(
            &bound,
            &ready_snap("LEVEL_INTERMEDIATE", 1, 100),
            None,
            expired_now,
        );
        assert_eq!(row.membership_remaining_days, Some(-3));
        assert!(!row.is_urgent(), "会员过期本身不告急");

        let hot = account_row_at(
            &bound,
            &ready_snap("LEVEL_INTERMEDIATE", 80, 100),
            None,
            expired_now,
        );
        assert_eq!(hot.membership_remaining_days, Some(-3));
        assert!(hot.is_urgent(), "告急仍只看用量窗口");
    }

    fn snapshot_message_of(
        accounts: Vec<AccountRow>,
        reason: QuotaCollectReason,
        stats: &QuotaCollectStats,
    ) -> Snapshot {
        Snapshot {
            account_count: accounts.len() as u32,
            urgent_count: 0,
            current_label: "--".into(),
            accounts,
            theme: ThemeId::Classic,
            proactive_renewal: false,
            message: attach_skip_message(None, reason, stats),
        }
    }

    #[tokio::test]
    async fn user_initiated_snapshot_message_when_all_success_throttled() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample_quota("alice", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "alice", chrono::Duration::seconds(1));
        cache.save(&cache_path);

        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let (rows, stats) = collect_account_rows(
            provider.clone(),
            None,
            &[sample_account("alice")],
            &cache_path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        let snapshot = snapshot_message_of(rows, QuotaCollectReason::UserInitiated, &stats);
        let message = snapshot.message.expect("刚刚更新过");
        assert_eq!(message.text, "刚刚更新过");
        assert_eq!(message.tone, "info");
    }

    #[tokio::test]
    async fn scheduled_snapshot_has_no_skip_toast() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample_quota("alice", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "alice", chrono::Duration::seconds(1));
        cache.save(&cache_path);

        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let (rows, stats) = collect_account_rows(
            provider.clone(),
            None,
            &[sample_account("alice")],
            &cache_path,
            QuotaCollectReason::Scheduled,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        let snapshot = snapshot_message_of(rows, QuotaCollectReason::Scheduled, &stats);
        assert!(snapshot.message.is_none());
    }

    #[tokio::test]
    async fn user_initiated_snapshot_message_when_all_429_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let mut cache = QuotaCache::default();
        cache.set("kimi", "alice", vec![sample_quota("alice", None, None)]);
        cache.age_success("kimi", "alice", chrono::Duration::minutes(5));
        cache.record_failure(
            "kimi",
            "alice",
            &kimi_switch_core::Error::quota_http(429, "429 rate limited"),
        );
        cache.save(&cache_path);

        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let (rows, stats) = collect_account_rows(
            provider.clone(),
            None,
            &[sample_account("alice")],
            &cache_path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(rows[0].quotas[0].text, "1% (1/100)");
        let snapshot = snapshot_message_of(rows, QuotaCollectReason::UserInitiated, &stats);
        let message = snapshot.message.expect("429 提示");
        assert_eq!(message.text, "请求过于频繁，请稍后再试");
        assert_eq!(message.tone, "info");
    }

    #[tokio::test]
    async fn user_initiated_snapshot_message_none_when_only_auth_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let mut cache = QuotaCache::default();
        cache.record_failure(
            "kimi",
            "alice",
            &kimi_switch_core::Error::quota_credential_expired(
                "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
            ),
        );
        cache.save(&cache_path);

        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let (rows, stats) = collect_account_rows(
            provider.clone(),
            None,
            &[sample_account("alice")],
            &cache_path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(rows[0].credential_expired);
        let snapshot = snapshot_message_of(rows, QuotaCollectReason::UserInitiated, &stats);
        assert!(snapshot.message.is_none());
    }

    #[tokio::test]
    async fn user_initiated_snapshot_message_none_when_some_accounts_queried() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("quota_cache.json");
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample_quota("alice", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "alice", chrono::Duration::seconds(10));
        cache.set(
            "kimi",
            "bob",
            vec![sample_quota("bob", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "bob", chrono::Duration::seconds(1));
        cache.save(&cache_path);

        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let (rows, stats) = collect_account_rows(
            provider.clone(),
            None,
            &[sample_account("alice"), sample_account("bob")],
            &cache_path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(rows[0].membership.as_deref(), Some("Allegro"));
        assert_eq!(rows[1].membership.as_deref(), Some("Allegretto"));
        let snapshot = snapshot_message_of(rows, QuotaCollectReason::UserInitiated, &stats);
        assert!(snapshot.message.is_none());
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

    fn nicknamed(id: &str, label: &str, nickname: &str) -> Account {
        let mut account = sample_account(id);
        account.label = label.into();
        kimi_switch_core::write_official_nickname_extra(&mut account.extra, nickname);
        account
    }

    #[test]
    fn import_auth_toast_uses_persisted_official_nickname_not_raw_id() {
        let dir = tempfile::tempdir().unwrap();
        let registry = kimi_switch_core::AccountRegistry::new(dir.path().join("registry.toml"));
        let account = sample_account("acc-1");
        registry.upsert(account.clone()).unwrap();
        assert_eq!(
            import_success_message(&account),
            "已导入账号 acc-1",
            "persist 前内存 Account 仍是裸 id"
        );

        kimi_switch_core::persist_official_nickname(&registry, "kimi", &account.id, "Forever")
            .unwrap();

        let for_toast = account_for_notice(&registry, account);
        assert_eq!(import_success_message(&for_toast), "已导入账号 Forever");
        assert_eq!(
            auth_success_message(&for_toast),
            "授权成功，已添加账号 Forever"
        );
        assert_eq!(
            reauth_success_message(&for_toast),
            "已更新账号 Forever 的登录凭证"
        );
    }

    #[test]
    fn gui_notices_use_display_name_not_raw_id() {
        let named = nicknamed("acc-1", "工作机", "Forever");
        assert_eq!(import_success_message(&named), "已导入账号 Forever");
        assert_eq!(swap_success_message(&named, None), "已切换到 Forever");
        assert_eq!(swap_failure_message(&named), "切换到 Forever 失败");
        assert_eq!(delete_success_message(&named), "已删除 Forever");
        assert_eq!(
            rename_success_message(&named, "新别名"),
            "已把 Forever 重命名为「新别名」"
        );
        assert_eq!(auth_success_message(&named), "授权成功，已添加账号 Forever");
        assert_eq!(
            reauth_success_message(&named),
            "已更新账号 Forever 的登录凭证"
        );

        let aliased = {
            let mut account = sample_account("acc-2");
            account.label = "工作机".into();
            account
        };
        assert_eq!(swap_success_message(&aliased, None), "已切换到 工作机");
        assert_eq!(
            swap_success_message(&sample_account("acc-3"), None),
            "已切换到 acc-3"
        );
    }

    #[test]
    fn swap_success_notice_appends_cached_plan_name() {
        let named = nicknamed("acc-1", "工作机", "Forever");
        assert_eq!(
            swap_success_message(&named, Some("Allegro")),
            "已切换到 Forever（Allegro）"
        );
        assert_eq!(
            swap_success_message(&named, Some("Allegretto")),
            "已切换到 Forever（Allegretto）"
        );
        assert_eq!(swap_success_message(&named, None), "已切换到 Forever");
        assert_eq!(swap_success_message(&named, Some("")), "已切换到 Forever");
    }

    #[test]
    fn cached_membership_plan_reads_local_quota_cache_only() {
        let mut cache = kimi_switch_core::QuotaCache::default();
        cache.set(
            "kimi",
            "acc-1",
            vec![sample_quota("acc-1", Some("LEVEL_ADVANCED"), None)],
        );
        assert_eq!(
            cached_membership_plan(&cache, "acc-1").as_deref(),
            Some("Allegro")
        );
        cache.set(
            "kimi",
            "acc-2",
            vec![sample_quota("acc-2", Some("LEVEL_INTERMEDIATE"), None)],
        );
        assert_eq!(
            cached_membership_plan(&cache, "acc-2").as_deref(),
            Some("Allegretto")
        );
        assert_eq!(cached_membership_plan(&cache, "missing"), None);
        let empty = kimi_switch_core::QuotaCache::default();
        assert_eq!(cached_membership_plan(&empty, "acc-1"), None);
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

    /// 会员刷新替身的脚本（issue #52）：按账号决定 refresh 的返回形态。
    #[derive(Clone)]
    enum MembershipScript {
        /// 已绑定：刷新成功，返回给定的新到期展示日（None = 订阅无到期数据）。
        Fresh(Option<&'static str>),
        /// 已绑定但刷新失败（给定 HTTP 状态，模拟网页 token 401 / 服务 5xx）。
        Fail(u16),
        /// 已绑定但挂起：请求发出后睡 30s（远超 join 预算 2s）才按请求超时
        /// 失败，模拟 www.kimi.com 不可达——用于验证 join 预算放行。
        Hang,
    }

    /// 会员刷新计数 + 脚本化替身（issue #52）：不在脚本里的账号视为未绑定
    /// （返回 None，不发请求）；`calls` 只数脚本化「已绑定」路径——对齐生产
    /// 契约里 `None = 未发请求` 的语义，计数即网页请求次数。
    struct MembershipDouble {
        calls: AtomicUsize,
        scripts: Mutex<HashMap<String, MembershipScript>>,
    }

    impl MembershipDouble {
        fn new(scripts: impl IntoIterator<Item = (&'static str, MembershipScript)>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                scripts: Mutex::new(
                    scripts
                        .into_iter()
                        .map(|(id, script)| (id.to_string(), script))
                        .collect(),
                ),
            }
        }
    }

    #[async_trait::async_trait]
    impl WebMembershipRefresh for MembershipDouble {
        async fn refresh(
            &self,
            target: &AccountId,
        ) -> Option<kimi_switch_core::Result<WebBinding>> {
            // 先把脚本克隆出来再 match：挂起分支含 await，MutexGuard 不得跨
            // await 持有（future 需保持 Send）。
            let script = match self.scripts.lock().unwrap().get(&target.0) {
                Some(script) => script.clone(),
                // 未绑定：不发请求（计数不动）。
                None => return None,
            };
            match script {
                MembershipScript::Fresh(expiry) => {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    Some(Ok(WebBinding {
                        expiry_text: expiry.map(str::to_string),
                        period_days: None,
                    }))
                }
                MembershipScript::Fail(status) => {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    Some(Err(kimi_switch_core::Error::provider_http(
                        status,
                        "membership subscription HTTP failure",
                    )))
                }
                // 挂起：请求已发出（计数），睡 30s 后按请求超时失败。
                MembershipScript::Hang => {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    Some(Err(kimi_switch_core::Error::provider(
                        "membership subscription request timed out",
                    )))
                }
            }
        }
    }
}
