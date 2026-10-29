//! 自动切换串行化锁（ADR-0020「自动切换串行化锁」）。
//!
//! 全部自动触发——GUI 快照编排、CLI status 自动切换、配额失败被动切换——共用这一把排他文件锁
//! （[`crate::paths::AppPaths::auto_swap_lock_file`]），把「重读状态 → 判定 → 带期望当前账号的
//! 切换 → 写冷却、往返记忆与回驻确认计数」整段排成队列：后到者在锁内重读自动切换状态，看到
//! 前一个持锁者写下的冷却即自然落空，不会在 [提交, 写冷却] 间隙里把耗尽证据错按在新当前账号
//! 上再切一跳。
//!
//! - 锁顺序固定为本锁 → 交换锁（registry 目录的 `.kimi-switch.lock`）。手动切换只拿交换锁，
//!   不经本锁；本锁的临界区里不做网络请求。
//! - 文件名沿用 #162 被动串行化锁的 `passive-swap.lock`，免迁移：GUI 与 hook 所调 CLI 版本
//!   不一致时，新旧进程仍在同一个文件上排队。
//! - 等待上限对齐交换锁的 3 秒；等不到视为另一自动触发处理中，返回 `None`，调用方按各自口径
//!   放弃（GUI 本轮静默跳过，CLI status 与被动留痕记 `busy`）。打不开或锁不上的 IO 故障同样
//!   返回 `None` 并记 warn：自动切换宁可这一轮不做，也不在锁外判定。
//! - 文件锁随 guard drop 或进程退出释放；guard 不绑定线程，可以跨 `.await` 持有。

use std::fs::File;
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::switch_lock::{lock_within_wait_limit, open_lock_file, LockWaitError};

/// 锁文件名，沿用 #162 被动串行化锁的旧名（免迁移）。
pub const LOCK_FILE_NAME: &str = "passive-swap.lock";

/// 持有期间独占自动切换串行化锁，drop 即释放。
///
/// 须绑定到具名变量（如 `let Some(_serialized) = … else { … }`）持有到临界区结束；
/// `let Some(_) = …` 不绑定，锁会在该语句结束时立即释放。
pub struct AutoSwapLock(File);

impl AutoSwapLock {
    /// 阻塞获取：锁被他人持有时轮询等待，最多 3 秒（与交换锁同一个等待循环）。拿不到返回 `None`。
    ///
    /// 阻塞 IO；异步上下文用 [`Self::acquire_async`]。
    pub fn acquire(path: &Path) -> Option<Self> {
        let file = match open_lock_file(path) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(err = %err, lock = %path.display(), "auto-swap lock unavailable");
                return None;
            }
        };
        match lock_within_wait_limit(&file) {
            Ok(()) => Some(Self(file)),
            Err(LockWaitError::Busy { waited }) => {
                tracing::debug!(
                    lock = %path.display(),
                    waited_ms = waited.as_millis() as u64,
                    "auto-swap lock busy; another automatic swap is in progress"
                );
                None
            }
            Err(LockWaitError::Failed(err)) => {
                tracing::warn!(err = %err, lock = %path.display(), "auto-swap lock failed");
                None
            }
        }
    }

    /// 异步获取：等待放进 `spawn_blocking`，不占住调用方的异步运行时线程，GUI 不因等锁卡顿。
    pub async fn acquire_async(path: PathBuf) -> Option<Self> {
        match tokio::task::spawn_blocking(move || Self::acquire(&path)).await {
            Ok(lock) => lock,
            Err(err) => {
                tracing::warn!(err = %err, "auto-swap lock wait aborted");
                None
            }
        }
    }
}

impl Drop for AutoSwapLock {
    fn drop(&mut self) {
        // 显式解锁；即便遗漏，进程退出时 OS 也会释放文件锁。
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AppPaths;

    /// 锁文件沿用 #162 被动串行化锁的路径 `<state_dir>/passive-swap.lock`，与自动切换状态
    /// 文件同目录：改名会让版本不一致的 GUI 与 hook 所调 CLI 不再互相排队。
    #[test]
    fn lock_file_keeps_the_passive_lock_path() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config_dir: root.path().join("config"),
            data_dir: root.path().join("data"),
            state_dir: root.path().join("data").join("state"),
            cache_dir: root.path().join("cache"),
        };
        assert_eq!(
            paths.auto_swap_lock_file(),
            root.path()
                .join("data")
                .join("state")
                .join("passive-swap.lock")
        );
        assert_eq!(
            paths.auto_swap_lock_file().parent(),
            paths.auto_swap_state_file().parent()
        );
    }

    /// 等锁不占运行时线程：持锁方释放之前，同一个 current-thread 运行时上的别的任务照常推进；
    /// 释放后等待方随即拿到锁。等待若阻塞了运行时线程，下面的释放要等它 3 秒超时后才轮得到，
    /// 等待方只会拿到 `None`。
    #[tokio::test(flavor = "current_thread")]
    async fn acquire_async_waits_off_the_runtime_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_FILE_NAME);
        let held = AutoSwapLock::acquire(&path).expect("空闲时立即拿到");

        let waiter = tokio::spawn(AutoSwapLock::acquire_async(path.clone()));
        // 让等待方先跑起来，进入阻塞线程池里的等待。
        tokio::task::yield_now().await;
        drop(held);

        assert!(
            waiter.await.unwrap().is_some(),
            "持锁方释放后等待方应拿到锁"
        );
    }
}
