//! Kimi usage 查询：GET {base}/usages。数值为字符串、reset 为 ISO8601。
//! usage → 7d 窗口；limits[].window(duration+timeUnit) 换算分钟：300→5h、10080→7d、其余 Custom。

use chrono::{DateTime, Utc};
use kimi_switch_core::error::{Error, Result};
use kimi_switch_core::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};

use crate::kimi_files;

fn base_url() -> String {
    std::env::var("KIMI_CODE_BASE_URL")
        .unwrap_or_else(|_| "https://api.kimi.com/coding/v1".into())
        .trim_end_matches('/')
        .to_string()
}

fn to_u64(v: Option<&serde_json::Value>) -> Option<u64> {
    let v = v?;
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    v.as_str().and_then(|s| s.parse().ok())
}

fn reset_at(detail: &serde_json::Value) -> Option<DateTime<Utc>> {
    let s = detail.get("resetTime")?.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn window_from_minutes(minutes: u64) -> QuotaWindow {
    match minutes {
        300 => QuotaWindow::FiveHour,
        10_080 => QuotaWindow::SevenDay,
        _ => QuotaWindow::Custom,
    }
}

fn minutes_of(window: &serde_json::Value) -> Option<u64> {
    let duration = to_u64(window.get("duration"))?;
    let unit = window
        .get("timeUnit")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mins = match unit {
        u if u.contains("MINUTE") => duration,
        u if u.contains("HOUR") => duration * 60,
        u if u.contains("DAY") => duration * 60 * 24,
        _ => duration,
    };
    Some(mins)
}

fn quota_from(
    detail: &serde_json::Value,
    window: QuotaWindow,
    provider: &str,
    id: &AccountId,
    membership: Option<&str>,
) -> Option<Quota> {
    let limit = to_u64(detail.get("limit"))?;
    let used = to_u64(detail.get("used")).or_else(|| {
        let rem = to_u64(detail.get("remaining"))?;
        Some(limit.saturating_sub(rem))
    })?;
    let pct = if limit > 0 {
        used as f64 / limit as f64 * 100.0
    } else {
        0.0
    };
    let membership = membership.map(String::from);
    Some(Quota {
        provider: provider.into(),
        account_id: id.clone(),
        window,
        used,
        limit,
        reset_at: reset_at(detail),
        status: QuotaStatus::from_percent(pct),
        // GUI 过渡仍从 note 读会员等级；#27 改读一等字段后可停双写。
        note: membership.clone(),
        membership,
    })
}

/// 从 `/me` JSON 抽取官方昵称。只读 `nickname` 字符串；缺字段、空串、非字符串都视为未知。
pub fn parse_official_nickname(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let raw = value.get("nickname").and_then(|item| item.as_str())?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `/me` 独立短超时：对齐会员 join 预算，远小于 20s 额度超时。超时 = 未知。
const OFFICIAL_NICKNAME_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

async fn fetch_official_nickname(access_token: &str, api_base: &str) -> Option<String> {
    let url = format!("{}/me", api_base.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(OFFICIAL_NICKNAME_HTTP_TIMEOUT)
        .build()
        .ok()?;
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "kimi-switch")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    parse_official_nickname(&body)
}

/// 额度查询或导入成功后顺带缓存官方昵称。`/me` 失败不改变 extra、不失败主流程。
async fn cache_official_nickname(
    access_token: &str,
    account_id: &AccountId,
    api_base: &str,
    registry: Option<&kimi_switch_core::AccountRegistry>,
) {
    let Some(registry) = registry else {
        return;
    };
    let Some(nickname) = fetch_official_nickname(access_token, api_base).await else {
        return;
    };
    let _ = kimi_switch_core::persist_official_nickname(registry, "kimi", account_id, &nickname);
}

/// 导入/授权拿到 blob 后若能取出 access_token，顺带缓存官方昵称。失败忽略。
pub async fn cache_official_nickname_from_blob(
    registry: &kimi_switch_core::AccountRegistry,
    account_id: &AccountId,
    blob: &str,
) {
    let Some(access_token) = kimi_switch_engine::extract_access_token(blob) else {
        return;
    };
    cache_official_nickname(&access_token, account_id, &base_url(), Some(registry)).await;
}

/// 解析 /usages 响应为多窗口 Quota。会员等级写入 `Quota.membership`，并双写 `note`。
pub fn parse_usages(body: &str, provider: &str, id: &AccountId) -> Vec<Quota> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let membership = v.pointer("/user/membership/level").and_then(|x| x.as_str());
    let mut out = Vec::new();
    if let Some(usage) = v.get("usage") {
        if let Some(q) = quota_from(usage, QuotaWindow::SevenDay, provider, id, membership) {
            out.push(q);
        }
    }
    if let Some(limits) = v.get("limits").and_then(|x| x.as_array()) {
        for item in limits {
            let window = item
                .get("window")
                .and_then(minutes_of)
                .map(window_from_minutes)
                .unwrap_or(QuotaWindow::Custom);
            let detail = item.get("detail").unwrap_or(item);
            if let Some(q) = quota_from(detail, window, provider, id, membership) {
                out.push(q);
            }
        }
    }
    out
}

/// active 账号查询 401 时，按官方锁协议恢复一次令牌后只重试一次。
/// 协调恢复不可用或已放弃、且 attempted access token 已过 JWT exp 时，
/// 该 401 归入「凭证过期型 401」（见 `classify_unrecovered_401`），其余失败原样冒出。
pub async fn fetch_quota_with_active_recovery(
    access_token: &str,
    account: &Account,
    registry: &kimi_switch_core::AccountRegistry,
) -> Result<Vec<Quota>> {
    let api_base = base_url();
    fetch_quota_with_active_recovery_at(
        access_token,
        account,
        &api_base,
        &crate::paths::kimi_home(),
        None,
        Some(registry),
    )
    .await
}

async fn fetch_quota_with_active_recovery_at(
    access_token: &str,
    account: &Account,
    api_base: &str,
    home: &std::path::Path,
    test_recovery: Option<(&str, crate::oauth::RefreshLockProtocol)>,
    registry: Option<&kimi_switch_core::AccountRegistry>,
) -> Result<Vec<Quota>> {
    let (quotas, token_for_me) = match fetch_quota_at(access_token, account, api_base).await {
        Err(error) if account.active && error.http_status() == Some(401) => {
            let recovered = if let Some((oauth_base, protocol)) = test_recovery {
                crate::oauth::recover_active_401_at(
                    access_token,
                    account,
                    home,
                    oauth_base,
                    protocol,
                )
                .await?
            } else {
                crate::oauth::recover_active_401(access_token, account).await?
            };
            let Some(fresh_access) = recovered else {
                return Err(classify_unrecovered_401(error, access_token));
            };
            (
                fetch_quota_at(&fresh_access, account, api_base).await?,
                fresh_access,
            )
        }
        Ok(quotas) => (quotas, access_token.to_string()),
        Err(error) => return Err(error),
    };
    if let Some(registry) = registry {
        let token_for_me = token_for_me;
        let account_id = account.id.clone();
        let api_base = api_base.to_string();
        let registry = kimi_switch_core::AccountRegistry::new(registry.path().clone());
        tokio::spawn(async move {
            cache_official_nickname(&token_for_me, &account_id, &api_base, Some(&registry)).await;
        });
    }
    Ok(quotas)
}

/// 「凭证过期型 401」的引导文案（issue #41）：卡片用它替代裸 401，给出可行动的恢复路径。
/// 主动续期被拒（issue #44）复用同一文案，不新造。
pub(crate) const CREDENTIAL_EXPIRED_GUIDANCE: &str =
    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权";

/// 协调恢复不可用或已放弃后的 401 归类：attempted access token 按 JWT exp 确证已过期 →
/// 「凭证过期型 401」（引导文案替代裸 401）；无法确证过期 → 原样返回，类别与文案不变。
fn classify_unrecovered_401(error: Error, attempted_access_token: &str) -> Error {
    if error.http_status() == Some(401) && kimi_files::jwt_expired(attempted_access_token) {
        return Error::quota_credential_expired(CREDENTIAL_EXPIRED_GUIDANCE);
    }
    error
}

async fn fetch_quota_at(
    access_token: &str,
    account: &Account,
    api_base: &str,
) -> Result<Vec<Quota>> {
    let url = format!("{}/usages", api_base.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "kimi-switch")
        .send()
        .await
        .map_err(|e| Error::quota_network(format!("kimi usages request failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::quota_http(
            status.as_u16(),
            format!("kimi usages HTTP {status}: {body}"),
        ));
    }
    Ok(parse_usages(&body, "kimi", &account.id))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::test_support::MockServer;
    use kimi_switch_core::error::QuotaFailureKind;

    const REAL: &str = r#"{
      "usage": {"limit":"100","used":"4","remaining":"96","resetTime":"2026-07-24T07:52:15Z"},
      "limits": [{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
                  "detail":{"limit":"100","used":"18","remaining":"82","resetTime":"2026-07-17T12:52:15Z"}}]
    }"#;

    async fn wait_received(server: &MockServer, n: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if server.received() >= n {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timeout waiting for {n} mock requests, got {}", server.received());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn wait_official_nickname(
        registry: &kimi_switch_core::AccountRegistry,
        expected: Option<&str>,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let stored = registry
                .find("kimi", &AccountId("u-123".into()))
                .unwrap()
                .unwrap();
            if kimi_switch_core::official_nickname_from_extra(&stored.extra) == expected {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "timeout waiting for official nickname {expected:?}, got {:?}",
                    kimi_switch_core::official_nickname_from_extra(&stored.extra)
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn parse_official_nickname_reads_top_level_nickname() {
        let body = r#"{"nickname":"Forever","userId":"u1"}"#;
        assert_eq!(parse_official_nickname(body).as_deref(), Some("Forever"));
    }

    #[test]
    fn parse_official_nickname_missing_or_blank_is_unknown() {
        assert_eq!(parse_official_nickname(r#"{"userId":"u1"}"#), None);
        assert_eq!(parse_official_nickname(r#"{"nickname":""}"#), None);
        assert_eq!(parse_official_nickname(r#"{"nickname":"   "}"#), None);
        assert_eq!(parse_official_nickname(r#"{"nickname":1}"#), None);
        assert_eq!(parse_official_nickname("not-json"), None);
    }

    #[test]
    fn parse_official_nickname_ignores_other_profile_fields() {
        let body = r#"{"nickname":"Forever","phone":"+86","email":"a@b.c","avatar":"http://x","user_level_name":"Allegro"}"#;
        assert_eq!(parse_official_nickname(body).as_deref(), Some("Forever"));
    }

    #[tokio::test]
    async fn usages_success_returns_quota_even_when_me_hangs() {
        let temporary = tempfile::tempdir().unwrap();
        let registry =
            kimi_switch_core::AccountRegistry::new(temporary.path().join("registry.toml"));
        registry.upsert(active_account()).unwrap();
        let usages = r#"{"usage":{"limit":"100","used":"4","remaining":"96","resetTime":"2026-07-24T07:52:15Z"}}"#;
        let server = MockServer::start_then_hang(vec![("200 OK", usages)]);

        let started = std::time::Instant::now();
        let quotas = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            fetch_quota_with_active_recovery_at(
                "tok",
                &active_account(),
                server.base_url(),
                temporary.path(),
                None,
                Some(&registry),
            ),
        )
        .await
        .expect("/usages 成功后不得因 /me 挂起而丢掉额度")
        .unwrap();

        assert_eq!(quotas[0].used, 4);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "/usages 成功必须立刻交回额度，不得等 /me：{:?}",
            started.elapsed()
        );
        assert_eq!(
            kimi_switch_core::official_nickname_from_extra(
                &registry
                    .find("kimi", &AccountId("u-123".into()))
                    .unwrap()
                    .unwrap()
                    .extra
            ),
            None,
            "挂起的 /me 不得写入昵称"
        );
    }

    #[tokio::test]
    async fn me_timeout_keeps_old_nickname_and_does_not_hang() {
        let temporary = tempfile::tempdir().unwrap();
        let registry =
            kimi_switch_core::AccountRegistry::new(temporary.path().join("registry.toml"));
        let mut seeded = active_account();
        kimi_switch_core::write_official_nickname_extra(&mut seeded.extra, "OldName");
        registry.upsert(seeded).unwrap();
        let server = MockServer::start_then_hang(vec![]);

        tokio::time::timeout(
            std::time::Duration::from_secs(4),
            cache_official_nickname(
                "tok",
                &AccountId("u-123".into()),
                server.base_url(),
                Some(&registry),
            ),
        )
        .await
        .expect("/me 必须有短超时，不得挂起导入/授权");

        let stored = registry
            .find("kimi", &AccountId("u-123".into()))
            .unwrap()
            .unwrap();
        assert_eq!(
            kimi_switch_core::official_nickname_from_extra(&stored.extra),
            Some("OldName")
        );
    }

    #[tokio::test]
    async fn quota_success_caches_official_nickname_from_me() {
        let temporary = tempfile::tempdir().unwrap();
        let registry =
            kimi_switch_core::AccountRegistry::new(temporary.path().join("registry.toml"));
        registry.upsert(active_account()).unwrap();
        let server = MockServer::start(vec![
            (
                "200 OK",
                r#"{"usage":{"limit":"100","used":"4","remaining":"96","resetTime":"2026-07-24T07:52:15Z"}}"#,
            ),
            ("200 OK", r#"{"nickname":"Forever","phone":"+86"}"#),
        ]);

        let quotas = fetch_quota_with_active_recovery_at(
            "tok",
            &active_account(),
            server.base_url(),
            temporary.path(),
            None,
            Some(&registry),
        )
        .await
        .unwrap();

        assert_eq!(quotas[0].used, 4);
        wait_received(&server, 2).await;
        wait_official_nickname(&registry, Some("Forever")).await;
        let stored = registry
            .find("kimi", &AccountId("u-123".into()))
            .unwrap()
            .unwrap();
        assert!(stored.extra.get("phone").is_none());
        assert_eq!(
            server.finish(),
            vec!["GET /usages HTTP/1.1", "GET /me HTTP/1.1"]
        );
    }

    #[tokio::test]
    async fn me_failure_keeps_old_nickname_and_does_not_fail_quota() {
        let temporary = tempfile::tempdir().unwrap();
        let registry =
            kimi_switch_core::AccountRegistry::new(temporary.path().join("registry.toml"));
        let mut seeded = active_account();
        kimi_switch_core::write_official_nickname_extra(&mut seeded.extra, "OldName");
        registry.upsert(seeded).unwrap();
        let server = MockServer::start(vec![
            (
                "200 OK",
                r#"{"usage":{"limit":"100","used":"4","remaining":"96","resetTime":"2026-07-24T07:52:15Z"}}"#,
            ),
            ("500 Internal Server Error", r#"{"error":"boom"}"#),
        ]);

        let quotas = fetch_quota_with_active_recovery_at(
            "tok",
            &active_account(),
            server.base_url(),
            temporary.path(),
            None,
            Some(&registry),
        )
        .await
        .unwrap();

        assert_eq!(quotas[0].used, 4);
        wait_received(&server, 2).await;
        wait_official_nickname(&registry, Some("OldName")).await;
        assert_eq!(
            server.finish(),
            vec!["GET /usages HTTP/1.1", "GET /me HTTP/1.1"]
        );
    }

    #[tokio::test]
    async fn recovered_quota_uses_fresh_access_token_for_me() {
        let temporary = tempfile::tempdir().unwrap();
        let credentials = crate::paths::active_cred_path(temporary.path());
        std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        let old = credential_blob("OLD", "R1");
        let old_access = extract_json_string(&old, "access_token");
        std::fs::write(&credentials, &old).unwrap();
        let registry =
            kimi_switch_core::AccountRegistry::new(temporary.path().join("registry.toml"));
        registry.upsert(active_account()).unwrap();
        let server = MockServer::start(vec![
            ("401 Unauthorized", r#"{"error":"expired"}"#),
            (
                "200 OK",
                r#"{"access_token":"NEW","refresh_token":"R2","expires_in":3600,"scope":"kimi-code","token_type":"Bearer"}"#,
            ),
            (
                "200 OK",
                r#"{"usage":{"limit":"100","used":"7","remaining":"93","resetTime":"2026-07-24T07:52:15Z"}}"#,
            ),
            ("200 OK", r#"{"nickname":"Forever"}"#),
        ]);

        fetch_quota_with_active_recovery_at(
            &old_access,
            &active_account(),
            server.base_url(),
            temporary.path(),
            Some((
                server.base_url(),
                crate::oauth::RefreshLockProtocol::TypeScriptDirectory,
            )),
            Some(&registry),
        )
        .await
        .unwrap();

        wait_received(&server, 4).await;
        wait_official_nickname(&registry, Some("Forever")).await;
        let requests = server.finish_full();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].starts_with("GET /usages HTTP/1.1"));
        assert!(requests[1].starts_with("POST /api/oauth/token HTTP/1.1"));
        assert!(requests[2].starts_with("GET /usages HTTP/1.1"));
        assert!(requests[3].starts_with("GET /me HTTP/1.1"));
        let me = requests[3].to_ascii_lowercase();
        assert!(
            me.contains("authorization: bearer new"),
            "恢复成功后 /me 必须用新 access token"
        );
        assert!(
            !requests[3].contains(&old_access),
            "/me 不得使用恢复前的 access token"
        );
        assert!(
            me.contains("user-agent: kimi-switch"),
            "/me 须带 kimi-switch UA"
        );
    }

    #[test]
    fn parses_weekly_and_5h() {
        let q = parse_usages(REAL, "kimi", &AccountId("u1".into()));
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].window, QuotaWindow::SevenDay);
        assert_eq!((q[0].used, q[0].limit), (4, 100));
        assert_eq!(q[1].window, QuotaWindow::FiveHour);
        assert_eq!((q[1].used, q[1].limit), (18, 100));
        assert!(q[1].reset_at.is_some());
    }

    #[test]
    fn used_derived_from_remaining_when_absent() {
        let body =
            r#"{"usage":{"limit":"100","remaining":"70","resetTime":"2026-07-24T07:52:15Z"}}"#;
        let q = parse_usages(body, "kimi", &AccountId("u1".into()));
        assert_eq!((q[0].used, q[0].limit), (30, 100));
        assert_eq!(q[0].note, None);
    }

    #[test]
    fn membership_level_rides_on_note() {
        let body = r#"{"user":{"userId":"u1","membership":{"level":"LEVEL_INTERMEDIATE"}},
            "usage":{"limit":"100","used":"4","remaining":"96","resetTime":"2026-07-24T07:52:15Z"}}"#;
        let q = parse_usages(body, "kimi", &AccountId("u1".into()));
        assert_eq!(q[0].note.as_deref(), Some("LEVEL_INTERMEDIATE"));
        assert_eq!(q[0].membership.as_deref(), Some("LEVEL_INTERMEDIATE"));
    }

    #[tokio::test]
    async fn active_401_refreshes_once_persists_and_retries_usage_once() {
        let temporary = tempfile::tempdir().unwrap();
        let credentials = crate::paths::active_cred_path(temporary.path());
        std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        let old = credential_blob("OLD", "R1");
        let old_access = extract_json_string(&old, "access_token");
        std::fs::write(&credentials, &old).unwrap();
        let server = MockServer::start(vec![
            ("401 Unauthorized", r#"{"error":"expired"}"#),
            (
                "200 OK",
                r#"{"access_token":"NEW","refresh_token":"R2","expires_in":3600,"scope":"kimi-code","token_type":"Bearer"}"#,
            ),
            (
                "200 OK",
                r#"{"usage":{"limit":"100","used":"7","remaining":"93","resetTime":"2026-07-24T07:52:15Z"}}"#,
            ),
        ]);
        let quotas = fetch_quota_with_active_recovery_at(
            &old_access,
            &active_account(),
            server.base_url(),
            temporary.path(),
            Some((
                server.base_url(),
                crate::oauth::RefreshLockProtocol::TypeScriptDirectory,
            )),
            None,
        )
        .await
        .unwrap();

        assert_eq!(quotas[0].used, 7);
        let saved = std::fs::read_to_string(credentials).unwrap();
        assert_eq!(extract_json_string(&saved, "access_token"), "NEW");
        assert_eq!(extract_json_string(&saved, "refresh_token"), "R2");
        assert_eq!(
            server.finish(),
            vec![
                "GET /usages HTTP/1.1",
                "POST /api/oauth/token HTTP/1.1",
                "GET /usages HTTP/1.1"
            ]
        );
    }

    #[tokio::test]
    async fn rejected_refresh_does_not_overwrite_live_credentials() {
        let temporary = tempfile::tempdir().unwrap();
        let credentials = crate::paths::active_cred_path(temporary.path());
        std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        let old = credential_blob("OLD", "R1");
        let old_access = extract_json_string(&old, "access_token");
        std::fs::write(&credentials, &old).unwrap();
        let server = MockServer::start(vec![
            ("401 Unauthorized", r#"{"error":"expired"}"#),
            ("401 Unauthorized", r#"{"error":"invalid_grant"}"#),
        ]);

        let result = fetch_quota_with_active_recovery_at(
            &old_access,
            &active_account(),
            server.base_url(),
            temporary.path(),
            Some((
                server.base_url(),
                crate::oauth::RefreshLockProtocol::TypeScriptDirectory,
            )),
            None,
        )
        .await;

        let err = result.unwrap_err();
        assert_eq!(err.http_status(), Some(401));
        assert!(err.is_authentication_failure());
        assert!(!err.is_retryable());
        assert_eq!(std::fs::read_to_string(credentials).unwrap(), old);
        assert_eq!(server.finish().len(), 2);
    }

    // --- 凭证过期型 401（issue #41）：协调恢复不可用/已放弃 + JWT exp 已过期 → 引导文案 ---

    #[tokio::test]
    async fn expired_token_401_with_abandoned_recovery_is_credential_expired() {
        let temporary = tempfile::tempdir().unwrap();
        let credentials = crate::paths::active_cred_path(temporary.path());
        std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        // access token 的 JWT exp 早已过去；官方拒绝刷新（invalid_grant）→ 协调恢复放弃。
        let blob = credential_blob_with_exp("OLD", "R1", 1_000_000_000);
        let old_access = extract_json_string(&blob, "access_token");
        std::fs::write(&credentials, &blob).unwrap();
        let server = MockServer::start(vec![
            ("401 Unauthorized", r#"{"error":"expired"}"#),
            ("401 Unauthorized", r#"{"error":"invalid_grant"}"#),
        ]);

        let err = fetch_quota_with_active_recovery_at(
            &old_access,
            &active_account(),
            server.base_url(),
            temporary.path(),
            Some((
                server.base_url(),
                crate::oauth::RefreshLockProtocol::TypeScriptDirectory,
            )),
            None,
        )
        .await
        .unwrap_err();

        assert_eq!(
            err.quota_failure().map(|f| f.kind),
            Some(QuotaFailureKind::CredentialExpired)
        );
        assert_eq!(err.http_status(), Some(401));
        assert!(err.is_authentication_failure());
        assert!(!err.is_retryable());
        assert!(
            err.to_string().contains(CREDENTIAL_EXPIRED_GUIDANCE),
            "{err}"
        );
        assert!(
            !err.to_string().contains("HTTP 401"),
            "引导文案应替代裸 401：{err}"
        );
        // 恢复被拒绝不得覆盖 live 凭证。
        assert_eq!(std::fs::read_to_string(credentials).unwrap(), blob);
        // 确证走过了「查询 401 → 协调恢复被拒」完整路径。
        assert_eq!(
            server.finish(),
            vec!["GET /usages HTTP/1.1", "POST /api/oauth/token HTTP/1.1"]
        );
    }

    #[tokio::test]
    async fn unexpired_token_401_with_abandoned_recovery_keeps_http_kind() {
        let temporary = tempfile::tempdir().unwrap();
        let credentials = crate::paths::active_cred_path(temporary.path());
        std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        // exp 在未来（2126 年）→ 不能确证过期，协调恢复放弃后维持原类别与裸 401 文案。
        let blob = credential_blob_with_exp("OLD", "R1", 4_000_000_000);
        let old_access = extract_json_string(&blob, "access_token");
        std::fs::write(&credentials, &blob).unwrap();
        let server = MockServer::start(vec![
            ("401 Unauthorized", r#"{"error":"expired"}"#),
            ("401 Unauthorized", r#"{"error":"invalid_grant"}"#),
        ]);

        let err = fetch_quota_with_active_recovery_at(
            &old_access,
            &active_account(),
            server.base_url(),
            temporary.path(),
            Some((
                server.base_url(),
                crate::oauth::RefreshLockProtocol::TypeScriptDirectory,
            )),
            None,
        )
        .await
        .unwrap_err();

        assert_eq!(
            err.quota_failure().map(|f| f.kind),
            Some(QuotaFailureKind::Http)
        );
        assert_eq!(err.http_status(), Some(401));
        assert!(err.to_string().contains("HTTP 401"), "{err}");
        assert_eq!(std::fs::read_to_string(credentials).unwrap(), blob);
        assert_eq!(server.finish().len(), 2);
    }

    /// F4 回归：停泊账号（active=false）+ 过期 JWT + usages 401 → 维持原 Http 类别
    /// 裸 401，不进入「凭证过期型 401」归类（归类只服务当前账号的协调恢复兜底）。
    #[tokio::test]
    async fn parked_account_401_keeps_http_kind_and_skips_credential_expired() {
        let temporary = tempfile::tempdir().unwrap();
        let credentials = crate::paths::active_cred_path(temporary.path());
        std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        // access token 的 JWT exp 早已过去；若误入归类路径会被改写成引导文案。
        let blob = credential_blob_with_exp("OLD", "R1", 1_000_000_000);
        let old_access = extract_json_string(&blob, "access_token");
        std::fs::write(&credentials, &blob).unwrap();
        let server = MockServer::start(vec![("401 Unauthorized", r#"{"error":"expired"}"#)]);

        let err = fetch_quota_with_active_recovery_at(
            &old_access,
            &parked_account(),
            server.base_url(),
            temporary.path(),
            None, // 停泊账号本就不进协调恢复；显式 None 钉住
            None,
        )
        .await
        .unwrap_err();

        assert_eq!(
            err.quota_failure().map(|f| f.kind),
            Some(QuotaFailureKind::Http),
            "停泊账号 401 不得改归类：{err}"
        );
        assert_eq!(err.http_status(), Some(401));
        assert!(err.to_string().contains("HTTP 401"), "维持裸 401：{err}");
        assert!(
            !err.to_string().contains(CREDENTIAL_EXPIRED_GUIDANCE),
            "不得出现凭证过期引导文案：{err}"
        );
        // 只打了 usages 一次：未尝试协调恢复（无 refresh 请求）。
        assert_eq!(server.finish(), vec!["GET /usages HTTP/1.1"]);
        assert_eq!(std::fs::read_to_string(credentials).unwrap(), blob);
    }

    #[test]
    fn classify_unrecovered_401_requires_provable_expiry() {
        // 过期 JWT → 归入凭证过期型 401。
        let expired = credential_blob_with_exp("OLD", "R1", 1_000_000_000);
        let expired_token = extract_json_string(&expired, "access_token");
        let err = classify_unrecovered_401(
            Error::quota_http(401, "kimi usages HTTP 401"),
            &expired_token,
        );
        assert_eq!(
            err.quota_failure().map(|f| f.kind),
            Some(QuotaFailureKind::CredentialExpired)
        );
        assert!(err.to_string().contains(CREDENTIAL_EXPIRED_GUIDANCE));

        // 既有测试 blob 的 JWT 无 exp → 无法确证过期，维持裸 401。
        let no_exp = credential_blob("OLD", "R1");
        let no_exp_token = extract_json_string(&no_exp, "access_token");
        let err = classify_unrecovered_401(
            Error::quota_http(401, "kimi usages HTTP 401"),
            &no_exp_token,
        );
        assert_eq!(
            err.quota_failure().map(|f| f.kind),
            Some(QuotaFailureKind::Http)
        );
        assert!(err.to_string().contains("HTTP 401"));
    }

    #[test]
    fn classify_never_touches_non_401_failures() {
        let expired = credential_blob_with_exp("OLD", "R1", 1_000_000_000);
        let token = extract_json_string(&expired, "access_token");
        let err = classify_unrecovered_401(Error::quota_http(500, "kimi usages HTTP 500"), &token);
        assert_eq!(
            err.quota_failure().map(|f| f.kind),
            Some(QuotaFailureKind::Http)
        );
        assert_eq!(err.http_status(), Some(500));
        let err = classify_unrecovered_401(Error::quota_network("connection refused"), &token);
        assert_eq!(
            err.quota_failure().map(|f| f.kind),
            Some(QuotaFailureKind::Network)
        );
    }

    fn credential_blob_with_exp(access: &str, refresh: &str, exp: i64) -> String {
        // payload = {"user_id":"u-123","client_id":"c-1","exp":<exp>}
        let jwt = format!(
            "{}-{access}",
            crate::test_support::jwt_with_payload(&format!(
                r#"{{"user_id":"u-123","client_id":"c-1","exp":{exp}}}"#
            ))
        );
        format!(r#"{{"access_token":"{jwt}","refresh_token":"{refresh}","expires_at":0}}"#)
    }

    fn credential_blob(access: &str, refresh: &str) -> String {
        // payload = {"user_id":"u-123","client_id":"c-1"}
        let jwt = format!("header.eyJ1c2VyX2lkIjoidS0xMjMiLCJjbGllbnRfaWQiOiJjLTEifQ.sig-{access}");
        format!(r#"{{"access_token":"{jwt}","refresh_token":"{refresh}","expires_at":0}}"#)
    }

    fn active_account() -> Account {
        Account {
            provider: "kimi".into(),
            id: AccountId("u-123".into()),
            label: "u-123".into(),
            active: true,
            created_at: Utc::now(),
            last_used_at: None,
            extra: serde_json::Map::new(),
        }
    }

    fn parked_account() -> Account {
        Account {
            active: false,
            ..active_account()
        }
    }

    fn extract_json_string(raw: &str, field: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        value.get(field).unwrap().as_str().unwrap().to_string()
    }
}
