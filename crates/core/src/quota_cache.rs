//! quota 查询结果的磁盘缓存。
//!
//! 存储在 `cache_dir/quota_cache.json`，按单个 quota 窗口过滤：
//! - 有 `reset_at` 的窗口：reset_at 已过则该窗口数据失效，不返回。
//! - 无 `reset_at` 的窗口：按窗口类型兜底 TTL（5h/7d/30d），超出则失效。
//! - 所有窗口都失效 → 整条 entry 不返回（等同缓存未命中）。
//!
//! 缓存是可丢弃数据，读写失败静默忽略。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, QuotaFailure, QuotaFailureKind};
use crate::model::{Quota, QuotaWindow};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEntry {
    pub quotas: Vec<Quota>,
    pub cached_at: DateTime<Utc>,
}

/// `get()` 返回的有效缓存快照；quotas 已过滤掉过期窗口。
pub struct ValidEntry {
    pub quotas: Vec<Quota>,
    pub cached_at: DateTime<Utc>,
}

/// 连续查询失败的记录，用于失败退避。只存错误摘要，不含任何凭证。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEntry {
    /// 最近一次失败时间。
    pub failed_at: DateTime<Utc>,
    /// 连续失败次数（成功一次即清零）。
    pub consecutive: u32,
    /// 最近一次失败的错误文本，退避期间原样复用给展示层。
    pub error: String,
    /// 额度失败种类。老缓存文件缺此字段时视为 [`QuotaFailureKind::Other`]。
    #[serde(default)]
    pub kind: QuotaFailureKind,
    /// 结构化 HTTP 状态。老缓存文件缺此字段时视为未知。
    #[serde(default)]
    pub status: Option<u16>,
}

impl FailureEntry {
    fn from_error(error: &Error, consecutive: u32) -> Self {
        let failure = quota_failure_of(error);
        Self {
            failed_at: Utc::now(),
            consecutive,
            error: failure.message,
            kind: failure.kind,
            status: failure.status,
        }
    }

    /// 还原为错误类型，分类走 [`Error`] 上的唯一定义。
    pub fn as_error(&self) -> Error {
        Error::quota_fetch(QuotaFailure {
            kind: self.kind,
            status: self.status,
            message: self.error.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuotaCache {
    entries: HashMap<String, CachedEntry>,
    /// 失败退避表。老版本缓存文件没有该字段，`default` 保证可平滑升级。
    #[serde(default)]
    failures: HashMap<String, FailureEntry>,
}

impl QuotaCache {
    /// 从文件加载缓存；文件不存在或解析失败返回空缓存。
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 将缓存写入文件。同目录 tmp+rename，避免并发写撕裂；失败静默忽略（缓存是可丢弃数据）。
    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = save_atomically(path, json.as_bytes());
        }
    }

    /// 获取仍有效的缓存快照，已过期的窗口被过滤。
    /// 若所有窗口都过期或无缓存，返回 None。
    pub fn get(&self, provider: &str, account_id: &str) -> Option<ValidEntry> {
        let entry = self.entries.get(&cache_key(provider, account_id))?;
        let now = Utc::now();
        let valid_quotas: Vec<Quota> = entry
            .quotas
            .iter()
            .filter(|q| !quota_expired(q, entry.cached_at, now))
            .cloned()
            .collect();
        if valid_quotas.is_empty() {
            return None;
        }
        Some(ValidEntry {
            quotas: valid_quotas,
            cached_at: entry.cached_at,
        })
    }

    /// 同 [`get`](Self::get)，但保留 `reset_at` 已过的窗口（原样返回缓存值）；无 `reset_at`
    /// 的窗口仍按兜底 TTL 失效。供只读缓存的自动切换判定（配额失败被动切换）使用：判定按
    /// 「过点重置」把这些窗口当作已重置（ADR-0020），缓存层若先丢掉它们，窗口全部过点的
    /// 账号会整条失去快照、被误判为不可用。展示路径仍用 [`get`](Self::get)。
    pub fn get_keeping_past_resets(&self, provider: &str, account_id: &str) -> Option<ValidEntry> {
        let entry = self.entries.get(&cache_key(provider, account_id))?;
        let now = Utc::now();
        let quotas: Vec<Quota> = entry
            .quotas
            .iter()
            .filter(|q| q.reset_at.is_some() || !quota_expired(q, entry.cached_at, now))
            .cloned()
            .collect();
        if quotas.is_empty() {
            return None;
        }
        Some(ValidEntry {
            quotas,
            cached_at: entry.cached_at,
        })
    }

    /// 若缓存足够新(`cached_at` 距今 < `max_age`)且仍有有效窗口，返回快照；否则 None。
    /// 用于「缓存节流」：够新就直接复用、跳过真实 quota 查询，避免高频打 usage 端点。
    ///
    /// 最近一次失败是鉴权/缺凭据时不复用成功缓存：那些数字往往是串号或已作废令牌留下的，
    /// 继续当 `Ready` 会让「needs re-login」的行看起来像额度耗尽。
    pub fn fresh(
        &self,
        provider: &str,
        account_id: &str,
        max_age: std::time::Duration,
    ) -> Option<ValidEntry> {
        let key = cache_key(provider, account_id);
        if self
            .failures
            .get(&key)
            .is_some_and(|entry| entry.as_error().is_authentication_failure())
        {
            return None;
        }
        let entry = self.entries.get(&key)?;
        let age = Utc::now() - entry.cached_at;
        // age 为负(时钟回拨/未来时间戳)时视为「不新鲜」,保守地重新查询。
        if age < Duration::zero() || age >= Duration::from_std(max_age).ok()? {
            return None;
        }
        self.get(provider, account_id)
    }

    /// 更新或插入缓存条目；同时清掉该账号的失败退避记录。
    pub fn set(&mut self, provider: &str, account_id: &str, quotas: Vec<Quota>) {
        let key = cache_key(provider, account_id);
        self.failures.remove(&key);
        self.entries.insert(
            key,
            CachedEntry {
                quotas,
                cached_at: Utc::now(),
            },
        );
    }

    /// 记一次查询失败，累加连续失败次数。
    ///
    /// 失败结果不进 `entries`（不能拿失败当数据用），单独记在这里只为算退避窗口。
    /// 鉴权失败还会丢掉旧的成功缓存，避免下一屏把过期 0% 当成真实余量。
    pub fn record_failure(&mut self, provider: &str, account_id: &str, error: &Error) {
        let key = cache_key(provider, account_id);
        if error.is_authentication_failure() {
            self.entries.remove(&key);
        }
        let consecutive = self
            .failures
            .get(&key)
            .map(|f| f.consecutive.saturating_add(1))
            .unwrap_or(1);
        self.failures
            .insert(key, FailureEntry::from_error(error, consecutive));
    }

    /// 删除某账号的成功缓存与失败退避（`rm` 时调用，避免尸号数字粘在下一个同 id 导入上）。
    pub fn remove(&mut self, provider: &str, account_id: &str) {
        let key = cache_key(provider, account_id);
        self.entries.remove(&key);
        self.failures.remove(&key);
    }

    /// 该账号是否仍处于失败退避窗口内；是则返回最近一次失败记录，调用方应跳过真实查询。
    ///
    /// 退避时长 = `base * 2^(consecutive - 1)`，封顶 `cap`。
    pub fn in_failure_backoff(
        &self,
        provider: &str,
        account_id: &str,
        base: std::time::Duration,
        cap: std::time::Duration,
    ) -> Option<&FailureEntry> {
        let entry = self.failures.get(&cache_key(provider, account_id))?;
        // 401/403 往往会在原生客户端刚刷新凭据后立刻恢复。仍保留一个基础窗口避免请求风暴，
        // 但不把旧鉴权失败指数退避到 15 分钟，否则用户明明已经恢复登录仍会长期看到旧错误。
        let effective_cap = if entry.as_error().is_authentication_failure() {
            base
        } else {
            cap
        };
        let backoff = failure_backoff(base, effective_cap, entry.consecutive);
        let age = Utc::now() - entry.failed_at;
        // age 为负(时钟回拨)时视为已过窗口，保守地允许重查。
        if age < Duration::zero() || age >= Duration::from_std(backoff).ok()? {
            return None;
        }
        Some(entry)
    }

    /// 把成功缓存的时间戳拨到过去，供测试模拟节流窗口已过。
    /// 跨 crate 的 gui 测试需要，故保持 pub。
    #[doc(hidden)]
    pub fn age_success(&mut self, provider: &str, account_id: &str, age: Duration) {
        if let Some(entry) = self.entries.get_mut(&cache_key(provider, account_id)) {
            entry.cached_at = Utc::now() - age;
        }
    }
}

/// 连续失败 n 次后的退避时长：`base * 2^(n-1)`，封顶 `cap`。
fn failure_backoff(
    base: std::time::Duration,
    cap: std::time::Duration,
    consecutive: u32,
) -> std::time::Duration {
    let shift = consecutive.saturating_sub(1).min(16);
    base.saturating_mul(1_u32 << shift).min(cap.max(base))
}

/// 同目录唯一临时文件 + rename。Unix 创建即 0600。
fn save_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = unique_tmp_path(path);
    let result = (|| {
        write_private(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".tmp.{}.{}", std::process::id(), seq));
    PathBuf::from(name)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

fn cache_key(provider: &str, account_id: &str) -> String {
    format!("{provider}::{account_id}")
}

/// 把任意 [`Error`] 归约成额度失败载荷（分类 / HTTP 状态 / 文案），持久化与展示共用。
pub(crate) fn quota_failure_of(error: &Error) -> QuotaFailure {
    match error {
        Error::QuotaFetch(failure) => failure.clone(),
        Error::Provider(failure) => QuotaFailure {
            kind: if failure.status.is_some() {
                QuotaFailureKind::Http
            } else {
                QuotaFailureKind::Other
            },
            status: failure.status,
            message: failure.message.clone(),
        },
        other => QuotaFailure {
            kind: QuotaFailureKind::Other,
            status: other.http_status(),
            message: other.to_string(),
        },
    }
}

/// 判断单个 quota 窗口是否已失效。
fn quota_expired(q: &Quota, cached_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    if let Some(reset_at) = q.reset_at {
        // 窗口已重置 → 数据失效
        return reset_at <= now;
    }
    // 没有 reset_at：按窗口类型兜底 TTL
    cached_at + window_ttl(q.window) <= now
}

fn window_ttl(window: QuotaWindow) -> Duration {
    match window {
        QuotaWindow::FiveHour | QuotaWindow::Custom => Duration::hours(5),
        QuotaWindow::SevenDay => Duration::days(7),
        QuotaWindow::Month | QuotaWindow::FirstPartyModels | QuotaWindow::Api => Duration::days(30),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountId, QuotaStatus};

    fn sample_quota() -> Quota {
        Quota {
            provider: "alpha".into(),
            account_id: AccountId("a@x.com".into()),
            window: QuotaWindow::SevenDay,
            used: 10,
            limit: 100,
            reset_at: Some(Utc::now() + Duration::days(3)),
            status: QuotaStatus::Ok,
            note: None,
            membership: None,
        }
    }

    #[test]
    fn fresh_returns_only_within_window() {
        let mut cache = QuotaCache::default();
        cache.set("alpha", "a@x.com", vec![sample_quota()]);
        // 刚写入 → 90s 窗口内算新鲜。
        assert!(cache
            .fresh("alpha", "a@x.com", std::time::Duration::from_secs(90))
            .is_some());
        // 0 窗口 → 任何缓存都视为不新鲜,强制重新查询。
        assert!(cache
            .fresh("alpha", "a@x.com", std::time::Duration::from_secs(0))
            .is_none());
        // 未知账号 → None。
        assert!(cache
            .fresh("alpha", "b@x.com", std::time::Duration::from_secs(90))
            .is_none());
    }

    #[test]
    fn failure_backoff_grows_and_clears_on_success() {
        let base = std::time::Duration::from_secs(90);
        let cap = std::time::Duration::from_secs(900);
        let mut cache = QuotaCache::default();

        // 首次失败 → 退避 base，窗口内跳过查询。
        cache.record_failure(
            "alpha",
            "a@x.com",
            &Error::quota_http(429, "429 rate limited"),
        );
        let hit = cache
            .in_failure_backoff("alpha", "a@x.com", base, cap)
            .unwrap();
        assert_eq!(hit.consecutive, 1);
        assert_eq!(hit.error, "429 rate limited");
        assert_eq!(hit.status, Some(429));
        assert!(!hit.as_error().is_authentication_failure());

        // 第 3 次失败 → 退避 4*base；把 failed_at 拨到 3*base 之前仍在窗口内。
        cache.record_failure(
            "alpha",
            "a@x.com",
            &Error::quota_http(429, "429 rate limited"),
        );
        cache.record_failure(
            "alpha",
            "a@x.com",
            &Error::quota_http(429, "429 rate limited"),
        );
        cache.failures.get_mut("alpha::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(270);
        assert!(cache
            .in_failure_backoff("alpha", "a@x.com", base, cap)
            .is_some());

        // 超过封顶时长 → 放行重查。
        cache.failures.get_mut("alpha::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(1000);
        assert!(cache
            .in_failure_backoff("alpha", "a@x.com", base, cap)
            .is_none());

        // 查成功一次即清零，不再退避。
        cache.set("alpha", "a@x.com", vec![sample_quota()]);
        assert!(cache
            .in_failure_backoff("alpha", "a@x.com", base, cap)
            .is_none());
    }

    #[test]
    fn authentication_failure_backoff_is_capped_at_base_interval() {
        let base = std::time::Duration::from_secs(90);
        let cap = std::time::Duration::from_secs(900);
        let mut cache = QuotaCache::default();

        for _ in 0..5 {
            cache.record_failure(
                "alpha",
                "a@x.com",
                &Error::quota_http(401, "wording that never mentions the status"),
            );
        }
        cache.failures.get_mut("alpha::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(89);
        assert!(cache
            .in_failure_backoff("alpha", "a@x.com", base, cap)
            .is_some());

        cache.failures.get_mut("alpha::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(91);
        assert!(cache
            .in_failure_backoff("alpha", "a@x.com", base, cap)
            .is_none());
    }

    #[test]
    fn auth_failure_drops_success_cache_and_skips_fresh() {
        let mut cache = QuotaCache::default();
        cache.set("gamma", "auth0|dead", vec![sample_quota()]);
        cache.record_failure(
            "gamma",
            "auth0|dead",
            &Error::quota_http(401, "wording that never mentions the status"),
        );
        assert!(cache.get("gamma", "auth0|dead").is_none());
        assert!(cache
            .fresh("gamma", "auth0|dead", std::time::Duration::from_secs(90))
            .is_none());
    }

    #[test]
    fn remove_clears_success_and_failure_entries() {
        let mut cache = QuotaCache::default();
        cache.set("gamma", "auth0|gone", vec![sample_quota()]);
        cache.record_failure(
            "gamma",
            "auth0|gone",
            &Error::quota_http(429, "429 rate limited"),
        );
        cache.remove("gamma", "auth0|gone");
        assert!(cache.get("gamma", "auth0|gone").is_none());
        assert!(cache
            .in_failure_backoff(
                "gamma",
                "auth0|gone",
                std::time::Duration::from_secs(90),
                std::time::Duration::from_secs(900)
            )
            .is_none());
    }

    #[test]
    fn failure_backoff_is_capped() {
        let base = std::time::Duration::from_secs(90);
        let cap = std::time::Duration::from_secs(900);
        assert_eq!(failure_backoff(base, cap, 1), base);
        assert_eq!(failure_backoff(base, cap, 3), base * 4);
        // 2^(20-1) * 90s 远超封顶,必须被 cap 住。
        assert_eq!(failure_backoff(base, cap, 20), cap);
    }

    #[test]
    fn fresh_rejects_stale_entry() {
        let mut cache = QuotaCache::default();
        cache.set("alpha", "a@x.com", vec![sample_quota()]);
        // 手动把 cached_at 拨到 5 分钟前 → 超过 90s 窗口,不新鲜。
        if let Some(entry) = cache.entries.get_mut("alpha::a@x.com") {
            entry.cached_at = Utc::now() - Duration::minutes(5);
        }
        assert!(cache
            .fresh("alpha", "a@x.com", std::time::Duration::from_secs(90))
            .is_none());
    }

    /// 判定用读取（issue #191 / ADR-0020）：`reset_at` 已过的窗口原样保留（展示用的 `get`
    /// 照旧丢掉）；无 `reset_at` 的窗口仍按兜底 TTL 失效；一个窗口都不剩时返回 None。
    #[test]
    fn get_keeping_past_resets_keeps_reset_passed_windows_only() {
        let passed = Utc::now() - Duration::hours(1);
        let mut reset_passed = sample_quota();
        reset_passed.reset_at = Some(passed);
        let mut no_reset = sample_quota();
        no_reset.window = QuotaWindow::FiveHour;
        no_reset.reset_at = None;

        let mut cache = QuotaCache::default();
        cache.set("alpha", "a@x.com", vec![reset_passed, no_reset.clone()]);
        // 6 小时前写入：无 reset_at 的 5 小时窗已过兜底 TTL。
        cache.age_success("alpha", "a@x.com", Duration::hours(6));
        assert!(cache.get("alpha", "a@x.com").is_none(), "展示路径照旧丢掉");
        let entry = cache
            .get_keeping_past_resets("alpha", "a@x.com")
            .expect("reset_at 已过的窗口应保留");
        assert_eq!(entry.quotas.len(), 1, "TTL 过期的无 reset_at 窗口仍丢掉");
        assert_eq!(entry.quotas[0].reset_at, Some(passed));
        assert_eq!(entry.quotas[0].used, 10, "原样返回缓存值，不改写");

        let mut ttl_only = QuotaCache::default();
        ttl_only.set("alpha", "b@x.com", vec![no_reset]);
        ttl_only.age_success("alpha", "b@x.com", Duration::hours(6));
        assert!(ttl_only
            .get_keeping_past_resets("alpha", "b@x.com")
            .is_none());
        assert!(cache
            .get_keeping_past_resets("alpha", "nobody@x.com")
            .is_none());
    }

    fn fat_quotas(n: usize) -> Vec<Quota> {
        (0..n).map(|_| sample_quota()).collect()
    }

    /// GUI/CLI 会并发 `save`；直接 `fs::write` 会把文件撕成半截 JSON。
    /// 半截文件被 `load` 当成空缓存，失败退避表消失，调用方会立刻重新打网。
    #[test]
    fn concurrent_saves_keep_valid_json_and_backoff_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota_cache.json");

        let mut writer_a = QuotaCache::default();
        writer_a.record_failure(
            "alpha",
            "writer-a",
            &Error::quota_http(429, "429 rate limited"),
        );
        writer_a.set("alpha", "acct-a", fat_quotas(24));
        writer_a.save(&path);

        let mut writer_b = QuotaCache::default();
        writer_b.record_failure(
            "alpha",
            "writer-b",
            &Error::quota_http(429, "429 rate limited"),
        );
        writer_b.set("alpha", "acct-b", fat_quotas(48));

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let torn = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let watch_path = path.clone();
        let watch_stop = stop.clone();
        let watch_torn = torn.clone();
        let watcher = std::thread::spawn(move || {
            while !watch_stop.load(std::sync::atomic::Ordering::Relaxed) {
                match std::fs::read_to_string(&watch_path) {
                    Ok(raw) => {
                        if raw.is_empty()
                            || serde_json::from_str::<serde_json::Value>(&raw).is_err()
                        {
                            *watch_torn.lock().unwrap() = Some(raw);
                            break;
                        }
                    }
                    Err(_) => {}
                }
            }
        });

        let path_a = path.clone();
        let path_b = path.clone();
        let handle_a = std::thread::spawn(move || {
            for _ in 0..300 {
                writer_a.save(&path_a);
            }
        });
        let handle_b = std::thread::spawn(move || {
            for _ in 0..300 {
                writer_b.save(&path_b);
            }
        });
        handle_a.join().unwrap();
        handle_b.join().unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        watcher.join().unwrap();

        if let Some(snippet) = torn.lock().unwrap().as_ref() {
            panic!("concurrent saves tore cache into invalid JSON; snippet={snippet:?}");
        }

        let raw =
            std::fs::read_to_string(&path).expect("cache file should exist after concurrent saves");
        serde_json::from_str::<serde_json::Value>(&raw).unwrap_or_else(|e| {
            panic!("concurrent saves tore cache into invalid JSON: {e}; snippet={raw:?}");
        });

        let loaded = QuotaCache::load(&path);
        let base = std::time::Duration::from_secs(90);
        let cap = std::time::Duration::from_secs(900);
        let backoff_a = loaded
            .in_failure_backoff("alpha", "writer-a", base, cap)
            .is_some();
        let backoff_b = loaded
            .in_failure_backoff("alpha", "writer-b", base, cap)
            .is_some();
        assert!(
            backoff_a || backoff_b,
            "torn cache loads as empty and skips failure backoff, which would immediately re-query"
        );
    }

    /// 损坏文件必须自愈为空缓存，不能让调用方当成「有退避」或 panic。
    #[test]
    fn load_heals_corrupt_file_as_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota_cache.json");
        std::fs::write(&path, "{\"entries\":").unwrap();
        let loaded = QuotaCache::load(&path);
        assert!(loaded.get("alpha", "a@x.com").is_none());
        assert!(loaded
            .in_failure_backoff(
                "alpha",
                "a@x.com",
                std::time::Duration::from_secs(90),
                std::time::Duration::from_secs(900)
            )
            .is_none());
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_cache_file_with_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota_cache.json");
        let mut cache = QuotaCache::default();
        cache.set("alpha", "a@x.com", vec![sample_quota()]);
        cache.save(&path);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "quota cache file must be 0600");
    }
}
