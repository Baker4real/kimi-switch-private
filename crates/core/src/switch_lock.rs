//! 账号注册表写操作与原子切换共用的文件锁。
//!
//! 锁文件为 `<lock_dir>/.kimi-switch.lock`。`fs2` 排他锁通常不可重入，
//! 因此本模块按线程记录持锁深度：`swap_with_snapshot` 持锁期间
//! `commit` → `AccountRegistry::set_active` → `save` 不得再抢同一把锁。
//!
//! 等待上限（issue #153）：获取排他锁最多等 [`LOCK_WAIT_LIMIT`]（3 秒），
//! 超时返回 [`Error::SwitchLockTimeout`]（「另一切换操作未完成，请重试」），
//! 不再无限阻塞并发操作。

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;

use crate::error::{Error, Result};

pub const LOCK_FILE_NAME: &str = ".kimi-switch.lock";

/// 排他锁等待上限：超时视为另一处切换卡住，返回明确错误而非无限等待。
const LOCK_WAIT_LIMIT: Duration = Duration::from_secs(3);
/// 等待期间的轮询间隔；总等待最多超出一个间隔。
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(20);

thread_local! {
    static HELD: RefCell<Option<Held>> = const { RefCell::new(None) };
}

struct Held {
    path: PathBuf,
    file: File,
    depth: u32,
}

pub fn lock_path(lock_dir: &Path) -> PathBuf {
    lock_dir.join(LOCK_FILE_NAME)
}

/// 在 `lock_dir/.kimi-switch.lock` 上进入临界区。同一线程对同一锁文件可重入。
pub fn with_exclusive_lock<T>(lock_dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let path = lock_path(lock_dir);
    enter(&path)?;
    struct Exit;
    impl Drop for Exit {
        fn drop(&mut self) {
            exit();
        }
    }
    let _exit = Exit;
    f()
}

fn paths_match(held: &Path, requested: &Path) -> bool {
    if held == requested {
        return true;
    }
    match (fs::canonicalize(held), fs::canonicalize(requested)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn enter(path: &Path) -> Result<()> {
    HELD.with(|slot| {
        {
            let mut held = slot.borrow_mut();
            if let Some(current) = held.as_mut() {
                if paths_match(&current.path, path) {
                    current.depth += 1;
                    return Ok(());
                }
                return Err(Error::provider(format!(
                    "kimi-switch lock already held for {} while acquiring {}",
                    current.path.display(),
                    path.display()
                )));
            }
        }

        let file = open_lock_file(path)?;
        acquire_exclusive(&file, path)?;
        *slot.borrow_mut() = Some(Held {
            path: path.to_path_buf(),
            file,
            depth: 1,
        });
        Ok(())
    })
}

/// 以 [`LOCK_WAIT_LIMIT`] 为上限获取排他锁：锁被他人持有时轮询等待，
/// 超时返回 [`Error::SwitchLockTimeout`]；非竞争类错误（权限、IO 故障）如实上抛。
fn acquire_exclusive(file: &File, path: &Path) -> Result<()> {
    match lock_within_wait_limit(file) {
        Ok(()) => Ok(()),
        Err(LockWaitError::Busy { waited }) => {
            tracing::warn!(
                lock = %path.display(),
                waited_ms = waited.as_millis() as u64,
                "swap lock wait exceeded limit; giving up"
            );
            Err(Error::SwitchLockTimeout)
        }
        Err(LockWaitError::Failed(e)) => Err(Error::provider(format!(
            "lock {} failed: {e}",
            path.display()
        ))),
    }
}

/// 打开（必要时连同父目录创建）锁文件。交换锁、自动切换串行化锁与自动切换状态文件锁共用。
pub(crate) fn open_lock_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

/// 带等待上限的排他锁没拿到的原因。
pub(crate) enum LockWaitError {
    /// 等满 [`LOCK_WAIT_LIMIT`] 仍被他人持有。
    Busy { waited: Duration },
    /// 非竞争类错误（权限、IO 故障）。
    Failed(io::Error),
}

/// 在 `file` 上取排他锁，最多等 [`LOCK_WAIT_LIMIT`]（3 秒）：被他人持有时每
/// [`LOCK_POLL_INTERVAL`] 重试一次，竞争判据见 [`is_lock_contention`]，其余错误立即返回。
/// 交换锁、自动切换串行化锁与自动切换状态文件锁共用这一个等待循环，各自决定没拿到时怎么办。
pub(crate) fn lock_within_wait_limit(file: &File) -> std::result::Result<(), LockWaitError> {
    let started = Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(e) if is_lock_contention(&e) => {
                if started.elapsed() >= LOCK_WAIT_LIMIT {
                    return Err(LockWaitError::Busy {
                        waited: started.elapsed(),
                    });
                }
                std::thread::sleep(LOCK_POLL_INTERVAL);
            }
            Err(e) => return Err(LockWaitError::Failed(e)),
        }
    }
}

/// fs2 `try_lock_exclusive` 的竞争信号：Unix flock 返回 `EWOULDBLOCK`
/// （std 映射为 `WouldBlock`），Windows `LockFileEx` 返回 `ERROR_LOCK_VIOLATION`
/// （raw os error 33）。其余错误视为真实故障，不参与等待循环。
fn is_lock_contention(err: &io::Error) -> bool {
    const ERROR_LOCK_VIOLATION: i32 = 33;
    err.kind() == io::ErrorKind::WouldBlock || err.raw_os_error() == Some(ERROR_LOCK_VIOLATION)
}

fn exit() {
    HELD.with(|slot| {
        let mut held = slot.borrow_mut();
        let Some(current) = held.as_mut() else {
            return;
        };
        current.depth = current.depth.saturating_sub(1);
        if current.depth == 0 {
            if let Some(current) = held.take() {
                let _ = FileExt::unlock(&current.file);
            }
        }
    });
}
