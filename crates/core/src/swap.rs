//! 通用「切换激活账号」骨架。
//!
//! Provider 不应该自己再写 flock → snapshot → 写文件 → 回滚 这套流程，
//! 用 [`swap_with_snapshot`] 声明「要写哪些目标 + 提交回调」即可。
//!
//! 设计要点：
//! - 阻塞 IO。调用方需要确保自己已在 `tokio::task::spawn_blocking` 内。
//! - 每个目标按顺序：(a) 备份到 snapshot 目录 + 内存；(b) 调用 writer 写新内容。
//! - 任一步失败：把所有 target 回滚到备份状态（原本存在的恢复内容；原本不存在的删除）。
//! - writer 成功后还会调用 `commit`，常见用途是 `AccountRegistry::set_active`。
//!   commit 失败同样回滚已写入的文件。
//! - 锁文件位于账号注册表同目录（`config_dir/.kimi-switch.lock`），
//!   与注册表写操作共用临界区；同一线程可重入，避免 commit 二次加锁死锁。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::{Error, Result};
use crate::paths::AppPaths;

#[cfg(test)]
thread_local! {
    static TEST_NOW: std::cell::Cell<Option<DateTime<Utc>>> = const { std::cell::Cell::new(None) };
}

fn snapshot_now() -> DateTime<Utc> {
    #[cfg(test)]
    if let Some(now) = TEST_NOW.with(|cell| cell.get()) {
        return now;
    }
    Utc::now()
}

/// 在 `root` 下独占创建 `{provider}-{ts}`。目录已存在时加 `-n` 后缀，避免同微秒静默覆盖。
fn unique_snapshot_dir(root: &Path, provider_id: &str, ts: &str) -> Result<PathBuf> {
    fs::create_dir_all(root)?;
    let primary = root.join(format!("{provider_id}-{ts}"));
    if try_create_dir(&primary)? {
        return Ok(primary);
    }
    for n in 1..=1024 {
        let candidate = root.join(format!("{provider_id}-{ts}-{n}"));
        if try_create_dir(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(Error::provider(format!(
        "snapshot dir collision: {}",
        primary.display()
    )))
}

fn try_create_dir(path: &Path) -> Result<bool> {
    match fs::create_dir(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// 写入新内容到目标路径的函数对象。各 Provider 用 [`Box::new`] 自己造，
/// 把序列化、权限设置等细节封进去。
pub type SwapWriter<'a> = Box<dyn FnOnce(&Path) -> Result<()> + Send + 'a>;

/// 一次 swap 中需要原子更新的文件。
pub struct SwapTarget<'a> {
    /// 快照子目录内的文件名（snapshot 用，不需要包含路径）。
    pub snapshot_name: &'a str,
    /// 上游客户端读取的真实路径。
    pub live_path: PathBuf,
    /// 写入函数；只负责把新内容写到 `live_path`。
    pub writer: SwapWriter<'a>,
}

/// 执行一次受保护的切换。
///
/// 参数：
/// - `provider_id`：用于快照目录命名（`<provider>-<ts>`）。
/// - `provider_home`：确保存在的客户端主目录。
/// - `lock_dir`：锁文件目录（生产为账号注册表父目录，与注册表写操作同一临界区）。
/// - `targets`：要原子写入的文件列表。
/// - `commit`：所有文件写入成功后执行的最后一步（典型：把 registry 标 active）。
///
/// 失败时已写入的文件会回滚到调用前的状态。
pub fn swap_with_snapshot<'a, C>(
    provider_id: &str,
    provider_home: &Path,
    lock_dir: &Path,
    targets: Vec<SwapTarget<'a>>,
    commit: C,
) -> Result<()>
where
    C: FnOnce() -> Result<()>,
{
    fs::create_dir_all(provider_home)?;
    crate::switch_lock::with_exclusive_lock(lock_dir, || swap_locked(provider_id, targets, commit))
}

fn swap_locked<'a, C>(provider_id: &str, targets: Vec<SwapTarget<'a>>, commit: C) -> Result<()>
where
    C: FnOnce() -> Result<()>,
{
    // 1. 创建快照目录（微秒时间戳；同微秒碰撞加后缀，不复用目录）。
    let ts = snapshot_now().format("%Y%m%dT%H%M%S%.6fZ").to_string();
    let snap_dir = unique_snapshot_dir(&AppPaths::resolve()?.snapshots_dir(), provider_id, &ts)?;

    // 2. 备份现有文件（原本存在的进 snapshot 目录 + 内存）。
    let mut plans: Vec<TargetPlan> = Vec::with_capacity(targets.len());
    for target in targets {
        let backup = if target.live_path.exists() {
            let raw = fs::read_to_string(&target.live_path)?;
            let snapshot_path = snap_dir.join(target.snapshot_name);
            fs::write(&snapshot_path, &raw)?;
            restrict_permissions(&snapshot_path)?;
            Some(raw)
        } else {
            None
        };
        plans.push(TargetPlan {
            live_path: target.live_path,
            writer: target.writer,
            backup,
        });
    }
    tracing::info!(snapshot = %snap_dir.display(), "pre-swap snapshot saved");

    // 3. 顺序写入；任一失败回滚所有 plan（即使 writer 没跑到也尝试 restore，让状态一致）。
    let backups: Vec<(PathBuf, Option<String>)> = plans
        .iter()
        .map(|p| (p.live_path.clone(), p.backup.clone()))
        .collect();

    for plan in plans {
        let TargetPlan {
            live_path, writer, ..
        } = plan;
        if let Err(e) = writer(&live_path) {
            rollback(&backups);
            return Err(e);
        }
    }

    // 4. 收尾 commit（典型：registry.set_active）。失败同样回滚。
    if let Err(e) = commit() {
        rollback(&backups);
        return Err(e);
    }

    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

struct TargetPlan<'a> {
    live_path: PathBuf,
    writer: SwapWriter<'a>,
    backup: Option<String>,
}

fn rollback(backups: &[(PathBuf, Option<String>)]) {
    for (path, backup) in backups {
        match backup {
            Some(raw) => {
                if let Err(e) = fs::write(path, raw)
                    .and_then(|()| restrict_permissions(path).map_err(std::io::Error::other))
                {
                    tracing::error!(err=%e, path=%path.display(), "rollback restore failed");
                }
            }
            None => {
                // 原本不存在 → 把刚写入的删掉，保持「未登录」状态。
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_registry::AccountRegistry;
    use crate::error::Error;
    use crate::model::{Account, AccountId};
    use std::sync::Mutex;
    use std::time::Duration;

    static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn with_isolated_home<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("KIMI_SWITCH_HOME");
        std::env::set_var("KIMI_SWITCH_HOME", tmp.path());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.0 {
                    Some(value) => std::env::set_var("KIMI_SWITCH_HOME", value),
                    None => std::env::remove_var("KIMI_SWITCH_HOME"),
                }
            }
        }
        let _restore = Restore(previous);
        f(tmp.path())
    }

    fn lock_dir() -> PathBuf {
        AppPaths::resolve().unwrap().config_dir
    }

    fn with_frozen_now<T>(now: DateTime<Utc>, f: impl FnOnce() -> T) -> T {
        let previous = TEST_NOW.with(|cell| cell.replace(Some(now)));
        struct Restore(Option<DateTime<Utc>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                TEST_NOW.with(|cell| cell.set(self.0));
            }
        }
        let _restore = Restore(previous);
        f()
    }

    fn parse_utc(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn snapshot_second_prefix(dir: &Path) -> &str {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap();
        // `{provider}-{YYYYMMDDTHHMMSS}.{frac}Z` 或同微秒后缀 `...Z-n`
        let stamp = name.split_once('-').map(|(_, rest)| rest).unwrap_or(name);
        stamp.split('.').next().unwrap()
    }

    fn write_then_ok(contents: &'static str) -> SwapWriter<'static> {
        Box::new(move |path| {
            fs::write(path, contents)?;
            Ok(())
        })
    }

    fn write_then_err(contents: &'static str, message: &'static str) -> SwapWriter<'static> {
        Box::new(move |path| {
            fs::write(path, contents)?;
            Err(Error::provider(message))
        })
    }

    fn snapshot_dirs(provider_id: &str) -> Vec<PathBuf> {
        let root = AppPaths::resolve().unwrap().snapshots_dir();
        if !root.exists() {
            return Vec::new();
        }
        let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&format!("{provider_id}-")))
            })
            .collect();
        dirs.sort();
        dirs
    }

    #[test]
    fn writer_failure_rolls_back_existing_live_file() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();

            let err = swap_with_snapshot(
                "ticket25",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_err("new", "injected writer failure"),
                }],
                || Ok(()),
            )
            .unwrap_err();
            assert!(err.to_string().contains("injected writer failure"), "{err}");
            assert_eq!(fs::read_to_string(&live).unwrap(), "old");
        });
    }

    #[test]
    fn commit_failure_rolls_back_existing_live_file() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();

            let err = swap_with_snapshot(
                "ticket25",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_ok("new"),
                }],
                || Err(Error::provider("injected commit failure")),
            )
            .unwrap_err();
            assert!(err.to_string().contains("injected commit failure"), "{err}");
            assert_eq!(fs::read_to_string(&live).unwrap(), "old");
        });
    }

    #[test]
    fn rollback_deletes_live_file_that_did_not_exist_before_swap() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            assert!(!live.exists());

            let err = swap_with_snapshot(
                "ticket25",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_ok("new"),
                }],
                || Err(Error::provider("injected commit failure")),
            )
            .unwrap_err();
            assert!(err.to_string().contains("injected commit failure"), "{err}");
            assert!(
                !live.exists(),
                "原本不存在的 live 文件在回滚后应被删除，仍在: {}",
                live.display()
            );
        });
    }

    fn swap_live(home: &Path, live: &Path, contents: &'static str) {
        swap_with_snapshot(
            "ticket25",
            home,
            &lock_dir(),
            vec![SwapTarget {
                snapshot_name: "live",
                live_path: live.to_path_buf(),
                writer: write_then_ok(contents),
            }],
            || Ok(()),
        )
        .unwrap();
    }

    fn assert_independent_snapshots(dirs: &[PathBuf], expected: &[&str]) {
        assert_eq!(
            dirs.len(),
            expected.len(),
            "应留下 {} 个快照目录，实际: {dirs:?}",
            expected.len()
        );
        let mut backed_up: Vec<String> = dirs
            .iter()
            .map(|dir| fs::read_to_string(dir.join("live")).unwrap())
            .collect();
        backed_up.sort();
        let mut expected: Vec<String> = expected.iter().map(|s| (*s).to_string()).collect();
        expected.sort();
        assert_eq!(backed_up, expected);
    }

    #[test]
    fn two_swaps_in_the_same_second_keep_independent_snapshot_dirs() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "v0").unwrap();

            // 冻结到同一秒、不同微秒：秒级命名会撞车，微秒命名必须给出两个目录。
            with_frozen_now(parse_utc("2026-01-02T03:04:05.000100Z"), || {
                swap_live(home, &live, "v1");
            });
            with_frozen_now(parse_utc("2026-01-02T03:04:05.000200Z"), || {
                swap_live(home, &live, "v2");
            });

            let dirs = snapshot_dirs("ticket25");
            assert_independent_snapshots(&dirs, &["v0", "v1"]);
            let prefixes: Vec<&str> = dirs.iter().map(|d| snapshot_second_prefix(d)).collect();
            assert!(
                prefixes.iter().all(|p| *p == "20260102T030405"),
                "两次切换必须落在同一秒: {dirs:?}"
            );
            let names: Vec<String> = dirs
                .iter()
                .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            assert_ne!(
                names[0], names[1],
                "同一秒不同微秒应是两个目录名: {names:?}"
            );
        });
    }

    #[test]
    fn two_swaps_in_the_same_microsecond_keep_independent_snapshot_dirs() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "v0").unwrap();

            with_frozen_now(parse_utc("2026-01-02T03:04:05.000100Z"), || {
                swap_live(home, &live, "v1");
                swap_live(home, &live, "v2");
            });

            let dirs = snapshot_dirs("ticket25");
            assert_independent_snapshots(&dirs, &["v0", "v1"]);
            let names: Vec<String> = dirs
                .iter()
                .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            assert!(
                names
                    .iter()
                    .any(|n| n == "ticket25-20260102T030405.000100Z"),
                "应保留原始微秒目录: {names:?}"
            );
            assert!(
                names
                    .iter()
                    .any(|n| n.starts_with("ticket25-20260102T030405.000100Z-")),
                "同微秒不得 create_dir 复用目录，应加后缀: {names:?}"
            );
        });
    }

    fn sample_account(id: &str) -> Account {
        Account {
            provider: "kimi".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active: false,
            created_at: Utc::now(),
            last_used_at: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn set_active_during_swap_commit_does_not_deadlock() {
        with_isolated_home(|home| {
            let paths = AppPaths::resolve().unwrap();
            let registry = AccountRegistry::new(paths.registry_file());
            registry.upsert(sample_account("alice")).unwrap();

            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();

            let (tx, rx) = std::sync::mpsc::channel();
            let live_for_swap = live.clone();
            let home_for_swap = home.to_path_buf();
            std::thread::spawn(move || {
                let result = swap_with_snapshot(
                    "ticket25",
                    &home_for_swap,
                    &lock_dir(),
                    vec![SwapTarget {
                        snapshot_name: "live",
                        live_path: live_for_swap,
                        writer: write_then_ok("new"),
                    }],
                    || registry.set_active("kimi", &AccountId("alice".into())),
                );
                let _ = tx.send(result);
            });

            let result = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("swap commit → set_active 在同一把锁上重入死锁");
            result.unwrap();
            assert_eq!(fs::read_to_string(&live).unwrap(), "new");
            let alice = AccountRegistry::new(paths.registry_file())
                .find("kimi", &AccountId("alice".into()))
                .unwrap()
                .unwrap();
            assert!(alice.active);
        });
    }

    #[test]
    fn swap_set_active_and_concurrent_rename_keep_both_updates() {
        use std::sync::{Arc, Barrier};

        with_isolated_home(|home| {
            let paths = AppPaths::resolve().unwrap();
            let registry = AccountRegistry::new(paths.registry_file());
            registry.upsert(sample_account("alice")).unwrap();
            registry.upsert(sample_account("bob")).unwrap();

            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();

            let barrier = Arc::new(Barrier::new(2));
            let swap_barrier = barrier.clone();
            let swap_home = home.to_path_buf();
            let swap_live = live.clone();
            let swap_registry = AccountRegistry::new(paths.registry_file());
            let swap = std::thread::spawn(move || {
                swap_barrier.wait();
                for _ in 0..40 {
                    swap_with_snapshot(
                        "ticket25",
                        &swap_home,
                        &lock_dir(),
                        vec![SwapTarget {
                            snapshot_name: "live",
                            live_path: swap_live.clone(),
                            writer: write_then_ok("new"),
                        }],
                        || swap_registry.set_active("kimi", &AccountId("alice".into())),
                    )
                    .unwrap();
                }
            });

            let rename_registry = AccountRegistry::new(paths.registry_file());
            let rename = std::thread::spawn(move || {
                barrier.wait();
                for i in 0..40 {
                    let mut bob = sample_account("bob");
                    bob.label = format!("bob-{i}");
                    rename_registry.upsert(bob).unwrap();
                }
            });

            swap.join().expect("swap thread");
            rename.join().expect("rename thread");

            let all = AccountRegistry::new(paths.registry_file()).load().unwrap();
            let alice = all.iter().find(|a| a.id.0 == "alice").expect("alice");
            let bob = all.iter().find(|a| a.id.0 == "bob").expect("bob");
            assert!(alice.active, "切换标记被改名覆盖: {all:?}");
            assert!(
                bob.label.starts_with("bob-"),
                "改名被切换覆盖: {}",
                bob.label
            );
            assert!(!bob.active, "{all:?}");
            assert_eq!(fs::read_to_string(&live).unwrap(), "new");
        });
    }
}
