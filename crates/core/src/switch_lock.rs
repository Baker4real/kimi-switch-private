//! 账号注册表写操作与原子切换共用的文件锁。
//!
//! 锁文件为 `<lock_dir>/.kimi-switch.lock`。`fs2` 排他锁通常不可重入，
//! 因此本模块按线程记录持锁深度：`swap_with_snapshot` 持锁期间
//! `commit` → `AccountRegistry::set_active` → `save` 不得再抢同一把锁。

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::error::{Error, Result};

pub const LOCK_FILE_NAME: &str = ".kimi-switch.lock";

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

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        file.lock_exclusive()
            .map_err(|e| Error::provider(format!("lock {} failed: {e}", path.display())))?;
        *slot.borrow_mut() = Some(Held {
            path: path.to_path_buf(),
            file,
            depth: 1,
        });
        Ok(())
    })
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
