//! 通用「切换激活账号」骨架。
//!
//! Provider 不应该自己再写 flock → snapshot → 写文件 → 回滚 这套流程，
//! 用 [`swap_with_snapshot`] 声明「要写哪些目标 + 提交回调」即可。
//!
//! 设计要点：
//! - 阻塞 IO。调用方需要确保自己已在 `tokio::task::spawn_blocking` 内。
//! - 每个目标按顺序：(a) 备份到 snapshot 目录 + 内存；(b) 调用 writer 写新内容。
//! - 任一步失败：把所有 target 回滚到备份状态（原本存在的按字节原子恢复；原本不存在的删除）。
//!   回滚失败并入返回错误，以便区分「已恢复」与「live 可能处于中间态」。
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

/// 每个 provider 保留的最近切换快照份数。默认 10；不写入 settings。
const SNAPSHOT_KEEP_LAST: usize = 10;

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

/// `{provider}-{ts}` / `{provider}-{ts}-{n}`；ts 以年份数字开头，避免误伤 `foo` vs `foo-bar`。
fn is_snapshot_dir_name_for(name: &str, provider_id: &str) -> bool {
    let Some(rest) = name.strip_prefix(provider_id) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('-') else {
        return false;
    };
    rest.starts_with(|c: char| c.is_ascii_digit())
}

/// 按目录名排序后只留最近 [`SNAPSHOT_KEEP_LAST`] 份；失败只记警告，不向上传播。
fn prune_old_snapshots(root: &Path, provider_id: &str) {
    let mut dirs = match list_provider_snapshot_dirs(root, provider_id) {
        Ok(dirs) => dirs,
        Err(e) => {
            tracing::warn!(
                err = %e,
                provider = provider_id,
                keep = SNAPSHOT_KEEP_LAST,
                "failed to list swap snapshots for pruning"
            );
            return;
        }
    };
    dirs.sort();
    let excess = dirs.len().saturating_sub(SNAPSHOT_KEEP_LAST);
    for path in dirs.into_iter().take(excess) {
        match fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                err = %e,
                path = %path.display(),
                provider = provider_id,
                keep = SNAPSHOT_KEEP_LAST,
                "failed to prune old swap snapshot"
            ),
        }
    }
}

fn list_provider_snapshot_dirs(root: &Path, provider_id: &str) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_snapshot_dir_name_for(name, provider_id) {
            dirs.push(path);
        }
    }
    Ok(dirs)
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
/// 失败时已写入的文件会回滚到调用前的状态；回滚自身失败会并入返回错误。
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
    swap_with_snapshot_protected(provider_id, provider_home, lock_dir, targets, Vec::new(), commit)
}

/// 同 [`swap_with_snapshot`]，但额外把 `protected` 里的文件纳入备份/回滚集。
///
/// 这些文件不由 `targets` 的 writer 写入（典型：registry.toml 由 `commit` 里的
/// `AccountRegistry::set_active` 写），却在同一临界区内备份进切换前快照，
/// commit 失败时与 live 一起按字节原子恢复。没有这一保护，commit 阶段 registry
/// 写入的 fsync 失败（rename 已成功）会留下「registry 已换 active 标记、live 却
/// 回滚为旧账号」的发散中间态（issue #153 续跑，reviewer P1-2）；纳入后 registry
/// 的写入（含其 fsync 失败）进入既有回滚/错误路径，spec #152 决策 7 的验收语义
/// 对全部切换写入点成立。
///
/// 快照内备份文件名取原文件名，调用方保证不与 `targets` 的 `snapshot_name` 冲突。
pub fn swap_with_snapshot_protected<'a, C>(
    provider_id: &str,
    provider_home: &Path,
    lock_dir: &Path,
    targets: Vec<SwapTarget<'a>>,
    protected: Vec<PathBuf>,
    commit: C,
) -> Result<()>
where
    C: FnOnce() -> Result<()>,
{
    fs::create_dir_all(provider_home)?;
    crate::switch_lock::with_exclusive_lock(lock_dir, || {
        swap_locked(provider_id, targets, protected, commit)
    })
}

fn swap_locked<'a, C>(
    provider_id: &str,
    targets: Vec<SwapTarget<'a>>,
    protected: Vec<PathBuf>,
    commit: C,
) -> Result<()>
where
    C: FnOnce() -> Result<()>,
{
    // 1. 创建快照目录（微秒时间戳；同微秒碰撞加后缀，不复用目录）。
    let ts = snapshot_now().format("%Y%m%dT%H%M%S%.6fZ").to_string();
    let snap_dir = unique_snapshot_dir(&AppPaths::resolve()?.snapshots_dir(), provider_id, &ts)?;

    // 2. 备份现有文件（原本存在的进 snapshot 目录 + 内存）。回滚集同时覆盖
    //    targets 的 live 文件与 protected 的 commit 阶段文件（registry 等）。
    let mut backups: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    let mut plans: Vec<TargetPlan> = Vec::with_capacity(targets.len());
    for target in targets {
        let backup = backup_into_snapshot(&snap_dir, &target.live_path, target.snapshot_name)?;
        backups.push((target.live_path.clone(), backup));
        plans.push(TargetPlan {
            live_path: target.live_path,
            writer: target.writer,
        });
    }
    for path in protected {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("protected");
        let backup = backup_into_snapshot(&snap_dir, &path, name)?;
        backups.push((path, backup));
    }
    tracing::info!(snapshot = %snap_dir.display(), "pre-swap snapshot saved");

    // 3. 顺序写入；任一失败回滚所有 plan（即使 writer 没跑到也尝试 restore，让状态一致）。
    for plan in plans {
        let TargetPlan { live_path, writer } = plan;
        if let Err(e) = writer(&live_path) {
            return Err(with_rollback(e, rollback(&backups)));
        }
    }

    // 4. 收尾 commit（典型：registry.set_active）。失败同样回滚（含 protected 文件）。
    if let Err(e) = commit() {
        return Err(with_rollback(e, rollback(&backups)));
    }

    // 5. 提交成功后再按上限清理本 provider 历史快照；失败只记警告，不阻断切换。
    if let Some(root) = snap_dir.parent() {
        prune_old_snapshots(root, provider_id);
    }

    Ok(())
}

/// 备份单个文件进切换前快照：存在的写进快照目录（含文件级 fsync，issue #153）
/// 并返回字节，不存在的返回 None（回滚语义 = 删除）。
fn backup_into_snapshot(
    snap_dir: &Path,
    live_path: &Path,
    snapshot_name: &str,
) -> Result<Option<Vec<u8>>> {
    match fs::read(live_path) {
        Ok(raw) => {
            let snapshot_path = snap_dir.join(snapshot_name);
            fs::write(&snapshot_path, &raw)?;
            restrict_permissions(&snapshot_path)?;
            sync_file(&snapshot_path)?;
            Ok(Some(raw))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
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
}

fn with_rollback(origin: Error, rolled: std::result::Result<(), String>) -> Error {
    match rolled {
        Ok(()) => origin,
        Err(details) => Error::provider(format!("{origin}; {details}")),
    }
}

/// 切换路径写入的文件级 fsync（issue #153）：在「临时文件 + rename」之后把文件数据
/// 落盘，掉电不再丢失最后一次切换。只做文件级，不做目录 fsync（目录项持久性依赖
/// NTFS 元数据日志）。Windows 的 `FlushFileBuffers` 需要写权限句柄，故以写权限
/// 重新打开目标；失败如实上抛，由既有回滚/错误路径兜底。
pub fn sync_file(path: &Path) -> Result<()> {
    #[cfg(test)]
    if take_fail_next_sync() {
        return Err(Error::provider(format!(
            "injected fsync failure: {}",
            path.display()
        )));
    }
    #[cfg(test)]
    FSYNC_CALLS.with(|cell| cell.set(cell.get() + 1));
    let file = fs::OpenOptions::new().write(true).open(path)?;
    file.sync_all()
        .map_err(|e| Error::provider(format!("fsync {} failed: {e}", path.display())))
}

// 测试 seam（issue #153）：让下一次 [`sync_file`] 失败，覆盖错误注入路径。
#[cfg(test)]
thread_local! {
    static FAIL_NEXT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FSYNC_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn take_fail_next_sync() -> bool {
    FAIL_NEXT_SYNC.with(|cell| cell.replace(false))
}

/// 测试专用：注入下一次 `sync_file` 失败（一次性，触发后自动复位）。
#[cfg(test)]
pub(crate) fn fail_next_sync() {
    FAIL_NEXT_SYNC.with(|cell| cell.set(true));
}

/// 测试专用：读取本线程 `sync_file` 的累计调用次数，用于断言调用可达性。
#[cfg(test)]
pub(crate) fn sync_call_count() -> u32 {
    FSYNC_CALLS.with(|cell| cell.get())
}

/// 同目录临时文件 + rename；Unix 下临时文件经 [`restrict_permissions`] 收为 0600。
/// rename 之后做文件级 fsync（issue #153），失败进入既有回滚/错误路径。
fn atomic_write_file(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".tmp.{}", std::process::id()));
        PathBuf::from(name)
    };
    let written: Result<()> = (|| {
        fs::write(&tmp, contents)?;
        restrict_permissions(&tmp)?;
        fs::rename(&tmp, path)?;
        sync_file(path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn restore_one(path: &Path, backup: Option<&[u8]>) -> Result<()> {
    match backup {
        Some(raw) => atomic_write_file(path, raw),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            // 原本就不存在：视为已恢复到「未登录」状态。
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        },
    }
}

fn rollback(backups: &[(PathBuf, Option<Vec<u8>>)]) -> std::result::Result<(), String> {
    let mut errors = Vec::new();
    for (path, backup) in backups {
        if let Err(e) = restore_one(path, backup.as_deref()) {
            tracing::error!(err=%e, path=%path.display(), "rollback restore failed");
            errors.push(format!("{}: {e}", path.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "rollback failed; live files may be in an intermediate state ({})",
            errors.join("; ")
        ))
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
            assert!(
                !err.to_string().contains("rollback failed"),
                "回滚成功时错误不应声称回滚失败: {err}"
            );
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
            assert!(
                !err.to_string().contains("rollback failed"),
                "回滚成功时错误不应声称回滚失败: {err}"
            );
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
                !err.to_string().contains("rollback failed"),
                "回滚成功时错误不应声称回滚失败: {err}"
            );
            assert!(
                !live.exists(),
                "原本不存在的 live 文件在回滚后应被删除，仍在: {}",
                live.display()
            );
        });
    }

    #[test]
    fn non_utf8_live_file_can_still_complete_swap() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, [0xff, 0xfe, 0x00, b'!']).unwrap();

            swap_with_snapshot(
                "ticket75",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_ok("new"),
                }],
                || Ok(()),
            )
            .unwrap();
            assert_eq!(fs::read_to_string(&live).unwrap(), "new");
        });
    }

    #[test]
    fn partial_write_restores_first_already_written_target() {
        with_isolated_home(|home| {
            let first = home.join("first.txt");
            let second = home.join("second.txt");
            fs::write(&first, "old-first").unwrap();
            fs::write(&second, "old-second").unwrap();

            let err = swap_with_snapshot(
                "ticket75",
                home,
                &lock_dir(),
                vec![
                    SwapTarget {
                        snapshot_name: "first",
                        live_path: first.clone(),
                        writer: write_then_ok("new-first"),
                    },
                    SwapTarget {
                        snapshot_name: "second",
                        live_path: second.clone(),
                        writer: Box::new(|_| Err(Error::provider("second target failed"))),
                    },
                ],
                || Ok(()),
            )
            .unwrap_err();
            assert!(err.to_string().contains("second target failed"), "{err}");
            assert!(
                !err.to_string().contains("rollback failed"),
                "回滚成功时错误不应声称回滚失败: {err}"
            );
            assert_eq!(fs::read_to_string(&first).unwrap(), "old-first");
            assert_eq!(fs::read_to_string(&second).unwrap(), "old-second");
        });
    }

    #[test]
    fn rollback_failure_is_returned_and_live_left_in_intermediate_state() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();

            let err = swap_with_snapshot(
                "ticket75",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: Box::new(|path| {
                        fs::write(path, "new")?;
                        fs::remove_file(path)?;
                        fs::create_dir(path)?;
                        Err(Error::provider("injected writer failure"))
                    }),
                }],
                || Ok(()),
            )
            .unwrap_err();
            let text = err.to_string();
            assert!(text.contains("injected writer failure"), "{text}");
            assert!(
                text.contains("rollback failed"),
                "回滚失败必须并入返回错误: {text}"
            );
            assert!(
                text.contains("intermediate state"),
                "应能区分 live 中间态: {text}"
            );
            assert!(
                live.is_dir(),
                "回滚失败后 live 应仍是写入制造的目录中间态: {}",
                live.display()
            );
        });
    }

    #[test]
    fn missing_live_delete_failure_is_returned() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            assert!(!live.exists());

            let err = swap_with_snapshot(
                "ticket75",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: Box::new(|path| {
                        fs::create_dir(path)?;
                        fs::write(path.join("keep"), b"x")?;
                        Err(Error::provider("injected writer failure"))
                    }),
                }],
                || Ok(()),
            )
            .unwrap_err();
            let text = err.to_string();
            assert!(text.contains("injected writer failure"), "{text}");
            assert!(
                text.contains("rollback failed"),
                "原本不存在的 live 删除失败不得静默: {text}"
            );
            assert!(
                live.is_dir(),
                "删除失败后目录中间态应仍在: {}",
                live.display()
            );
            assert_eq!(fs::read(live.join("keep")).unwrap(), b"x");
        });
    }

    #[test]
    fn damaged_registry_blocks_swap_but_keeps_live_file_intact() {
        with_isolated_home(|home| {
            let paths = AppPaths::resolve().unwrap();
            let registry = AccountRegistry::new(paths.registry_file());
            registry.upsert(sample_account("alice")).unwrap();

            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();
            fs::write(paths.registry_file(), "this is not valid toml {{{").unwrap();

            let err = swap_with_snapshot(
                "ticket75",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_ok("new"),
                }],
                || registry.set_active("kimi", &AccountId("alice".into())),
            )
            .unwrap_err();
            let text = err.to_string();
            assert!(text.contains("toml"), "损坏的 registry 应阻断切换: {text}");
            assert!(
                !text.contains("rollback failed"),
                "live 已恢复时不应报告回滚失败: {text}"
            );
            assert_eq!(fs::read_to_string(&live).unwrap(), "old");
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

    fn plant_snapshot_dir(root: &Path, name: &str, contents: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("live"), contents).unwrap();
        dir
    }

    #[test]
    fn successful_swap_drops_oldest_snapshot_when_over_keep_last() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();
            let root = AppPaths::resolve().unwrap().snapshots_dir();
            for day in 1..=10 {
                plant_snapshot_dir(
                    &root,
                    &format!("ticket77-202001{day:02}T000000.000000Z"),
                    &format!("old-{day}"),
                );
            }
            let other = plant_snapshot_dir(&root, "other-20200101T000000.000000Z", "other-keep");

            with_frozen_now(parse_utc("2026-06-07T08:09:10.000001Z"), || {
                swap_with_snapshot(
                    "ticket77",
                    home,
                    &lock_dir(),
                    vec![SwapTarget {
                        snapshot_name: "live",
                        live_path: live.clone(),
                        writer: write_then_ok("new"),
                    }],
                    || Ok(()),
                )
                .unwrap();
            });

            let kept = snapshot_dirs("ticket77");
            assert_eq!(kept.len(), 10, "超过保留上限时应只留最近 10 份: {kept:?}");
            let names: Vec<String> = kept
                .iter()
                .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            assert!(
                !names
                    .iter()
                    .any(|n| n == "ticket77-20200101T000000.000000Z"),
                "最旧快照应被清理: {names:?}"
            );
            assert!(
                names.iter().any(|n| n.contains("20200102T000000")),
                "次旧快照应仍保留: {names:?}"
            );
            assert!(
                names
                    .iter()
                    .any(|n| n == "ticket77-20260607T080910.000001Z"),
                "本次切换快照应保留: {names:?}"
            );
            assert!(
                other.is_dir(),
                "其他 provider 的快照不得被清理: {}",
                other.display()
            );
            assert_eq!(
                fs::read_to_string(other.join("live")).unwrap(),
                "other-keep"
            );
            assert_eq!(fs::read_to_string(&live).unwrap(), "new");
        });
    }

    #[test]
    fn same_microsecond_suffix_is_newer_than_unsuffixed_dir() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();
            let root = AppPaths::resolve().unwrap().snapshots_dir();
            plant_snapshot_dir(&root, "ticket77-20200101T000000.000000Z", "base");
            for n in 1..=9 {
                plant_snapshot_dir(
                    &root,
                    &format!("ticket77-20200101T000000.000000Z-{n}"),
                    &format!("sfx-{n}"),
                );
            }

            with_frozen_now(parse_utc("2026-06-07T08:09:10.000001Z"), || {
                swap_with_snapshot(
                    "ticket77",
                    home,
                    &lock_dir(),
                    vec![SwapTarget {
                        snapshot_name: "live",
                        live_path: live.clone(),
                        writer: write_then_ok("new"),
                    }],
                    || Ok(()),
                )
                .unwrap();
            });

            let kept = snapshot_dirs("ticket77");
            let names: Vec<String> = kept
                .iter()
                .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            assert_eq!(kept.len(), 10, "应只留最近 10 份: {names:?}");
            assert!(
                !names
                    .iter()
                    .any(|n| n == "ticket77-20200101T000000.000000Z"),
                "无后缀同微秒目录更旧，应被清理: {names:?}"
            );
            assert!(
                names
                    .iter()
                    .any(|n| n == "ticket77-20200101T000000.000000Z-1"),
                "带 -n 后缀的同微秒目录应视为更新而保留: {names:?}"
            );
            assert!(
                names
                    .iter()
                    .any(|n| n == "ticket77-20260607T080910.000001Z"),
                "本次切换快照应保留: {names:?}"
            );
        });
    }

    #[test]
    fn snapshot_prune_failure_does_not_fail_swap() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();
            let root = AppPaths::resolve().unwrap().snapshots_dir();
            let oldest = plant_snapshot_dir(&root, "ticket77-20200101T000000.000000Z", "oldest");
            for day in 2..=10 {
                plant_snapshot_dir(
                    &root,
                    &format!("ticket77-202001{day:02}T000000.000000Z"),
                    "kept",
                );
            }

            #[cfg(windows)]
            let _held = {
                use std::os::windows::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(oldest.join("live"))
                    .unwrap()
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&oldest, fs::Permissions::from_mode(0o555)).unwrap();
            }

            let result = swap_with_snapshot(
                "ticket77",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_ok("new"),
                }],
                || Ok(()),
            );

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&oldest, fs::Permissions::from_mode(0o755));
            }

            result.expect("清理失败不得阻断切换");
            assert_eq!(fs::read_to_string(&live).unwrap(), "new");
            assert!(
                oldest.is_dir(),
                "删不掉的最旧快照应仍在: {}",
                oldest.display()
            );
        });
    }

    // --- issue #153：切换路径写入 fsync ---

    /// 快照备份的 fsync 失败必须让切换如实报错（进既有错误路径），且此时任何
    /// live 文件都还没被写——不得静默成功。
    #[test]
    fn snapshot_fsync_failure_fails_swap_before_any_live_write() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();

            fail_next_sync();
            let err = swap_with_snapshot(
                "ticket153",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_ok("new"),
                }],
                || Ok(()),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("injected fsync failure"),
                "fsync 失败必须上抛: {err}"
            );
            assert_eq!(
                fs::read_to_string(&live).unwrap(),
                "old",
                "快照阶段失败时 live 尚未被写，应保持原内容"
            );
        });
    }

    /// 调用可达性：成功切换必须 fsync 切换前快照；写入失败回滚时恢复写也要 fsync。
    #[test]
    fn fsync_is_reached_for_snapshot_backup_and_rollback_restore() {
        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();

            let before = sync_call_count();
            swap_with_snapshot(
                "ticket153",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_ok("new"),
                }],
                || Ok(()),
            )
            .unwrap();
            let after_success = sync_call_count();
            assert!(
                after_success > before,
                "成功切换至少应对切换前快照做一次 fsync"
            );

            // 写入失败 → 回滚：restore_one 走 atomic_write_file，rename 后也要 fsync。
            let err = swap_with_snapshot(
                "ticket153",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_err("newer", "injected writer failure"),
                }],
                || Ok(()),
            )
            .unwrap_err();
            assert!(err.to_string().contains("injected writer failure"), "{err}");
            assert!(
                sync_call_count() > after_success,
                "回滚恢复写之后应再次 fsync"
            );
            assert_eq!(fs::read_to_string(&live).unwrap(), "new");
        });
    }

    /// issue #153：锁被他人持有时，第二次切换最多等约 3 秒即返回明确错误，
    /// 不再无限阻塞；错误文案语义「另一切换操作未完成，请重试」。
    #[test]
    fn swap_returns_clear_error_after_about_three_seconds_when_lock_held_elsewhere() {
        use std::sync::mpsc;

        with_isolated_home(|home| {
            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();

            let lock = lock_dir();
            let (held_tx, held_rx) = mpsc::channel::<()>();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let holder_lock = lock.clone();
            let holder = std::thread::spawn(move || {
                crate::switch_lock::with_exclusive_lock(&holder_lock, || {
                    held_tx.send(()).ok();
                    // 持锁直到测试释放信号（上限 10s 防悬挂）。
                    release_rx.recv_timeout(Duration::from_secs(10)).ok();
                    Ok(())
                })
            });
            held_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("持锁线程应先进入临界区");

            let started = std::time::Instant::now();
            let err = swap_with_snapshot(
                "ticket153",
                home,
                &lock,
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_ok("new"),
                }],
                || Ok(()),
            )
            .unwrap_err();
            let elapsed = started.elapsed();

            release_tx.send(()).ok();
            holder.join().unwrap().unwrap();

            assert!(
                matches!(err, crate::error::Error::SwitchLockTimeout),
                "持锁超时应返回 SwitchLockTimeout: {err}"
            );
            assert_eq!(err.to_string(), "另一切换操作未完成，请重试");
            assert!(
                elapsed >= Duration::from_millis(2500) && elapsed <= Duration::from_millis(4500),
                "有界等待应约 3 秒返回，实际 {elapsed:?}"
            );
            assert_eq!(
                fs::read_to_string(&live).unwrap(),
                "old",
                "超时失败时 live 不得被改动"
            );
        });
    }

    /// issue #153 续跑（reviewer P1-2）：commit 阶段 registry 写入（set_active 的
    /// tmp+rename+fsync）失败时，registry 必须与 live 一起回滚——盘上 registry.toml
    /// 不得残留新 active 标记而 live 已恢复旧账号（live/registry 发散中间态）。
    #[test]
    fn registry_fsync_failure_rolls_back_registry_and_live_consistently() {
        with_isolated_home(|home| {
            let paths = AppPaths::resolve().unwrap();
            let registry = AccountRegistry::new(paths.registry_file());
            registry.upsert(sample_account("alice")).unwrap();
            registry.upsert(sample_account("bob")).unwrap();
            // 切换前状态：alice 为 active，live 为 alice 内容。
            registry
                .set_active("kimi", &AccountId("alice".into()))
                .unwrap();

            let live = home.join("live.txt");
            fs::write(&live, "alice-live").unwrap();

            let err = swap_with_snapshot_protected(
                "ticket153",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_ok("bob-live"),
                }],
                vec![paths.registry_file()],
                || {
                    // 精确注入：此刻快照备份的 fsync 已全部完成，writer 只 fs::write
                    // 不 sync，因此下一个 sync_file 恰是 set_active → save_unlocked
                    // 里 registry 写入的 fsync（rename 已成功）。
                    fail_next_sync();
                    registry.set_active("kimi", &AccountId("bob".into()))
                },
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("injected fsync failure"),
                "registry fsync 失败必须上抛: {err}"
            );
            assert!(
                !err.to_string().contains("rollback failed"),
                "回滚成功时错误不应声称回滚失败: {err}"
            );

            // 不变式钉住：live 与 registry 都回到切换前状态，无发散中间态。
            assert_eq!(
                fs::read_to_string(&live).unwrap(),
                "alice-live",
                "live 应回滚为切换前内容"
            );
            let accounts = AccountRegistry::new(paths.registry_file())
                .load()
                .unwrap();
            let active = accounts
                .iter()
                .find(|a| a.active)
                .expect("registry 应回滚到仍有 active 标记的状态");
            assert_eq!(
                active.id.0, "alice",
                "registry 不得残留切换未完成的新 active 标记: {accounts:?}"
            );
        });
    }

    /// issue #153 续跑（reviewer P1-2）边界：protected 文件原本不存在（新装环境
    /// 首次切换、registry 尚未创建）时，commit 失败后回滚语义 = 删除，不留半截文件。
    #[test]
    fn protected_file_that_did_not_exist_is_deleted_on_rollback() {
        with_isolated_home(|home| {
            let paths = AppPaths::resolve().unwrap();
            assert!(!paths.registry_file().exists());

            let live = home.join("live.txt");
            fs::write(&live, "old").unwrap();

            let err = swap_with_snapshot_protected(
                "ticket153",
                home,
                &lock_dir(),
                vec![SwapTarget {
                    snapshot_name: "live",
                    live_path: live.clone(),
                    writer: write_then_ok("new"),
                }],
                vec![paths.registry_file()],
                || {
                    // commit 里「写出」registry 后再失败：模拟 set_active 写盘成功
                    // 但后续步骤失败，回滚必须把新建的 registry 删掉。
                    std::fs::write(paths.registry_file(), "half-written").unwrap();
                    Err(Error::provider("injected commit failure"))
                },
            )
            .unwrap_err();
            assert!(err.to_string().contains("injected commit failure"), "{err}");
            assert!(
                !err.to_string().contains("rollback failed"),
                "回滚成功时错误不应声称回滚失败: {err}"
            );
            assert!(
                !paths.registry_file().exists(),
                "原本不存在的 protected 文件在回滚后应被删除"
            );
            assert_eq!(fs::read_to_string(&live).unwrap(), "old");
        });
    }
}
