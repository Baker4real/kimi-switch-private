//! quota 查询的通用重试包装。
//!
//! Provider 只负责「怎么查」；CLI / daemon 共享这里的超时与重试策略，避免各处各写一套。

use std::time::Duration;

use crate::error::{Error, Result};
use crate::model::{AccountId, Quota};
use crate::provider::Provider;
use crate::settings::{self, Quota as QuotaSettings};

/// 额度重试日志里的 provider 标识。trait 不再暴露 `id()`，由实现内部常量自报（Kimi）。
const PROVIDER_ID: &str = "kimi";

tokio::task_local! {
    /// 本次 attempt 的超时时刻：到点外层超时就会取消 `Provider::query_quota`。
    static ATTEMPT_DEADLINE: tokio::time::Instant;
}

/// 本次额度查询 attempt 距外层超时还剩多久；不在 [`with_attempt_deadline`] 的 attempt 里
/// （没有外层超时会取消本次查询）时返回 `None`。
///
/// 供 Provider 在发出「被取消即丢失」的请求前判断剩余时间够不够：停泊账号遇 401 的强制轮换、
/// 当前账号的 401 协调恢复，盖不住轮换请求的超时上限时都不发（ADR-0019）。在
/// `Provider::query_quota` 外面另加超时的调用方必须经 [`with_attempt_deadline`] 加，否则
/// Provider 会当作没有外层超时。
pub fn attempt_time_left() -> Option<Duration> {
    ATTEMPT_DEADLINE
        .try_with(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
        .ok()
}

/// 在外层超时 `deadline` 下运行一次额度查询 attempt：到点即取消 `attempt` 并返回 `Err(Elapsed)`，
/// 期间经 [`attempt_time_left`] 向 Provider 公布剩余时间。[`query_quota_with_retry`] 的每次
/// attempt 都经它运行；另加外层超时的调用方也必须用它，不能直接套 `tokio::time::timeout`。
pub async fn with_attempt_deadline<F: std::future::Future>(
    deadline: tokio::time::Instant,
    attempt: F,
) -> std::result::Result<F::Output, tokio::time::error::Elapsed> {
    tokio::time::timeout_at(deadline, ATTEMPT_DEADLINE.scope(deadline, attempt)).await
}

/// 按当前配置查询 quota，失败或单次超时后做保守重试。
pub async fn query_quota_with_retry(provider: &dyn Provider, id: &AccountId) -> Result<Vec<Quota>> {
    let cfg = settings::current().quota.clone();
    query_quota_with_retry_config(provider, id, &cfg).await
}

async fn query_quota_with_retry_config(
    provider: &dyn Provider,
    id: &AccountId,
    cfg: &QuotaSettings,
) -> Result<Vec<Quota>> {
    let max_retries = cfg.fetch_retries.min(5);
    let max_attempts = max_retries.saturating_add(1);
    let timeout = Duration::from_millis(cfg.fetch_timeout_ms);

    for attempt in 1..=max_attempts {
        let query = provider.query_quota(id);
        // 超时时刻经 task-local 公布给 Provider（见 [`attempt_time_left`]）；
        // 时刻溢出（超时配置大到离谱）等同不设超时，与 `tokio::time::timeout` 一致。
        let outcome = match tokio::time::Instant::now().checked_add(timeout) {
            Some(deadline) => with_attempt_deadline(deadline, query).await,
            None => Ok(query.await),
        };
        let result = outcome.unwrap_or_else(|_| Err(Error::quota_timeout()));

        match result {
            Ok(quotas) => return Ok(quotas),
            Err(e) if attempt < max_attempts && e.is_retryable() => {
                let retry_delay = retry_delay(cfg.fetch_retry_delay_ms, attempt);
                tracing::debug!(
                    provider = PROVIDER_ID,
                    account = %id,
                    attempt,
                    max_attempts,
                    retry_delay_ms = retry_delay.as_millis(),
                    err = %e,
                    "quota fetch failed; retrying"
                );
                if !retry_delay.is_zero() {
                    tokio::time::sleep(retry_delay).await;
                }
            }
            Err(e) if !e.is_retryable() => return Err(e),
            Err(e) => return Err(e.with_attempt_count(max_attempts)),
        }
    }

    Err(Error::quota_other("quota fetch failed"))
}

fn retry_delay(base_ms: u64, failed_attempt: u32) -> Duration {
    let exponent = failed_attempt.saturating_sub(1).min(63);
    Duration::from_millis(base_ms.saturating_mul(1_u64 << exponent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use crate::provider::ActivateOutcome;

    struct FlakyProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for FlakyProvider {
        async fn activate(&self, _id: &AccountId) -> Result<()> {
            Ok(())
        }

        async fn activate_if_current(
            &self,
            _id: &AccountId,
            _expected_current: &AccountId,
        ) -> Result<ActivateOutcome> {
            Ok(ActivateOutcome::Activated)
        }

        async fn query_quota(&self, _id: &AccountId) -> Result<Vec<Quota>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Err(Error::quota_other("temporary failure"))
            } else {
                Ok(Vec::new())
            }
        }
    }

    #[tokio::test]
    async fn retries_once_after_failure() {
        let provider = FlakyProvider {
            calls: AtomicUsize::new(0),
        };
        let cfg = QuotaSettings {
            fetch_retries: 1,
            fetch_retry_delay_ms: 0,
            ..QuotaSettings::default()
        };

        let result =
            query_quota_with_retry_config(&provider, &AccountId("alice".into()), &cfg).await;

        assert!(result.is_ok());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    struct AlwaysTimeoutProvider;

    #[async_trait]
    impl Provider for AlwaysTimeoutProvider {
        async fn activate(&self, _id: &AccountId) -> Result<()> {
            Ok(())
        }

        async fn activate_if_current(
            &self,
            _id: &AccountId,
            _expected_current: &AccountId,
        ) -> Result<ActivateOutcome> {
            Ok(ActivateOutcome::Activated)
        }

        async fn query_quota(&self, _id: &AccountId) -> Result<Vec<Quota>> {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn final_timeout_reports_attempt_count() {
        let cfg = QuotaSettings {
            fetch_timeout_ms: 1,
            fetch_retries: 1,
            fetch_retry_delay_ms: 0,
            ..QuotaSettings::default()
        };

        let err =
            query_quota_with_retry_config(&AlwaysTimeoutProvider, &AccountId("alice".into()), &cfg)
                .await
                .unwrap_err()
                .to_string();

        assert!(err.contains("after 2 attempts"), "{err}");
    }

    struct AlwaysErrorProvider {
        calls: AtomicUsize,
        error: fn() -> Error,
    }

    #[async_trait]
    impl Provider for AlwaysErrorProvider {
        async fn activate(&self, _id: &AccountId) -> Result<()> {
            Ok(())
        }

        async fn activate_if_current(
            &self,
            _id: &AccountId,
            _expected_current: &AccountId,
        ) -> Result<ActivateOutcome> {
            Ok(ActivateOutcome::Activated)
        }

        async fn query_quota(&self, _id: &AccountId) -> Result<Vec<Quota>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err((self.error)())
        }
    }

    #[tokio::test]
    async fn retries_are_capped_at_five() {
        let provider = AlwaysErrorProvider {
            calls: AtomicUsize::new(0),
            error: || Error::quota_other("temporary failure"),
        };
        let cfg = QuotaSettings {
            fetch_retries: 99,
            fetch_retry_delay_ms: 0,
            ..QuotaSettings::default()
        };

        let err = query_quota_with_retry_config(&provider, &AccountId("alice".into()), &cfg)
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(provider.calls.load(Ordering::SeqCst), 6);
        assert!(err.contains("after 6 attempts"), "{err}");
    }

    #[tokio::test]
    async fn non_retryable_http_failures_are_not_retried() {
        for (status, message) in [
            (401, "usage returned 401 Unauthorized"),
            (403, "usage returned 403 Forbidden"),
            (429, "usage returned 429 Too Many Requests"),
        ] {
            let provider = AlwaysErrorProvider {
                calls: AtomicUsize::new(0),
                error: match status {
                    401 => || Error::quota_http(401, "usage returned 401 Unauthorized"),
                    403 => || Error::quota_http(403, "usage returned 403 Forbidden"),
                    _ => || Error::quota_http(429, "usage returned 429 Too Many Requests"),
                },
            };
            let cfg = QuotaSettings {
                fetch_retries: 5,
                fetch_retry_delay_ms: 0,
                ..QuotaSettings::default()
            };

            let err = query_quota_with_retry_config(&provider, &AccountId("alice".into()), &cfg)
                .await
                .unwrap_err();

            assert_eq!(provider.calls.load(Ordering::SeqCst), 1, "{message}");
            assert_eq!(err.http_status(), Some(status), "{message}");
            assert!(!err.is_retryable(), "{message}");
        }
    }

    #[tokio::test]
    async fn http_status_in_message_text_is_not_classification() {
        let provider = AlwaysErrorProvider {
            calls: AtomicUsize::new(0),
            error: || Error::quota_other("usage returned 401 Unauthorized"),
        };
        let cfg = QuotaSettings {
            fetch_retries: 1,
            fetch_retry_delay_ms: 0,
            ..QuotaSettings::default()
        };

        let err = query_quota_with_retry_config(&provider, &AccountId("alice".into()), &cfg)
            .await
            .unwrap_err();

        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(err.http_status(), None);
        assert!(err.is_retryable());
        assert!(!err.is_authentication_failure());
    }

    /// 在 `query_quota` 里读剩余时间，记下每次 attempt 看到的值。
    struct TimeLeftProbe {
        seen: std::sync::Mutex<Vec<Option<Duration>>>,
    }

    #[async_trait]
    impl Provider for TimeLeftProbe {
        async fn activate(&self, _id: &AccountId) -> Result<()> {
            Ok(())
        }

        async fn activate_if_current(
            &self,
            _id: &AccountId,
            _expected_current: &AccountId,
        ) -> Result<ActivateOutcome> {
            Ok(ActivateOutcome::Activated)
        }

        async fn query_quota(&self, _id: &AccountId) -> Result<Vec<Quota>> {
            tokio::time::sleep(Duration::from_millis(300)).await;
            self.seen.lock().unwrap().push(attempt_time_left());
            Ok(Vec::new())
        }
    }

    /// attempt 里能读到距外层超时的剩余时间（随耗时减少），attempt 外读不到。
    #[tokio::test(start_paused = true)]
    async fn attempt_time_left_is_published_only_inside_an_attempt() {
        let provider = TimeLeftProbe {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let cfg = QuotaSettings {
            fetch_timeout_ms: 1_000,
            ..QuotaSettings::default()
        };

        query_quota_with_retry_config(&provider, &AccountId("alice".into()), &cfg)
            .await
            .unwrap();

        assert_eq!(
            *provider.seen.lock().unwrap(),
            vec![Some(Duration::from_millis(700))]
        );
        assert_eq!(attempt_time_left(), None);
    }

    #[test]
    fn retry_delay_uses_exponential_backoff() {
        let delays: Vec<u128> = (1..=5)
            .map(|attempt| retry_delay(500, attempt).as_millis())
            .collect();
        assert_eq!(delays, vec![500, 1_000, 2_000, 4_000, 8_000]);
    }
}
