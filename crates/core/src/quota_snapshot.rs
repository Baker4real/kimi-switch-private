//! 额度快照：给一组账号，返回各账号额度结果。
//!
//! 节流、失败退避、并发查询、超时重试、陈旧回退都收口在这里。调用方只渲染。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::error::{Error, QuotaFailure};
use crate::model::{AccountId, Quota};
use crate::provider::Provider;
use crate::quota_cache::{quota_failure_of, QuotaCache, ValidEntry};
use crate::quota_query::query_quota_with_retry;
use crate::settings::{self, Quota as QuotaSettings};

/// 单个账号的额度快照。
#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    pub account_id: AccountId,
    /// 本条数据的取得时间：新鲜查询为本次写入缓存时刻，缓存/陈旧回退为原缓存时刻。
    pub fetched_at: DateTime<Utc>,
    /// 查询失败或退避跳过时复用了仍有效的旧窗口。
    pub stale: bool,
    /// 会员等级。优先取窗口一等字段，存量缓存回退到 `note`。
    pub membership: Option<String>,
    pub state: QuotaState,
}

/// 新鲜 / 缓存 / 失败。
#[derive(Debug, Clone)]
pub enum QuotaState {
    /// 刚完成真实查询。
    Ready(Vec<Quota>),
    /// 复用缓存：节流命中，或失败后的陈旧回退（见 [`QuotaSnapshot::stale`]）。
    Cached(Vec<Quota>),
    /// 无可用额度。携带结构化失败（种类 / HTTP 状态 / 文案），展示层按 `kind` 区分
    /// 一般查询失败与凭证过期型 401（`CredentialExpired` → 「凭证待续期」），不嗅探文案。
    Failed(QuotaFailure),
}

/// 查询一组账号的额度快照。
///
/// 内部读缓存、按当前配置做 90s 节流与失败退避、并发查询（含超时重试）、陈旧回退并写回缓存。
pub async fn collect<P>(
    provider: Arc<P>,
    provider_id: &str,
    account_ids: &[AccountId],
    cache_path: &Path,
) -> Vec<QuotaSnapshot>
where
    P: Provider + 'static,
{
    let cfg = settings::current().quota.clone();
    let mut cache = QuotaCache::load(cache_path);
    let out = collect_with(provider, provider_id, account_ids, &mut cache, &cfg).await;
    cache.save(cache_path);
    out
}

async fn collect_with<P>(
    provider: Arc<P>,
    provider_id: &str,
    account_ids: &[AccountId],
    cache: &mut QuotaCache,
    cfg: &QuotaSettings,
) -> Vec<QuotaSnapshot>
where
    P: Provider + 'static,
{
    let min_refresh = Duration::from_millis(cfg.min_refresh_interval_ms);
    let backoff_cap = Duration::from_millis(cfg.failure_backoff_max_ms);
    let mut out: Vec<Option<QuotaSnapshot>> = vec![None; account_ids.len()];
    let mut jobs = Vec::new();

    for (idx, id) in account_ids.iter().enumerate() {
        if let Some(entry) = cache.fresh(provider_id, &id.0, min_refresh) {
            out[idx] = Some(from_cache(id, entry, false));
            continue;
        }
        if let Some(failure) =
            cache.in_failure_backoff(provider_id, &id.0, min_refresh, backoff_cap)
        {
            let error = failure.as_error();
            let failed_at = failure.failed_at;
            out[idx] = Some(match cache.get(provider_id, &id.0) {
                Some(entry) if !error.is_authentication_failure() => from_cache(id, entry, true),
                _ => failed(id, error, failed_at),
            });
            continue;
        }
        jobs.push((idx, id.clone()));
    }

    let mut handles = Vec::new();
    for (idx, id) in jobs {
        let provider = provider.clone();
        let job_id = id.clone();
        handles.push((
            idx,
            id,
            tokio::spawn(async move { query_quota_with_retry(provider.as_ref(), &job_id).await }),
        ));
    }

    for (idx, id, handle) in handles {
        let result = match handle.await {
            Ok(inner) => inner,
            Err(join) => Err(Error::quota_other(format!(
                "quota fetch task failed: {join}"
            ))),
        };
        match result {
            Ok(quotas) => {
                cache.set(provider_id, &id.0, quotas.clone());
                let fetched_at = cache
                    .get(provider_id, &id.0)
                    .map(|entry| entry.cached_at)
                    .unwrap_or_else(Utc::now);
                out[idx] = Some(QuotaSnapshot {
                    account_id: id,
                    fetched_at,
                    stale: false,
                    membership: membership_of(&quotas),
                    state: QuotaState::Ready(quotas),
                });
            }
            Err(error) => {
                cache.record_failure(provider_id, &id.0, &error);
                out[idx] = Some(match cache.get(provider_id, &id.0) {
                    Some(entry) if !error.is_authentication_failure() => {
                        from_cache(&id, entry, true)
                    }
                    _ => failed(&id, error, Utc::now()),
                });
            }
        }
    }

    out.into_iter()
        .enumerate()
        .map(|(idx, item)| {
            item.unwrap_or_else(|| {
                failed(
                    account_ids.get(idx).unwrap_or(&AccountId(String::new())),
                    Error::quota_other("n/a"),
                    Utc::now(),
                )
            })
        })
        .collect()
}

fn from_cache(id: &AccountId, entry: ValidEntry, stale: bool) -> QuotaSnapshot {
    QuotaSnapshot {
        account_id: id.clone(),
        fetched_at: entry.cached_at,
        stale,
        membership: membership_of(&entry.quotas),
        state: QuotaState::Cached(entry.quotas),
    }
}

fn failed(id: &AccountId, error: Error, fetched_at: DateTime<Utc>) -> QuotaSnapshot {
    QuotaSnapshot {
        account_id: id.clone(),
        fetched_at,
        stale: false,
        membership: None,
        state: QuotaState::Failed(quota_failure_of(&error)),
    }
}

fn membership_of(quotas: &[Quota]) -> Option<String> {
    quotas.iter().find_map(|q| {
        q.membership
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| q.note.clone().filter(|s| !s.is_empty()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use chrono::Duration as ChronoDuration;

    use crate::error::{Error, QuotaFailureKind, Result};
    use crate::model::{QuotaStatus, QuotaWindow};

    struct ScriptedProvider {
        calls: AtomicUsize,
        inflight: AtomicUsize,
        max_inflight: AtomicUsize,
        delay: Duration,
        responses: Mutex<HashMap<String, std::result::Result<Vec<Quota>, Error>>>,
    }

    impl ScriptedProvider {
        fn new(
            delay: Duration,
            pairs: &[(&str, std::result::Result<Vec<Quota>, Error>)],
        ) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                inflight: AtomicUsize::new(0),
                max_inflight: AtomicUsize::new(0),
                delay,
                responses: Mutex::new(
                    pairs
                        .iter()
                        .map(|(id, result)| {
                            (
                                (*id).to_string(),
                                match result {
                                    Ok(quotas) => Ok(quotas.clone()),
                                    Err(err) => Err(clone_error(err)),
                                },
                            )
                        })
                        .collect(),
                ),
            })
        }
    }

    fn clone_error(error: &Error) -> Error {
        match error {
            Error::QuotaFetch(failure) => Error::QuotaFetch(failure.clone()),
            Error::Provider(failure) => Error::Provider(failure.clone()),
            other => Error::quota_other(other.to_string()),
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn activate(&self, _id: &AccountId) -> Result<()> {
            Ok(())
        }

        async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let inflight = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_inflight.fetch_max(inflight, Ordering::SeqCst);
            let outcome = self
                .responses
                .lock()
                .unwrap()
                .get(&id.0)
                .map(|result| match result {
                    Ok(quotas) => Ok(quotas.clone()),
                    Err(err) => Err(clone_error(err)),
                });
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            match outcome {
                Some(Ok(quotas)) => Ok(quotas),
                Some(Err(err)) => Err(err),
                None => Err(Error::quota_other(format!("no fixture for {}", id.0))),
            }
        }
    }

    fn sample(id: &str, membership: Option<&str>, note: Option<&str>) -> Quota {
        Quota {
            provider: "kimi".into(),
            account_id: AccountId(id.into()),
            window: QuotaWindow::FiveHour,
            used: 1,
            limit: 100,
            reset_at: Some(Utc::now() + ChronoDuration::hours(4)),
            status: QuotaStatus::Ok,
            note: note.map(String::from),
            membership: membership.map(String::from),
        }
    }

    fn ids(names: &[&str]) -> Vec<AccountId> {
        names.iter().map(|id| AccountId((*id).into())).collect()
    }

    fn cache_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("quota_cache.json")
    }

    async fn run(
        provider: Arc<ScriptedProvider>,
        names: &[&str],
        cache_path: &Path,
    ) -> Vec<QuotaSnapshot> {
        collect(provider, "kimi", &ids(names), cache_path).await
    }

    fn cached_quotas(state: &QuotaState) -> &[Quota] {
        match state {
            QuotaState::Cached(quotas) | QuotaState::Ready(quotas) => quotas,
            QuotaState::Failed(err) => panic!("expected quotas, got failed: {err}"),
        }
    }

    #[tokio::test]
    async fn throttle_hit_skips_query_and_returns_cached() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample(
                "alice",
                Some("LEVEL_INTERMEDIATE"),
                Some("LEVEL_INTERMEDIATE"),
            )],
        );
        cache.save(&path);

        let provider = ScriptedProvider::new(Duration::ZERO, &[("alice", Ok(vec![]))]);
        let snaps = run(provider.clone(), &["alice"], &path).await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(snaps.len(), 1);
        assert!(!snaps[0].stale);
        assert!(matches!(snaps[0].state, QuotaState::Cached(_)));
        assert_eq!(cached_quotas(&snaps[0].state)[0].used, 1);
        assert_eq!(snaps[0].membership.as_deref(), Some("LEVEL_INTERMEDIATE"));
    }

    #[tokio::test]
    async fn failure_backoff_skips_query() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set("kimi", "alice", vec![sample("alice", None, None)]);
        cache.age_success("kimi", "alice", ChronoDuration::minutes(5));
        cache.record_failure("kimi", "alice", &Error::quota_http(429, "429 rate limited"));
        cache.record_failure(
            "kimi",
            "bob",
            &Error::quota_http(401, "wording that never mentions the status"),
        );
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[("alice", Ok(vec![])), ("bob", Ok(vec![]))],
        );
        let snaps = run(provider.clone(), &["alice", "bob"], &path).await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(snaps[0].stale);
        assert!(matches!(snaps[0].state, QuotaState::Cached(_)));
        assert_eq!(cached_quotas(&snaps[0].state)[0].used, 1);
        assert!(!snaps[1].stale);
        match &snaps[1].state {
            QuotaState::Failed(_) => {}
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn queries_accounts_concurrently_and_aggregates() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let provider = ScriptedProvider::new(
            Duration::from_millis(80),
            &[
                ("a", Ok(vec![sample("a", None, None)])),
                ("b", Ok(vec![sample("b", None, None)])),
                ("c", Ok(vec![sample("c", None, None)])),
            ],
        );

        let snaps = run(provider.clone(), &["a", "b", "c"], &path).await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
        assert!(
            provider.max_inflight.load(Ordering::SeqCst) >= 2,
            "queries should overlap, max inflight={}",
            provider.max_inflight.load(Ordering::SeqCst)
        );
        assert_eq!(snaps.len(), 3);
        for (snap, name) in snaps.iter().zip(["a", "b", "c"]) {
            assert_eq!(snap.account_id.0, name);
            assert!(!snap.stale);
            assert!(matches!(snap.state, QuotaState::Ready(_)));
            assert_eq!(cached_quotas(&snap.state)[0].used, 1);
        }
    }

    #[tokio::test]
    async fn query_failure_falls_back_to_stale_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set("kimi", "alice", vec![sample("alice", None, None)]);
        cache.age_success("kimi", "alice", ChronoDuration::minutes(5));
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[("alice", Err(Error::quota_http(429, "429 rate limited")))],
        );
        let snaps = run(provider.clone(), &["alice"], &path).await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(snaps[0].stale);
        assert!(matches!(snaps[0].state, QuotaState::Cached(_)));
        assert_eq!(cached_quotas(&snaps[0].state)[0].used, 1);
    }

    #[tokio::test]
    async fn credential_expired_failure_keeps_typed_kind_on_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Err(Error::quota_credential_expired(
                    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                )),
            )],
        );

        let snaps = run(provider, &["alice"], &path).await;

        match &snaps[0].state {
            QuotaState::Failed(failure) => {
                assert_eq!(failure.kind, QuotaFailureKind::CredentialExpired);
                assert_eq!(failure.status, Some(401));
                assert!(failure.message.contains("凭证已过期"), "{failure}");
            }
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failure_backoff_preserves_credential_expired_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        // 退避窗口内直接复用失败记录：类型化 kind 必须随缓存往返不丢。
        let mut cache = QuotaCache::default();
        cache.record_failure(
            "kimi",
            "alice",
            &Error::quota_credential_expired("凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权"),
        );
        cache.save(&path);

        let provider = ScriptedProvider::new(Duration::ZERO, &[("alice", Ok(vec![]))]);
        let snaps = run(provider.clone(), &["alice"], &path).await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        match &snaps[0].state {
            QuotaState::Failed(failure) => {
                assert_eq!(failure.kind, QuotaFailureKind::CredentialExpired);
                assert_eq!(failure.status, Some(401));
            }
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn results_distinguish_ready_cached_failed() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set("kimi", "alice", vec![sample("alice", None, None)]);
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[
                ("bob", Ok(vec![sample("bob", Some("LEVEL_ADVANCED"), None)])),
                ("carol", Err(Error::quota_http(429, "429 rate limited"))),
            ],
        );
        let snaps = run(provider.clone(), &["alice", "bob", "carol"], &path).await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert!(matches!(snaps[0].state, QuotaState::Cached(_)));
        assert!(!snaps[0].stale);
        assert!(matches!(snaps[1].state, QuotaState::Ready(_)));
        assert!(!snaps[1].stale);
        match &snaps[2].state {
            QuotaState::Failed(_) => {}
            other => panic!("expected failed, got {other:?}"),
        }
        assert!(!snaps[2].stale);
        assert_eq!(snaps[1].membership.as_deref(), Some("LEVEL_ADVANCED"));
    }

    #[tokio::test]
    async fn membership_is_first_class_on_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        // 存量缓存只有 note、没有 membership 字段。
        cache.set(
            "kimi",
            "alice",
            vec![sample("alice", None, Some("LEVEL_INTERMEDIATE"))],
        );
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "bob",
                Ok(vec![sample(
                    "bob",
                    Some("LEVEL_ADVANCED"),
                    Some("LEVEL_ADVANCED"),
                )]),
            )],
        );
        let snaps = run(provider.clone(), &["alice", "bob"], &path).await;

        assert_eq!(snaps[0].membership.as_deref(), Some("LEVEL_INTERMEDIATE"));
        assert!(matches!(snaps[0].state, QuotaState::Cached(_)));
        assert_eq!(snaps[1].membership.as_deref(), Some("LEVEL_ADVANCED"));
        assert!(matches!(snaps[1].state, QuotaState::Ready(_)));
    }
}
