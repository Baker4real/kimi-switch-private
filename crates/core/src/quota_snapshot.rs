//! 额度快照：给一组账号，返回各账号额度结果。
//!
//! 节流、失败退避、并发查询、超时重试、陈旧回退都收口在这里。调用方只渲染。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::defaults;
use crate::error::{Error, QuotaFailure};
use crate::model::{AccountId, Quota};
use crate::provider::Provider;
use crate::quota_cache::{quota_failure_of, QuotaCache, ValidEntry};
use crate::quota_query::query_quota_with_retry;
use crate::settings::{self, Quota as QuotaSettings};

/// 额度快照收集原因：决定成功缓存的节流窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaCollectReason {
    /// 计划内列出（启动、静默、CLI、import/swap 后再列出）：90s 成功节流。
    Scheduled,
    /// 用户点「刷新」发起的额度查询：5s 连点窗口。
    UserInitiated,
}

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

/// 一次额度收集里各账号为何跳过或真正发起了查询。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuotaCollectStats {
    /// 真正向 Provider 发起额度查询的账号数。
    pub queried: usize,
    /// 成功缓存仍在节流窗内而跳过。
    pub skipped_success_throttle: usize,
    /// 因 429 退避跳过。
    pub skipped_rate_limit: usize,
    /// 因鉴权失败或凭证待续期退避跳过。
    pub skipped_auth: usize,
    /// 聚焦收集（issue #161）中不在焦点内而未查询、直接以缓存/退避记录组装行的账号数。
    pub skipped_out_of_focus: usize,
}

/// 额度快照与跳过统计。GUI 用统计决定用户发起时要不要带提示。
#[derive(Debug, Clone)]
pub struct QuotaCollectOutcome {
    pub snapshots: Vec<QuotaSnapshot>,
    pub stats: QuotaCollectStats,
}

/// 查询一组账号的额度快照。
///
/// 内部读缓存、按收集原因做成功节流、失败退避、并发查询（含超时重试）、陈旧回退并写回缓存。
/// [`QuotaCollectReason::Scheduled`] 用配置里的最小间隔（默认 90s）；
/// [`QuotaCollectReason::UserInitiated`] 用 5s 连点窗口。
/// 失败退避：429 不打网；鉴权（401/403/凭证待续期）仅计划内跳过（有凭证恢复信号则仍查），用户发起会再查；
/// 超时/网络等 [`Error::is_retryable`] 失败仅计划内跳过，用户发起会再查。
pub async fn collect<P>(
    provider: Arc<P>,
    provider_id: &str,
    account_ids: &[AccountId],
    cache_path: &Path,
    reason: QuotaCollectReason,
) -> Vec<QuotaSnapshot>
where
    P: Provider + 'static,
{
    collect_detailed(provider, provider_id, account_ids, cache_path, reason)
        .await
        .snapshots
}

/// 同 [`collect`]，额外返回跳过/查询统计。聚焦语义见 [`collect_detailed_focused`]。
pub async fn collect_detailed<P>(
    provider: Arc<P>,
    provider_id: &str,
    account_ids: &[AccountId],
    cache_path: &Path,
    reason: QuotaCollectReason,
) -> QuotaCollectOutcome
where
    P: Provider + 'static,
{
    collect_detailed_focused(
        provider,
        provider_id,
        account_ids,
        cache_path,
        reason,
        None,
    )
    .await
}

/// 聚焦收集时目标账号（当前账号）的成功节流窗下限（issue #161）：与前端适应性
/// 档位的下限档 10s（`SILENT_QUOTA_CRITICAL_INTERVAL_MS`）对齐。仅作用于聚焦轮的
/// 目标账号；停泊账号与其他收集路径仍走 `min_refresh_interval_ms`（默认 90s），
/// 用户配置更小时从其小。失败退避与恢复信号语义不变（ADR-0009）。
pub const QUOTA_FOCUS_REFRESH_INTERVAL_MS: u64 = 10_000;

/// 同 [`collect_detailed`]，但带聚焦目标（issue #161 适应性轮询）：
/// `focus = Some(当前账号)` 时只对目标账号做真实查询，其余账号一律不发查询、
/// 以既有缓存或退避记录组装行（停泊账号请求节奏只由全量轮决定）；目标账号的
/// 成功节流窗放宽到 [`QUOTA_FOCUS_REFRESH_INTERVAL_MS`]，让 30s/10s 加密档的
/// 查询能真正落到网络。`focus = None` 与 [`collect_detailed`] 等价。
/// 快照始终覆盖全部账号，自动切换判定与展示不受聚焦影响。
pub async fn collect_detailed_focused<P>(
    provider: Arc<P>,
    provider_id: &str,
    account_ids: &[AccountId],
    cache_path: &Path,
    reason: QuotaCollectReason,
    focus: Option<&AccountId>,
) -> QuotaCollectOutcome
where
    P: Provider + 'static,
{
    let cfg = settings::current().quota.clone();
    let mut cache = QuotaCache::load(cache_path);
    let (snapshots, stats) = collect_with(
        provider,
        provider_id,
        account_ids,
        &mut cache,
        &cfg,
        reason,
        focus,
    )
    .await;
    cache.save(cache_path);
    QuotaCollectOutcome { snapshots, stats }
}

fn success_throttle(reason: QuotaCollectReason, cfg: &QuotaSettings) -> Duration {
    match reason {
        QuotaCollectReason::Scheduled => Duration::from_millis(cfg.min_refresh_interval_ms),
        QuotaCollectReason::UserInitiated => {
            Duration::from_millis(defaults::QUOTA_USER_INITIATED_MIN_INTERVAL_MS)
        }
    }
}

async fn collect_with<P>(
    provider: Arc<P>,
    provider_id: &str,
    account_ids: &[AccountId],
    cache: &mut QuotaCache,
    cfg: &QuotaSettings,
    reason: QuotaCollectReason,
    focus: Option<&AccountId>,
) -> (Vec<QuotaSnapshot>, QuotaCollectStats)
where
    P: Provider + 'static,
{
    let min_refresh = Duration::from_millis(cfg.min_refresh_interval_ms);
    let base_success_throttle = success_throttle(reason, cfg);
    // 聚焦轮（issue #161）：目标账号的成功节流窗放宽到 10s 下限（用户配置更小
    // 则从其小）；停泊账号不参与本轮查询，节流窗与其无关。
    let focus_success_throttle = if focus.is_some() {
        base_success_throttle.min(Duration::from_millis(QUOTA_FOCUS_REFRESH_INTERVAL_MS))
    } else {
        base_success_throttle
    };
    let backoff_cap = Duration::from_millis(cfg.failure_backoff_max_ms);
    let mut out: Vec<Option<QuotaSnapshot>> = vec![None; account_ids.len()];
    let mut jobs = Vec::new();
    let mut stats = QuotaCollectStats::default();

    for (idx, id) in account_ids.iter().enumerate() {
        if focus.is_some_and(|focus| focus != id) {
            // 聚焦轮的非目标账号（issue #161）：一律不发查询，用既有缓存或退避
            // 记录组装行，保证停泊账号的请求量不随加密档增加。
            stats.skipped_out_of_focus += 1;
            out[idx] = Some(out_of_focus_row(
                provider_id,
                id,
                cache,
                min_refresh,
                backoff_cap,
            ));
            continue;
        }
        let success_window = if focus == Some(id) {
            focus_success_throttle
        } else {
            base_success_throttle
        };
        if let Some(entry) = cache.fresh(provider_id, &id.0, success_window) {
            stats.skipped_success_throttle += 1;
            out[idx] = Some(from_cache(id, entry, false));
            continue;
        }
        if let Some(failure) =
            cache.in_failure_backoff(provider_id, &id.0, min_refresh, backoff_cap)
        {
            let error = failure.as_error();
            // 用户点「刷新」时，可重试失败（超时/网络等）与鉴权失败/凭证待续期
            // 忽略退避再打一次；429 限流继续跳过。
            let retry_now = reason == QuotaCollectReason::UserInitiated
                && (error.is_retryable() || error.is_authentication_failure());
            if !retry_now {
                // 将要因鉴权退避跳过时，凭证恢复信号放行真实查询；429 / 超时仍跳过。
                let recovered = error.is_authentication_failure()
                    && provider.credential_changed_since(id, failure.failed_at);
                if !recovered {
                    if error.http_status() == Some(429) {
                        stats.skipped_rate_limit += 1;
                    } else if error.is_authentication_failure() {
                        stats.skipped_auth += 1;
                    }
                    let failed_at = failure.failed_at;
                    out[idx] = Some(match cache.get(provider_id, &id.0) {
                        Some(entry) if !error.is_authentication_failure() => {
                            from_cache(id, entry, true)
                        }
                        _ => failed(id, error, failed_at),
                    });
                    continue;
                }
            }
        }
        stats.queried += 1;
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

    let snapshots = out
        .into_iter()
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
        .collect();
    (snapshots, stats)
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

/// 聚焦轮（issue #161）非目标账号的组装行：不发任何查询。组装优先级与全量轮的
/// 跳过形态保持一致——节流窗内成功缓存（非 stale，与全量轮 skip 同形）→ 退避记录
/// （陈旧缓存回退或 Failed，保留凭证待续期类型化 kind，供前端 5s 档判定复用）→
/// 窗外成功缓存（stale Cached，等下一个全量轮刷新）→ 从未查询过（n/a Failed 行）。
fn out_of_focus_row(
    provider_id: &str,
    id: &AccountId,
    cache: &QuotaCache,
    min_refresh: Duration,
    backoff_cap: Duration,
) -> QuotaSnapshot {
    if let Some(entry) = cache.fresh(provider_id, &id.0, min_refresh) {
        return from_cache(id, entry, false);
    }
    if let Some(failure) = cache.in_failure_backoff(provider_id, &id.0, min_refresh, backoff_cap) {
        let error = failure.as_error();
        let failed_at = failure.failed_at;
        return match cache.get(provider_id, &id.0) {
            Some(entry) if !error.is_authentication_failure() => from_cache(id, entry, true),
            _ => failed(id, error, failed_at),
        };
    }
    match cache.get(provider_id, &id.0) {
        Some(entry) => from_cache(id, entry, true),
        None => failed(id, Error::quota_other("n/a"), Utc::now()),
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

/// 给缺档位的快照补上账号 extra 缓存的官方档位（`official_membership`）。
///
/// 背景：官方 `/usages` 于 2026-09-14 移除 `user` 对象，查询结果不再携带
/// `user.membership.level`；档位来源迁到 `/me`（缓存在 extra）。快照自带档位
/// （旧缓存行 / 未来接口回补）仍然优先，缺失时才回退 extra——GUI 档位行、
/// 会员轨道与自动切换档位排序经此不断档。纯函数、不发网络。
pub fn apply_cached_membership(snapshots: &mut [QuotaSnapshot], accounts: &[crate::model::Account]) {
    for snapshot in snapshots.iter_mut() {
        if snapshot.membership.is_some() {
            continue;
        }
        let Some(account) = accounts.iter().find(|a| a.id == snapshot.account_id) else {
            continue;
        };
        if let Some(level) = crate::display_name::official_membership_from_extra(&account.extra) {
            snapshot.membership = Some(level);
        }
    }
}

/// 可用快照窗口里最早的 `reset_at`。
///
/// Ready 一律计入；Cached 仅非 stale 计入；Failed 与 stale Cached 不算可用。
/// 窗口缺 `reset_at` 则跳过；全部缺或没有可用窗口时返回 `None`。
pub fn earliest_reset_at(snapshots: &[QuotaSnapshot]) -> Option<DateTime<Utc>> {
    snapshots
        .iter()
        .filter_map(usable_quota_windows)
        .flatten()
        .filter_map(|quota| quota.reset_at)
        .min()
}

/// Ready 一律可用；Cached 仅在非 stale 回落时可用；Failed（含凭证待续期）不可用。
fn usable_quota_windows(snapshot: &QuotaSnapshot) -> Option<&[Quota]> {
    match &snapshot.state {
        QuotaState::Failed(_) => None,
        QuotaState::Cached(_) if snapshot.stale => None,
        QuotaState::Ready(quotas) | QuotaState::Cached(quotas) => Some(quotas.as_slice()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use chrono::Duration as ChronoDuration;

    use crate::error::{Error, QuotaFailure, QuotaFailureKind, Result};
    use crate::model::{QuotaStatus, QuotaWindow};

    struct ScriptedProvider {
        calls: AtomicUsize,
        /// 按账号 id 的真实查询计数（issue #161 聚焦轮请求计数证明）。
        calls_by_id: Mutex<HashMap<String, usize>>,
        inflight: AtomicUsize,
        max_inflight: AtomicUsize,
        delay: Duration,
        responses: Mutex<HashMap<String, std::result::Result<Vec<Quota>, Error>>>,
        /// 脚本化当前登录文件 mtime；缺省视为无凭证恢复信号。
        credential_mtime: Mutex<HashMap<String, DateTime<Utc>>>,
    }

    impl ScriptedProvider {
        fn new(
            delay: Duration,
            pairs: &[(&str, std::result::Result<Vec<Quota>, Error>)],
        ) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                calls_by_id: Mutex::new(HashMap::new()),
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
                credential_mtime: Mutex::new(HashMap::new()),
            })
        }

        /// 指定账号累计的真实查询次数（issue #161 请求计数证明）。
        fn calls_for(&self, id: &str) -> usize {
            self.calls_by_id
                .lock()
                .unwrap()
                .get(id)
                .copied()
                .unwrap_or(0)
        }

        fn mark_credential_changed(&self, id: &str) {
            // 晚于刚写入的失败时间，模拟官方刚重写当前登录文件。
            self.mark_credential_changed_at(id, Utc::now() + ChronoDuration::hours(1));
        }

        fn mark_credential_changed_at(&self, id: &str, mtime: DateTime<Utc>) {
            self.credential_mtime
                .lock()
                .unwrap()
                .insert(id.to_string(), mtime);
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
            {
                let mut by_id = self.calls_by_id.lock().unwrap();
                *by_id.entry(id.0.clone()).or_insert(0) += 1;
            }
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

        fn credential_changed_since(
            &self,
            account_id: &AccountId,
            failed_at: DateTime<Utc>,
        ) -> bool {
            self.credential_mtime
                .lock()
                .unwrap()
                .get(&account_id.0)
                .is_some_and(|mtime| *mtime > failed_at)
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
        collect(
            provider,
            "kimi",
            &ids(names),
            cache_path,
            QuotaCollectReason::Scheduled,
        )
        .await
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
    async fn user_initiated_queries_when_success_cache_is_10s_old() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample("alice", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "alice", ChronoDuration::seconds(10));
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
            )],
        );
        let snaps = collect(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(!snaps[0].stale);
        assert!(matches!(snaps[0].state, QuotaState::Ready(_)));
        assert_eq!(snaps[0].membership.as_deref(), Some("LEVEL_ADVANCED"));

        let written = QuotaCache::load(&path);
        let entry = written
            .get("kimi", "alice")
            .expect("success cache rewritten");
        assert!(Utc::now() - entry.cached_at < ChronoDuration::seconds(2));
        assert_eq!(
            entry.quotas[0].membership.as_deref(),
            Some("LEVEL_ADVANCED")
        );
    }

    #[tokio::test]
    async fn user_initiated_skips_query_when_success_cache_is_1s_old() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample("alice", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "alice", ChronoDuration::seconds(1));
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
            )],
        );
        let snaps = collect(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(!snaps[0].stale);
        assert!(matches!(snaps[0].state, QuotaState::Cached(_)));
        assert_eq!(snaps[0].membership.as_deref(), Some("LEVEL_INTERMEDIATE"));
    }

    #[tokio::test]
    async fn scheduled_skips_query_when_success_cache_is_10s_old() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample("alice", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "alice", ChronoDuration::seconds(10));
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
            )],
        );
        let snaps = run(provider.clone(), &["alice"], &path).await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(!snaps[0].stale);
        assert!(matches!(snaps[0].state, QuotaState::Cached(_)));
        assert_eq!(snaps[0].membership.as_deref(), Some("LEVEL_INTERMEDIATE"));
    }

    #[tokio::test]
    async fn scheduled_skips_within_90s_after_user_initiated_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample("alice", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "alice", ChronoDuration::seconds(10));
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
            )],
        );
        let first = collect(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::UserInitiated,
        )
        .await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(first[0].state, QuotaState::Ready(_)));

        let second = run(provider.clone(), &["alice"], &path).await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(!second[0].stale);
        assert!(matches!(second[0].state, QuotaState::Cached(_)));
        assert_eq!(second[0].membership.as_deref(), Some("LEVEL_ADVANCED"));
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
            &Error::quota_credential_expired(
                "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
            ),
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

    #[tokio::test]
    async fn user_initiated_queries_during_retryable_failure_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.record_failure("kimi", "alice", &Error::quota_timeout());
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
            )],
        );
        let snaps = collect(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(!snaps[0].stale);
        assert!(matches!(snaps[0].state, QuotaState::Ready(_)));
        assert_eq!(snaps[0].membership.as_deref(), Some("LEVEL_ADVANCED"));
    }

    #[tokio::test]
    async fn scheduled_skips_query_during_retryable_failure_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.record_failure("kimi", "alice", &Error::quota_timeout());
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
            )],
        );
        let snaps = run(provider.clone(), &["alice"], &path).await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        match &snaps[0].state {
            QuotaState::Failed(failure) => {
                assert_eq!(failure.kind, QuotaFailureKind::Timeout);
            }
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn user_initiated_retryable_success_clears_failure_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.record_failure("kimi", "alice", &Error::quota_timeout());
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[("alice", Ok(vec![sample("alice", None, None)]))],
        );
        let _ = collect(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::UserInitiated,
        )
        .await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        let written = QuotaCache::load(&path);
        assert!(written
            .in_failure_backoff(
                "kimi",
                "alice",
                Duration::from_secs(90),
                Duration::from_secs(900),
            )
            .is_none());
    }

    #[tokio::test]
    async fn user_initiated_skips_query_during_429_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set("kimi", "alice", vec![sample("alice", None, None)]);
        cache.age_success("kimi", "alice", ChronoDuration::minutes(5));
        cache.record_failure("kimi", "alice", &Error::quota_http(429, "429 rate limited"));
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
            )],
        );
        let snaps = collect(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(snaps[0].stale);
        assert!(matches!(snaps[0].state, QuotaState::Cached(_)));
        assert_eq!(cached_quotas(&snaps[0].state)[0].used, 1);
    }

    #[tokio::test]
    async fn user_initiated_queries_during_credential_expired_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.record_failure(
            "kimi",
            "alice",
            &Error::quota_credential_expired(
                "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
            ),
        );
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
            )],
        );
        let snaps = collect(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(!snaps[0].stale);
        assert!(matches!(snaps[0].state, QuotaState::Ready(_)));
        assert_eq!(snaps[0].membership.as_deref(), Some("LEVEL_ADVANCED"));
    }

    #[tokio::test]
    async fn user_initiated_requery_keeps_credential_expired_on_still_401() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.record_failure(
            "kimi",
            "alice",
            &Error::quota_credential_expired(
                "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
            ),
        );
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Err(Error::quota_credential_expired(
                    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                )),
            )],
        );
        let outcome = collect_detailed(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.stats.queried, 1);
        assert_eq!(outcome.stats.skipped_auth, 0);
        match &outcome.snapshots[0].state {
            QuotaState::Failed(failure) => {
                assert_eq!(failure.kind, QuotaFailureKind::CredentialExpired);
                assert_eq!(failure.status, Some(401));
                assert!(failure.message.contains("凭证已过期"), "{failure}");
            }
            other => panic!("expected credential expired, got {other:?}"),
        }
        assert!(outcome.snapshots[0].membership.is_none());

        let written = QuotaCache::load(&path);
        let recorded = written
            .in_failure_backoff(
                "kimi",
                "alice",
                Duration::from_secs(90),
                Duration::from_secs(900),
            )
            .expect("failure record kept");
        assert_eq!(recorded.kind, QuotaFailureKind::CredentialExpired);
        assert_eq!(recorded.status, Some(401));
        assert_eq!(recorded.consecutive, 2);
        assert!(written.get("kimi", "alice").is_none());
    }

    #[tokio::test]
    async fn collect_retry_now_bypasses_auth_backoff_not_429() {
        struct Case {
            name: &'static str,
            reason: QuotaCollectReason,
            error: Error,
            expect_query: bool,
        }
        let cases = [
            Case {
                name: "凭证待续期 + UserInitiated",
                reason: QuotaCollectReason::UserInitiated,
                error: Error::quota_credential_expired(
                    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                ),
                expect_query: true,
            },
            Case {
                name: "401 + UserInitiated",
                reason: QuotaCollectReason::UserInitiated,
                error: Error::quota_http(401, "unauthorized"),
                expect_query: true,
            },
            Case {
                name: "403 + UserInitiated",
                reason: QuotaCollectReason::UserInitiated,
                error: Error::quota_http(403, "forbidden"),
                expect_query: true,
            },
            Case {
                name: "429 + UserInitiated",
                reason: QuotaCollectReason::UserInitiated,
                error: Error::quota_http(429, "429 rate limited"),
                expect_query: false,
            },
            Case {
                name: "凭证待续期 + Scheduled",
                reason: QuotaCollectReason::Scheduled,
                error: Error::quota_credential_expired(
                    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                ),
                expect_query: false,
            },
            Case {
                name: "401 + Scheduled",
                reason: QuotaCollectReason::Scheduled,
                error: Error::quota_http(401, "unauthorized"),
                expect_query: false,
            },
        ];

        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = cache_path(&dir);
            let mut cache = QuotaCache::default();
            cache.record_failure("kimi", "alice", &case.error);
            cache.save(&path);

            let provider = ScriptedProvider::new(
                Duration::ZERO,
                &[(
                    "alice",
                    Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
                )],
            );
            let _ = collect(
                provider.clone(),
                "kimi",
                &ids(&["alice"]),
                &path,
                case.reason,
            )
            .await;
            let expected_calls = usize::from(case.expect_query);
            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                expected_calls,
                "{}",
                case.name
            );
        }
    }

    #[tokio::test]
    async fn user_initiated_queries_due_accounts_and_keeps_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample("alice", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "alice", ChronoDuration::seconds(10));
        cache.set(
            "kimi",
            "bob",
            vec![sample("bob", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "bob", ChronoDuration::seconds(1));
        let mut carol_cached = sample("carol", Some("LEVEL_INTERMEDIATE"), None);
        carol_cached.used = 3;
        cache.set("kimi", "carol", vec![carol_cached]);
        cache.age_success("kimi", "carol", ChronoDuration::minutes(5));
        cache.record_failure("kimi", "carol", &Error::quota_http(429, "429 rate limited"));
        cache.save(&path);

        let mut alice_fresh = sample("alice", Some("LEVEL_ADVANCED"), None);
        alice_fresh.used = 42;
        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[
                ("alice", Ok(vec![alice_fresh])),
                ("bob", Ok(vec![sample("bob", Some("LEVEL_ADVANCED"), None)])),
                (
                    "carol",
                    Ok(vec![sample("carol", Some("LEVEL_ADVANCED"), None)]),
                ),
            ],
        );
        let snaps = collect(
            provider.clone(),
            "kimi",
            &ids(&["alice", "bob", "carol"]),
            &path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(snaps[0].state, QuotaState::Ready(_)));
        assert_eq!(cached_quotas(&snaps[0].state)[0].used, 42);
        assert_eq!(snaps[0].membership.as_deref(), Some("LEVEL_ADVANCED"));
        assert!(!snaps[1].stale);
        assert!(matches!(snaps[1].state, QuotaState::Cached(_)));
        assert_eq!(snaps[1].membership.as_deref(), Some("LEVEL_INTERMEDIATE"));
        assert_eq!(cached_quotas(&snaps[1].state)[0].used, 1);
        assert!(snaps[2].stale);
        assert!(matches!(snaps[2].state, QuotaState::Cached(_)));
        assert_eq!(cached_quotas(&snaps[2].state)[0].used, 3);
        assert_eq!(snaps[2].membership.as_deref(), Some("LEVEL_INTERMEDIATE"));
    }

    #[tokio::test]
    async fn collect_stats_classify_user_initiated_skips() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "alice",
            vec![sample("alice", Some("LEVEL_INTERMEDIATE"), None)],
        );
        cache.age_success("kimi", "alice", ChronoDuration::seconds(1));
        cache.record_failure("kimi", "bob", &Error::quota_http(429, "429 rate limited"));
        cache.record_failure(
            "kimi",
            "carol",
            &Error::quota_credential_expired(
                "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
            ),
        );
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[
                ("alice", Ok(vec![])),
                ("bob", Ok(vec![])),
                ("carol", Ok(vec![])),
            ],
        );
        let outcome = collect_detailed(
            provider.clone(),
            "kimi",
            &ids(&["alice", "bob", "carol"]),
            &path,
            QuotaCollectReason::UserInitiated,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.stats.queried, 1);
        assert_eq!(outcome.stats.skipped_success_throttle, 1);
        assert_eq!(outcome.stats.skipped_rate_limit, 1);
        assert_eq!(outcome.stats.skipped_auth, 0);
        match &outcome.snapshots[2].state {
            QuotaState::Ready(quotas) => assert!(quotas.is_empty()),
            other => panic!("expected queried ready, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn collect_recovery_signal_bypasses_auth_backoff_not_429() {
        struct Case {
            name: &'static str,
            reason: QuotaCollectReason,
            error: Error,
            signal: bool,
            expect_query: bool,
        }
        let cases = [
            Case {
                name: "凭证待续期 + Scheduled + 恢复信号",
                reason: QuotaCollectReason::Scheduled,
                error: Error::quota_credential_expired(
                    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                ),
                signal: true,
                expect_query: true,
            },
            Case {
                name: "401 + Scheduled + 恢复信号",
                reason: QuotaCollectReason::Scheduled,
                error: Error::quota_http(401, "unauthorized"),
                signal: true,
                expect_query: true,
            },
            Case {
                name: "凭证待续期 + Scheduled + 无信号",
                reason: QuotaCollectReason::Scheduled,
                error: Error::quota_credential_expired(
                    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                ),
                signal: false,
                expect_query: false,
            },
            Case {
                name: "429 + Scheduled + 恢复信号",
                reason: QuotaCollectReason::Scheduled,
                error: Error::quota_http(429, "429 rate limited"),
                signal: true,
                expect_query: false,
            },
            Case {
                name: "超时 + Scheduled + 恢复信号",
                reason: QuotaCollectReason::Scheduled,
                error: Error::quota_timeout(),
                signal: true,
                expect_query: false,
            },
            Case {
                name: "凭证待续期 + UserInitiated + 恢复信号",
                reason: QuotaCollectReason::UserInitiated,
                error: Error::quota_credential_expired(
                    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                ),
                signal: true,
                expect_query: true,
            },
            Case {
                name: "429 + UserInitiated + 恢复信号",
                reason: QuotaCollectReason::UserInitiated,
                error: Error::quota_http(429, "429 rate limited"),
                signal: true,
                expect_query: false,
            },
        ];

        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = cache_path(&dir);
            let mut cache = QuotaCache::default();
            cache.record_failure("kimi", "alice", &case.error);
            cache.save(&path);

            let provider = ScriptedProvider::new(
                Duration::ZERO,
                &[(
                    "alice",
                    Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
                )],
            );
            if case.signal {
                provider.mark_credential_changed("alice");
            }
            let outcome = collect_detailed(
                provider.clone(),
                "kimi",
                &ids(&["alice"]),
                &path,
                case.reason,
            )
            .await;
            let expected_calls = usize::from(case.expect_query);
            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                expected_calls,
                "{}",
                case.name
            );
            assert_eq!(outcome.stats.queried, expected_calls, "{}", case.name);
            if case.expect_query {
                assert_eq!(outcome.stats.skipped_auth, 0, "{}", case.name);
                assert_eq!(outcome.stats.skipped_rate_limit, 0, "{}", case.name);
            }
        }
    }

    #[tokio::test]
    async fn scheduled_recovery_signal_queries_only_signaled_auth_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.record_failure(
            "kimi",
            "alice",
            &Error::quota_credential_expired(
                "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
            ),
        );
        cache.record_failure(
            "kimi",
            "bob",
            &Error::quota_credential_expired(
                "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
            ),
        );
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[
                (
                    "alice",
                    Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
                ),
                ("bob", Ok(vec![sample("bob", Some("LEVEL_ADVANCED"), None)])),
            ],
        );
        provider.mark_credential_changed("alice");
        let outcome = collect_detailed(
            provider.clone(),
            "kimi",
            &ids(&["alice", "bob"]),
            &path,
            QuotaCollectReason::Scheduled,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.stats.queried, 1);
        assert_eq!(outcome.stats.skipped_auth, 1);
        assert!(matches!(outcome.snapshots[0].state, QuotaState::Ready(_)));
        match &outcome.snapshots[1].state {
            QuotaState::Failed(failure) => {
                assert_eq!(failure.kind, QuotaFailureKind::CredentialExpired);
            }
            other => panic!("expected bob still 凭证待续期, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn scheduled_recovery_signal_requery_keeps_credential_expired_on_still_401() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.record_failure(
            "kimi",
            "alice",
            &Error::quota_credential_expired(
                "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
            ),
        );
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Err(Error::quota_credential_expired(
                    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                )),
            )],
        );
        provider.mark_credential_changed("alice");
        let outcome = collect_detailed(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::Scheduled,
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.stats.queried, 1);
        assert_eq!(outcome.stats.skipped_auth, 0);
        match &outcome.snapshots[0].state {
            QuotaState::Failed(failure) => {
                assert_eq!(failure.kind, QuotaFailureKind::CredentialExpired);
                assert_eq!(failure.status, Some(401));
            }
            other => panic!("expected credential expired, got {other:?}"),
        }

        let written = QuotaCache::load(&path);
        let recorded = written
            .in_failure_backoff(
                "kimi",
                "alice",
                Duration::from_secs(90),
                Duration::from_secs(900),
            )
            .expect("failure record kept");
        assert_eq!(recorded.kind, QuotaFailureKind::CredentialExpired);
        assert_eq!(recorded.consecutive, 2);
    }

    #[tokio::test]
    async fn scheduled_recovery_signal_is_one_shot_after_requery_still_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let failed_at = Utc::now() - ChronoDuration::seconds(60);
        let mtime = Utc::now() - ChronoDuration::seconds(30);
        std::fs::write(
            &path,
            serde_json::json!({
                "entries": {},
                "failures": {
                    "kimi::alice": {
                        "failed_at": failed_at,
                        "consecutive": 1,
                        "error": "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                        "kind": "credential_expired",
                        "status": 401
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[(
                "alice",
                Err(Error::quota_credential_expired(
                    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                )),
            )],
        );
        provider.mark_credential_changed_at("alice", mtime);

        let first = collect_detailed(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::Scheduled,
        )
        .await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.stats.queried, 1);
        assert_eq!(first.stats.skipped_auth, 0);

        let second = collect_detailed(
            provider.clone(),
            "kimi",
            &ids(&["alice"]),
            &path,
            QuotaCollectReason::Scheduled,
        )
        .await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.stats.queried, 0);
        assert_eq!(second.stats.skipped_auth, 1);
        match &second.snapshots[0].state {
            QuotaState::Failed(failure) => {
                assert_eq!(failure.kind, QuotaFailureKind::CredentialExpired);
            }
            other => panic!("expected still 凭证待续期, got {other:?}"),
        }
    }

    /// 把缓存里给定账号的成功缓存年龄拨到指定时长（聚焦轮时间线模拟用）。
    fn age_all(path: &Path, ids: &[&str], age: ChronoDuration) {
        let mut cache = QuotaCache::load(path);
        for id in ids {
            cache.age_success("kimi", id, age);
        }
        cache.save(path);
    }

    /// 聚焦轮（issue #161）基本语义：只查当前账号；目标账号 20s 旧缓存（旧 90s
    /// 节流窗内本会跳过）因节流放宽到 10s 而真实落网；停泊账号零查询、行来自
    /// 窗内缓存（非 stale）；快照仍覆盖全部账号。
    #[tokio::test]
    async fn focus_round_queries_only_focus_account_with_relaxed_throttle() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[
                ("alice", Ok(vec![sample("alice", None, None)])),
                ("bob", Ok(vec![sample("bob", None, None)])),
                ("carol", Ok(vec![sample("carol", None, None)])),
            ],
        );
        let all = ids(&["alice", "bob", "carol"]);
        // 冷启动全量轮：三账号各查一次，建立成功缓存。
        let full = collect_detailed(
            provider.clone(),
            "kimi",
            &all,
            &path,
            QuotaCollectReason::Scheduled,
        )
        .await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
        assert_eq!(full.stats.skipped_out_of_focus, 0);

        age_all(&path, &["alice", "bob", "carol"], ChronoDuration::seconds(20));
        let focus_id = AccountId("alice".into());
        let focused = collect_detailed_focused(
            provider.clone(),
            "kimi",
            &all,
            &path,
            QuotaCollectReason::Scheduled,
            Some(&focus_id),
        )
        .await;
        // 请求计数证明：本轮只新增 alice 一次查询。
        assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
        assert_eq!(provider.calls_for("alice"), 2);
        assert_eq!(provider.calls_for("bob"), 1);
        assert_eq!(provider.calls_for("carol"), 1);
        assert_eq!(focused.stats.queried, 1);
        assert_eq!(focused.stats.skipped_out_of_focus, 2);
        assert_eq!(focused.snapshots.len(), 3);
        assert!(matches!(focused.snapshots[0].state, QuotaState::Ready(_)));
        assert!(matches!(focused.snapshots[1].state, QuotaState::Cached(_)));
        assert!(!focused.snapshots[1].stale);
        assert!(matches!(focused.snapshots[2].state, QuotaState::Cached(_)));
        assert!(!focused.snapshots[2].stale);
    }

    /// 10s 是节流下限不是零：目标账号缓存 5s（< 10s）时聚焦轮同样跳过。
    #[tokio::test]
    async fn focus_round_skips_focus_account_within_10s_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[("alice", Ok(vec![sample("alice", None, None)]))],
        );
        let all = ids(&["alice"]);
        let _ = collect_detailed(
            provider.clone(),
            "kimi",
            &all,
            &path,
            QuotaCollectReason::Scheduled,
        )
        .await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        age_all(&path, &["alice"], ChronoDuration::seconds(5));
        let focus_id = AccountId("alice".into());
        let focused = collect_detailed_focused(
            provider.clone(),
            "kimi",
            &all,
            &path,
            QuotaCollectReason::Scheduled,
            Some(&focus_id),
        )
        .await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(focused.stats.queried, 0);
        assert_eq!(focused.stats.skipped_success_throttle, 1);
        assert!(matches!(focused.snapshots[0].state, QuotaState::Cached(_)));
        assert!(!focused.snapshots[0].stale);
    }

    /// 节流放宽的请求量证明（issue #161）：模拟 90s 时间线——t=0 全量轮、
    /// t=10..80 共 8 轮聚焦轮、t=90 全量轮。当前账号 alice = 9 次（冷启动 1 +
    /// 聚焦 8，10s 档节奏）；停泊账号 bob / carol 各 2 次（冷启动 + 90s 全量轮），
    /// 与改造前纯 90s 全量节奏完全一致——节流放宽没有给停泊账号增加任何请求。
    #[tokio::test]
    async fn focus_round_timeline_keeps_parked_request_volume_at_full_round_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[
                ("alice", Ok(vec![sample("alice", None, None)])),
                ("bob", Ok(vec![sample("bob", None, None)])),
                ("carol", Ok(vec![sample("carol", None, None)])),
            ],
        );
        let all = ids(&["alice", "bob", "carol"]);
        let focus_id = AccountId("alice".into());

        // t=0 全量轮（冷启动，三账号各 1 次）。
        let _ = collect_detailed(
            provider.clone(),
            "kimi",
            &all,
            &path,
            QuotaCollectReason::Scheduled,
        )
        .await;

        // t=10..80：每 10s 一轮聚焦轮，共 8 轮；每轮只查 alice。
        for tick in 1..=8i64 {
            // alice 每轮被重写，只需补 10s 年龄；停泊账号缓存自 t=0 起累计变旧。
            age_all(&path, &["alice"], ChronoDuration::seconds(10));
            age_all(&path, &["bob", "carol"], ChronoDuration::seconds(10 * tick));
            let focused = collect_detailed_focused(
                provider.clone(),
                "kimi",
                &all,
                &path,
                QuotaCollectReason::Scheduled,
                Some(&focus_id),
            )
            .await;
            assert_eq!(focused.stats.queried, 1, "tick {tick}");
            assert_eq!(focused.stats.skipped_out_of_focus, 2, "tick {tick}");
        }
        assert_eq!(provider.calls_for("alice"), 9);
        assert_eq!(provider.calls_for("bob"), 1);
        assert_eq!(provider.calls_for("carol"), 1);

        // t=90 全量轮：alice 缓存 10s（< 90s）跳过；停泊账号到龄各补 1 次。
        age_all(&path, &["alice"], ChronoDuration::seconds(10));
        age_all(&path, &["bob", "carol"], ChronoDuration::seconds(90));
        let full = collect_detailed(
            provider.clone(),
            "kimi",
            &all,
            &path,
            QuotaCollectReason::Scheduled,
        )
        .await;
        assert_eq!(provider.calls_for("alice"), 9);
        assert_eq!(provider.calls_for("bob"), 2);
        assert_eq!(provider.calls_for("carol"), 2);
        assert_eq!(full.stats.queried, 2);
        assert_eq!(full.stats.skipped_success_throttle, 1);
        assert_eq!(full.stats.skipped_out_of_focus, 0);
    }

    /// 停泊账号缓存已过 90s 窗且无失败记录：聚焦轮仍零查询，行回退 stale
    /// Cached（展示层降级为「缓存于」），等下一个全量轮刷新。
    #[tokio::test]
    async fn focus_round_serves_stale_cached_row_beyond_window_without_query() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set("kimi", "bob", vec![sample("bob", None, None)]);
        cache.age_success("kimi", "bob", ChronoDuration::seconds(120));
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[
                ("alice", Ok(vec![sample("alice", None, None)])),
                ("bob", Ok(vec![sample("bob", None, None)])),
            ],
        );
        let all = ids(&["alice", "bob"]);
        let focus_id = AccountId("alice".into());
        let focused = collect_detailed_focused(
            provider.clone(),
            "kimi",
            &all,
            &path,
            QuotaCollectReason::Scheduled,
            Some(&focus_id),
        )
        .await;

        assert_eq!(provider.calls_for("alice"), 1);
        assert_eq!(provider.calls_for("bob"), 0);
        assert!(focused.snapshots[1].stale);
        assert!(matches!(focused.snapshots[1].state, QuotaState::Cached(_)));
        assert_eq!(cached_quotas(&focused.snapshots[1].state)[0].used, 1);
    }

    /// 停泊账号在鉴权退避（凭证待续期）中：聚焦轮零查询，行保留类型化
    /// CredentialExpired——前端「任一行待续期 → 5s 档」的判定信号不因聚焦丢失。
    #[tokio::test]
    async fn focus_round_preserves_credential_expired_backoff_row_without_query() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        let mut cache = QuotaCache::default();
        cache.set("kimi", "alice", vec![sample("alice", None, None)]);
        cache.age_success("kimi", "alice", ChronoDuration::seconds(5));
        cache.record_failure(
            "kimi",
            "bob",
            &Error::quota_credential_expired(
                "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
            ),
        );
        cache.save(&path);

        let provider = ScriptedProvider::new(
            Duration::ZERO,
            &[
                ("alice", Ok(vec![sample("alice", None, None)])),
                ("bob", Ok(vec![sample("bob", None, None)])),
            ],
        );
        let all = ids(&["alice", "bob"]);
        let focus_id = AccountId("alice".into());
        let focused = collect_detailed_focused(
            provider.clone(),
            "kimi",
            &all,
            &path,
            QuotaCollectReason::Scheduled,
            Some(&focus_id),
        )
        .await;

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(focused.stats.queried, 0);
        assert_eq!(focused.stats.skipped_success_throttle, 1);
        assert_eq!(focused.stats.skipped_out_of_focus, 1);
        match &focused.snapshots[1].state {
            QuotaState::Failed(failure) => {
                assert_eq!(failure.kind, QuotaFailureKind::CredentialExpired);
                assert_eq!(failure.status, Some(401));
            }
            other => panic!("expected bob still 凭证待续期, got {other:?}"),
        }
    }

    /// 聚焦不改变 ADR-0009：目标账号在鉴权退避中仍被跳过（无恢复信号），
    /// 有恢复信号则放行真实查询。
    #[tokio::test]
    async fn focus_round_keeps_auth_backoff_and_recovery_signal_for_focus() {
        let all = ids(&["alice"]);
        for recovered in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = cache_path(&dir);
            let mut cache = QuotaCache::default();
            cache.record_failure(
                "kimi",
                "alice",
                &Error::quota_credential_expired(
                    "凭证已过期：在 Kimi CLI 里发条消息即可恢复，或重新授权",
                ),
            );
            cache.save(&path);

            let provider = ScriptedProvider::new(
                Duration::ZERO,
                &[(
                    "alice",
                    Ok(vec![sample("alice", Some("LEVEL_ADVANCED"), None)]),
                )],
            );
            if recovered {
                provider.mark_credential_changed("alice");
            }
            let focus_id = AccountId("alice".into());
            let focused = collect_detailed_focused(
                provider.clone(),
                "kimi",
                &all,
                &path,
                QuotaCollectReason::Scheduled,
                Some(&focus_id),
            )
            .await;
            let expected = usize::from(recovered);
            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                expected,
                "recovered={recovered}"
            );
            assert_eq!(focused.stats.queried, expected, "recovered={recovered}");
        }
    }

    fn utc(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn quota_at(id: &str, reset_at: Option<DateTime<Utc>>) -> Quota {
        Quota {
            provider: "kimi".into(),
            account_id: AccountId(id.into()),
            window: QuotaWindow::FiveHour,
            used: 99,
            limit: 100,
            reset_at,
            status: QuotaStatus::Ok,
            note: None,
            membership: None,
        }
    }

    fn snap(id: &str, stale: bool, state: QuotaState) -> QuotaSnapshot {
        QuotaSnapshot {
            account_id: AccountId(id.into()),
            fetched_at: utc("2026-09-03T12:00:00Z"),
            stale,
            membership: None,
            state,
        }
    }

    fn ready_at(id: &str, reset_at: Option<DateTime<Utc>>) -> QuotaSnapshot {
        snap(id, false, QuotaState::Ready(vec![quota_at(id, reset_at)]))
    }

    fn account_with_extra(id: &str, extra_pairs: &[(&str, &str)]) -> crate::model::Account {
        let extra = extra_pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), serde_json::json!(v)))
            .collect();
        crate::model::Account {
            provider: "kimi".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active: false,
            created_at: utc("2026-09-03T12:00:00Z"),
            last_used_at: None,
            extra,
        }
    }

    /// 官方 /usages 移除 user 对象后（2026-09-14）：缺档位快照回退 extra 缓存，
    /// 自带档位（旧缓存行）优先，未知账号 / 无缓存保持 None。
    #[test]
    fn apply_cached_membership_fills_missing_tier_from_extra() {
        let mut with_cache = ready_at("alice", None);
        with_cache.membership = None;
        let mut with_own = ready_at("bob", None);
        with_own.membership = Some("LEVEL_ADVANCED".into());
        let mut unknown = ready_at("carol", None);
        unknown.membership = None;
        let mut snapshots = vec![with_cache, with_own.clone(), unknown];
        let accounts = vec![
            account_with_extra("alice", &[("official_membership", "LEVEL_INTERMEDIATE")]),
            account_with_extra("bob", &[("official_membership", "LEVEL_ANDANTE")]),
            account_with_extra("carol", &[]),
        ];
        apply_cached_membership(&mut snapshots, &accounts);
        assert_eq!(
            snapshots[0].membership.as_deref(),
            Some("LEVEL_INTERMEDIATE"),
            "缺档位回退 extra 缓存"
        );
        assert_eq!(
            snapshots[1].membership.as_deref(),
            Some("LEVEL_ADVANCED"),
            "快照自带档位优先，不被 extra 覆盖"
        );
        assert_eq!(snapshots[2].membership, None, "无缓存保持未知");
    }

    /// 可用窗口取 `reset_at` 最小值；Failed / stale Cached 不计入。
    #[test]
    fn earliest_reset_at_min_of_usable_windows() {
        let early = utc("2026-09-03T16:00:00Z");
        let late = utc("2026-09-03T20:00:00Z");
        let later = utc("2026-09-04T08:00:00Z");
        let mut two_windows = quota_at("carol", Some(later));
        two_windows.window = QuotaWindow::SevenDay;
        assert_eq!(
            earliest_reset_at(&[
                ready_at("alice", Some(late)),
                ready_at("bob", Some(early)),
                snap(
                    "carol",
                    false,
                    QuotaState::Ready(vec![quota_at("carol", Some(later)), two_windows]),
                ),
            ]),
            Some(early)
        );
    }

    #[test]
    fn earliest_reset_at_includes_fresh_cached_not_failed_or_stale() {
        let cached_early = utc("2026-09-03T15:00:00Z");
        let stale_earlier = utc("2026-09-03T10:00:00Z");
        let ready_late = utc("2026-09-03T18:00:00Z");
        assert_eq!(
            earliest_reset_at(&[
                snap(
                    "cached",
                    false,
                    QuotaState::Cached(vec![quota_at("cached", Some(cached_early))]),
                ),
                snap(
                    "stale",
                    true,
                    QuotaState::Cached(vec![quota_at("stale", Some(stale_earlier))]),
                ),
                snap(
                    "failed",
                    false,
                    QuotaState::Failed(QuotaFailure::other("quota fetch failed")),
                ),
                ready_at("ready", Some(ready_late)),
            ]),
            Some(cached_early)
        );
    }

    #[test]
    fn earliest_reset_at_none_when_usable_windows_lack_reset() {
        assert_eq!(earliest_reset_at(&[]), None);
        assert_eq!(
            earliest_reset_at(&[
                ready_at("alice", None),
                snap(
                    "stale",
                    true,
                    QuotaState::Cached(vec![quota_at("stale", Some(utc("2026-09-03T10:00:00Z")))]),
                ),
                snap(
                    "failed",
                    false,
                    QuotaState::Failed(QuotaFailure::other("quota fetch failed")),
                ),
            ]),
            None
        );
    }
}
