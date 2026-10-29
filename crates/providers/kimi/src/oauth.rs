//! Kimi OAuth 刷新：POST {oauth_host}/api/oauth/token（form-urlencoded, grant_type=refresh_token）。

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fs2::FileExt;
use kimi_switch_core::error::{Error, Result};
use kimi_switch_core::Account;
use kimi_switch_engine::{
    extract_access_token, extract_refresh_token, forced_rotation_fits, FileBlobRuntime,
    RefreshOutcome,
};
use sha2::{Digest, Sha256};

use crate::kimi_files::decode_jwt_payload;
use crate::{kimi_files, paths, KimiRuntime};

const MIN_COORDINATED_REFRESH_VERSION: (u64, u64, u64) = (1, 31, 0);
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
/// 轮换请求的总超时：连接到响应体读完，代理回退直连也只用剩余时间（net.rs）。停泊账号的
/// 强制轮换与当前账号的 401 协调恢复都据此核对剩余预算（ADR-0019）。
pub(crate) const REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const TYPESCRIPT_STALE_AFTER: Duration = Duration::from_secs(10);
const STALE_RECHECK_INTERVAL: Duration = Duration::from_millis(100);
const OFFICIAL_OAUTH_HOST: &str = "https://auth.kimi.com";
/// 同进程内私有 tmp 名序号；与 pid 一起保证并发不撞名。
static PRIVATE_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RefreshLockProtocol {
    PythonFile,
    TypeScriptDirectory,
}

/// 官方刷新锁探测的三态结论（F1）。旧缓存值 `Option<RefreshLockProtocol>` 的 None
/// 同时混着「确定无锁」与「探测失败/不可解析/超时」，主动续期门必须区分两者：
/// 只有**确定无锁**才允许不经官方锁自创轮换；探测不确定一律 fail-closed。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockProbeOutcome {
    /// 确定存在可用的官方刷新锁协议（协调恢复走它）。
    Locked(RefreshLockProtocol),
    /// 确定无官方刷新锁：Windows + TS 0.x（官方 win32 直接放弃锁，见 ADR-0003）、
    /// Python 1.x 早于 1.31（协调锁尚未引入），或用户用 KIMI_DISABLE_OAUTH_LOCK
    /// 显式关闭了目录锁。
    NoLock,
    /// 探测失败、输出不可解析或主版本太新（2.x+）不识：不能确证无锁。
    Unknown,
}

/// 解析 OAuth host。release 固定官方常量；仅 debug 构建允许 `KIMI_CODE_OAUTH_HOST` 覆写。
pub(crate) fn oauth_host() -> String {
    oauth_host_from(std::env::var("KIMI_CODE_OAUTH_HOST").ok().as_deref())
}

/// 注入式 host 解析（测试标杆）。release 忽略覆写，避免环境变量把流量打到非官方域名。
fn oauth_host_from(override_host: Option<&str>) -> String {
    #[cfg(debug_assertions)]
    {
        if let Some(host) = override_host.map(str::trim).filter(|s| !s.is_empty()) {
            return host.trim_end_matches('/').to_string();
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = override_host;
    }
    OFFICIAL_OAUTH_HOST.to_string()
}

/// 从 blob 的 access_token JWT 里取 client_id（刷新请求需要）。
fn client_id_from_blob(blob: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(blob).ok()?;
    let token = v.get("access_token")?.as_str()?;
    decode_jwt_payload(token)?
        .get("client_id")?
        .as_str()
        .map(String::from)
}

/// 用 blob 里的 refresh_token 换新令牌，返回轮换后的完整 blob（合并回原 JSON 结构）。
pub async fn refresh_blob(blob: &str) -> Result<RefreshOutcome> {
    refresh_blob_at(blob, &oauth_host()).await
}

async fn refresh_blob_at(blob: &str, oauth_base: &str) -> Result<RefreshOutcome> {
    let Some(refresh) = extract_refresh_token(blob) else {
        return Ok(RefreshOutcome::Unsupported);
    };
    let Some(client_id) = client_id_from_blob(blob) else {
        return Ok(RefreshOutcome::Unsupported);
    };

    let url = format!("{}/api/oauth/token", oauth_base.trim_end_matches('/'));
    let form = [
        ("client_id", client_id.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
    ];

    let resp = crate::net::send(
        Some(REFRESH_HTTP_TIMEOUT),
        crate::net::ProxyFallback::ConnectOnly,
        |client| {
            client
                .post(&url)
                .header("User-Agent", "kimi-switch")
                .header("Accept", "application/json")
                .form(&form)
        },
    )
    .await
    .map_err(|e| Error::provider(format!("kimi refresh request failed: {e}")))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Ok(RefreshOutcome::DeadToken);
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    if parsed.get("error").and_then(|v| v.as_str()) == Some("invalid_grant") {
        return Ok(RefreshOutcome::DeadToken);
    }
    if !status.is_success() {
        return Err(Error::provider_http(
            status.as_u16(),
            format!("kimi refresh HTTP {status}"),
        ));
    }
    let access = parsed.get("access_token").and_then(|v| v.as_str());
    let Some(access) = access else {
        return Err(Error::provider(
            "kimi refresh response missing access_token",
        ));
    };

    // 合并回原 blob 结构，保留未知字段。
    let mut merged: serde_json::Value = serde_json::from_str(blob).unwrap_or(serde_json::json!({}));
    let obj = merged.as_object_mut().unwrap();
    obj.insert(
        "access_token".into(),
        serde_json::Value::String(access.into()),
    );
    for key in ["refresh_token", "scope", "token_type", "expires_in"] {
        if let Some(v) = parsed.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    if let Some(exp) = parsed.get("expires_in").and_then(|v| v.as_i64()) {
        let now = chrono::Utc::now().timestamp();
        obj.insert("expires_at".into(), serde_json::Value::from(now + exp));
    }
    Ok(RefreshOutcome::Rotated(merged.to_string()))
}

/// active 账号用量查询 401 后，与官方 Kimi 协调一次令牌恢复。
///
/// Python 版 1.31+ 使用 flock 锁；TypeScript 版 0.x 在 Unix 使用 proper-lockfile 目录锁。
/// 不支持协调锁的平台、旧版或版本未知时宁可保留 401，也不争抢一次性 refresh token。
/// 轮换结果持官方锁经轮换写回比对提交（`commit_rotation_and_release`，ADR-0019）。
pub async fn recover_active_401(
    attempted_access_token: &str,
    account: &Account,
    runtime: &KimiRuntime,
) -> Result<Option<String>> {
    let Some(protocol) = installed_kimi_refresh_lock_protocol().await else {
        tracing::warn!(
            "Kimi CLI refresh-lock protocol is unavailable; skip active token refresh to avoid refresh-token races"
        );
        return Ok(None);
    };
    recover_active_401_at(
        attempted_access_token,
        account,
        &paths::kimi_home(),
        &oauth_host(),
        protocol,
        runtime,
    )
    .await
}

async fn installed_kimi_refresh_lock_protocol() -> Option<RefreshLockProtocol> {
    match installed_kimi_lock_probe().await {
        LockProbeOutcome::Locked(protocol) => Some(protocol),
        // 协调恢复的 fail-safe 语义逐点不变：确定无锁与探测不确定都不自愈。
        LockProbeOutcome::NoLock | LockProbeOutcome::Unknown => None,
    }
}

/// 探测本机 Kimi CLI 的刷新锁协议（三态，进程内缓存一次）。
/// `kimi --version` 实测可到数秒；同一进程内协议不会变，缓存避免每次 401 自愈都再付一次代价，
/// 也降低被外层 quota fetch timeout 中途取消的概率。
async fn installed_kimi_lock_probe() -> LockProbeOutcome {
    static CACHED: OnceLock<LockProbeOutcome> = OnceLock::new();
    if let Some(cached) = CACHED.get() {
        return *cached;
    }
    let probed = tokio::task::spawn_blocking(|| {
        let Some(output) = Command::new("kimi")
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
        else {
            // 进程拉不起 / 非零退出：探测失败，不能确证无锁（fail-closed）。
            return LockProbeOutcome::Unknown;
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 找锁这一段与原实现逐点一致：stdout → stderr，找到后再过禁用开关。
        let found = lock_protocol_from_version_output(&stdout)
            .or_else(|| lock_protocol_from_version_output(&stderr));
        let lock_disabled = std::env::var("KIMI_DISABLE_OAUTH_LOCK").as_deref() == Ok("1");
        match found {
            Some(protocol) => match protocol_with_lock_setting(protocol, lock_disabled) {
                Some(protocol) => LockProbeOutcome::Locked(protocol),
                // 用户用 KIMI_DISABLE_OAUTH_LOCK 显式关闭目录锁：确定无锁可用。
                None => LockProbeOutcome::NoLock,
            },
            // 没有任何一路给出锁协议：区分「读到确定无锁的版本组合」与
            // 「读不出 / 版本不识」（后者 fail-closed，不臆断无锁）。
            None if provably_lock_free(&stdout) || provably_lock_free(&stderr) => {
                LockProbeOutcome::NoLock
            }
            None => LockProbeOutcome::Unknown,
        }
    })
    .await
    .unwrap_or(LockProbeOutcome::Unknown);
    *CACHED.get_or_init(|| probed)
}

/// 这路版本输出是否**确证**无官方刷新锁：能解析出版本，且该版本/平台组合确定无锁——
/// Windows 上 TS 0.x（官方 `process.platform === "win32"` 直接放弃锁，见 ADR-0003）、
/// Python 1.x 早于 1.31（协调锁尚未引入）。输出不可解析、旧 Python 0.x 前缀噪音、
/// 2.x+ 主版本不识 → false（不确定；主动续期门据此 fail-closed，不放行）。
fn provably_lock_free(output: &str) -> bool {
    let trimmed = output.trim();
    let Some(version) = parse_version(trimmed) else {
        return false;
    };
    if version.0 == 0
        && (trimmed.split_whitespace().count() != 1 || trimmed != format_version(version))
    {
        return false;
    }
    // `lock_protocol_for_version` 为 None 的版本里，只有 0.x（仅 Windows）与
    // 1.x < 1.31 是确定无锁；2.x+ 主版本不识 → 不确定。
    version.0 <= 1 && lock_protocol_for_version(version).is_none()
}

fn protocol_with_lock_setting(
    protocol: RefreshLockProtocol,
    lock_disabled: bool,
) -> Option<RefreshLockProtocol> {
    if lock_disabled && protocol == RefreshLockProtocol::TypeScriptDirectory {
        None
    } else {
        Some(protocol)
    }
}

fn lock_protocol_from_version_output(output: &str) -> Option<RefreshLockProtocol> {
    let trimmed = output.trim();
    let version = parse_version(trimmed)?;
    if version.0 == 0 {
        // 新 TypeScript 客户端输出裸 semver；旧 Python 0.x 带 kimi/kimi-cli/version 前缀。
        if trimmed.split_whitespace().count() != 1 || trimmed != format_version(version) {
            return None;
        }
    }
    lock_protocol_for_version(version)
}

fn format_version((major, minor, patch): (u64, u64, u64)) -> String {
    format!("{major}.{minor}.{patch}")
}

fn lock_protocol_for_version(version: (u64, u64, u64)) -> Option<RefreshLockProtocol> {
    if version.0 == 0 {
        #[cfg(not(windows))]
        return Some(RefreshLockProtocol::TypeScriptDirectory);
        #[cfg(windows)]
        return None;
    }
    (version.0 == 1 && version >= MIN_COORDINATED_REFRESH_VERSION)
        .then_some(RefreshLockProtocol::PythonFile)
}

fn parse_version(output: &str) -> Option<(u64, u64, u64)> {
    output.split_whitespace().find_map(|part| {
        let candidate = part.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        let mut numbers = candidate.split('.');
        let major = numbers.next()?.parse().ok()?;
        let minor = numbers.next()?.parse().ok()?;
        let patch = numbers
            .next()
            .unwrap_or("0")
            .trim_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .ok()?;
        Some((major, minor, patch))
    })
}

enum RefreshLockKind {
    File(File),
    Directory {
        path: PathBuf,
        stop_heartbeat: Sender<()>,
        heartbeat: JoinHandle<()>,
    },
}

struct RefreshLockGuard {
    kind: Option<RefreshLockKind>,
}

impl RefreshLockGuard {
    fn new(kind: RefreshLockKind) -> Self {
        Self { kind: Some(kind) }
    }
}

impl Drop for RefreshLockGuard {
    fn drop(&mut self) {
        let Some(kind) = self.kind.take() else {
            return;
        };
        match kind {
            RefreshLockKind::File(lock) => {
                let _ = FileExt::unlock(&lock);
            }
            RefreshLockKind::Directory {
                path,
                stop_heartbeat,
                heartbeat,
            } => {
                let _ = stop_heartbeat.send(());
                let _ = heartbeat.join();
                let _ = std::fs::remove_dir(path);
            }
        }
    }
}

struct LockedCredentials {
    guard: RefreshLockGuard,
    raw: String,
    dead_refresh_fingerprints: HashSet<String>,
}

pub(crate) async fn recover_active_401_at(
    attempted_access_token: &str,
    account: &Account,
    home: &Path,
    oauth_base: &str,
    protocol: RefreshLockProtocol,
    runtime: &KimiRuntime,
) -> Result<Option<String>> {
    let credential_path = paths::active_cred_path(home);
    let Some(locked) =
        acquire_and_read(home.to_path_buf(), credential_path.clone(), protocol).await?
    else {
        tracing::warn!("Timed out waiting for Kimi credential refresh lock");
        return Ok(None);
    };

    let metadata = kimi_files::parse_metadata(&locked.raw);
    if metadata.primary_id.as_deref() != Some(account.id.0.as_str()) {
        release_lock(locked.guard).await;
        return Ok(None);
    }

    let latest_access = extract_access_token(&locked.raw);
    if latest_access
        .as_deref()
        .is_some_and(|token| token != attempted_access_token)
    {
        release_lock(locked.guard).await;
        return Ok(latest_access);
    }

    let current_refresh = extract_refresh_token(&locked.raw);
    let current_refresh_fingerprint = current_refresh.as_deref().map(refresh_fingerprint);
    if current_refresh_fingerprint
        .as_ref()
        .is_some_and(|fp| locked.dead_refresh_fingerprints.contains(fp))
    {
        release_lock(locked.guard).await;
        return Ok(None);
    }

    // 预算门（ADR-0019 决策 7），与停泊账号的强制轮换同一条规则：等官方刷新锁可能已用掉本次
    // attempt 的大半预算，剩余时间盖不住轮换请求的超时上限加余量就不发，放锁按未恢复返回、留给
    // 下次查询；照发的话外层超时可能在响应返回前取消查询，丢掉服务端已消耗的 refresh token。
    let time_left = kimi_switch_core::quota_query::attempt_time_left();
    if !forced_rotation_fits(time_left, runtime.refresh_timeout()) {
        tracing::warn!(
            account = %account.id,
            time_left_ms = time_left.map(|left| left.as_millis()),
            "active 401 recovery got the refresh lock too late in its attempt; leaving rotation to the next query"
        );
        release_lock(locked.guard).await;
        return Ok(None);
    }

    let outcome = refresh_blob_at(&locked.raw, oauth_base).await;
    match outcome {
        Ok(RefreshOutcome::Rotated(rotated)) => {
            // 本次查询照用新 access token 重试，不论轮换结果最终落在当前登录文件还是库存。
            let fresh_access = extract_access_token(&rotated);
            commit_rotation_and_release(
                locked.guard,
                runtime.clone(),
                home.to_path_buf(),
                account.clone(),
                locked.raw,
                rotated,
                current_refresh_fingerprint,
            )
            .await?;
            Ok(fresh_access)
        }
        Ok(RefreshOutcome::DeadToken) => {
            if let Some(refresh) = current_refresh {
                write_dead_fingerprint_and_release(
                    locked.guard,
                    paths::dead_refresh_fingerprint_path(home),
                    refresh_fingerprint(&refresh),
                )
                .await?;
            } else {
                release_lock(locked.guard).await;
            }
            Ok(None)
        }
        Ok(RefreshOutcome::Unsupported) => {
            release_lock(locked.guard).await;
            Ok(None)
        }
        Err(error) => {
            release_lock(locked.guard).await;
            Err(error)
        }
    }
}

async fn acquire_and_read(
    home: PathBuf,
    credential_path: PathBuf,
    protocol: RefreshLockProtocol,
) -> Result<Option<LockedCredentials>> {
    acquire_and_read_with_timeout(home, credential_path, protocol, LOCK_WAIT_TIMEOUT).await
}

async fn acquire_and_read_with_timeout(
    home: PathBuf,
    credential_path: PathBuf,
    protocol: RefreshLockProtocol,
    wait_timeout: Duration,
) -> Result<Option<LockedCredentials>> {
    tokio::task::spawn_blocking(move || {
        let guard = RefreshLockGuard::new(match protocol {
            RefreshLockProtocol::PythonFile => {
                let lock_path = paths::credentials_lock_path(&home);
                if let Some(parent) = lock_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut lock = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(lock_path)?;
                if lock.metadata()?.len() == 0 {
                    lock.write_all(&[0])?;
                    lock.flush()?;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
                let deadline = Instant::now() + wait_timeout;
                loop {
                    match lock.try_lock_exclusive() {
                        Ok(()) => break,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return Ok(None);
                            }
                            std::thread::sleep(LOCK_RETRY_INTERVAL);
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                RefreshLockKind::File(lock)
            }
            RefreshLockProtocol::TypeScriptDirectory => {
                let sentinel = paths::oauth_lock_sentinel_path(&home);
                let lock_dir = paths::oauth_lock_dir_path(&home);
                if let Some(parent) = sentinel.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(sentinel)?;
                let deadline = Instant::now() + wait_timeout;
                let mut stale_candidate = None;
                loop {
                    match std::fs::create_dir(&lock_dir) {
                        Ok(()) => break,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            if try_reap_stale_directory(&lock_dir, &mut stale_candidate)? {
                                continue;
                            }
                            if Instant::now() >= deadline {
                                return Ok(None);
                            }
                            std::thread::sleep(LOCK_RETRY_INTERVAL);
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                let (stop_heartbeat, heartbeat) = start_directory_heartbeat(lock_dir.clone());
                RefreshLockKind::Directory {
                    path: lock_dir,
                    stop_heartbeat,
                    heartbeat,
                }
            }
        });
        let dead_refresh_fingerprints =
            load_dead_refresh_fingerprints(&paths::dead_refresh_fingerprint_path(&home));
        match std::fs::read_to_string(credential_path) {
            Ok(raw) => Ok(Some(LockedCredentials {
                guard,
                raw,
                dead_refresh_fingerprints,
            })),
            Err(error) => {
                release_lock_blocking(guard);
                Err(error.into())
            }
        }
    })
    .await
    .map_err(|e| Error::provider(format!("kimi credential lock task failed: {e}")))?
}

fn try_reap_stale_directory(
    lock_dir: &Path,
    candidate: &mut Option<(std::time::SystemTime, Instant)>,
) -> Result<bool> {
    let metadata = match std::fs::metadata(lock_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    let modified = metadata.modified()?;
    if modified.elapsed().unwrap_or_default() <= TYPESCRIPT_STALE_AFTER {
        *candidate = None;
        return Ok(false);
    }
    match candidate {
        Some((observed, since)) if *observed == modified => {
            if since.elapsed() < STALE_RECHECK_INTERVAL {
                return Ok(false);
            }
        }
        _ => {
            *candidate = Some((modified, Instant::now()));
            return Ok(false);
        }
    }
    if std::fs::metadata(lock_dir)?.modified()? != modified {
        *candidate = None;
        return Ok(false);
    }
    let unique = format!(
        "{}.kimi-switch-reap-{}-{}",
        lock_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("lock"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let reap_path = lock_dir.with_file_name(unique);
    match std::fs::rename(lock_dir, &reap_path) {
        Ok(()) => {
            std::fs::remove_dir_all(reap_path)?;
            *candidate = None;
            Ok(true)
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists
            ) =>
        {
            *candidate = None;
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

async fn release_lock(guard: RefreshLockGuard) {
    let _ = tokio::task::spawn_blocking(move || {
        release_lock_blocking(guard);
    })
    .await;
}

fn release_lock_blocking(guard: RefreshLockGuard) {
    drop(guard);
}

fn start_directory_heartbeat(path: PathBuf) -> (Sender<()>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || loop {
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = filetime::set_file_mtime(&path, filetime::FileTime::now());
            }
        }
    });
    (sender, handle)
}

/// 401 协调恢复的提交（ADR-0019）：轮换结果经引擎的轮换写回比对写回。交换锁内重读，库存仍持
/// 被消耗的 refresh token 才替换；当前登录文件仍属原账号、仍持被消耗的 token、原账号仍是
/// registry 当前账号，才写当前登录文件。恢复在途时切走了，当前登录文件不动，新凭证只落原账号
/// 库存。随后清掉被消耗 token 的拒止指纹，最后释放官方刷新锁。
///
/// 锁顺序：官方刷新锁 → 交换锁。提交是纯本地 IO，交换锁内不做网络请求；官方锁随提交同在
/// 阻塞任务里释放，查询被取消也不会在写回途中提前放锁。
async fn commit_rotation_and_release(
    guard: RefreshLockGuard,
    runtime: KimiRuntime,
    home: PathBuf,
    account: Account,
    consumed: String,
    rotated: String,
    fingerprint_to_clear: Option<String>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let result = (|| {
            runtime.commit_rotation(&home, &account, &consumed, &rotated)?;
            if let Some(fingerprint) = fingerprint_to_clear {
                remove_dead_refresh_fingerprint(
                    &paths::dead_refresh_fingerprint_path(&home),
                    &fingerprint,
                )?;
            }
            Ok(())
        })();
        release_lock_blocking(guard);
        result
    })
    .await
    .map_err(|e| Error::provider(format!("kimi credential write task failed: {e}")))?
}

async fn write_dead_fingerprint_and_release(
    guard: RefreshLockGuard,
    path: PathBuf,
    fingerprint: String,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let result = insert_dead_refresh_fingerprint(&path, fingerprint);
        release_lock_blocking(guard);
        result
    })
    .await
    .map_err(|e| Error::provider(format!("kimi dead-token guard task failed: {e}")))?
}

fn atomic_private_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::provider("kimi private write path has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let target_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("blob");
    let mut temporary = create_private_tempfile(parent, target_name)?;
    let mut leftover = TmpLeftover::new(temporary.path().to_path_buf());
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    persist_private_tempfile(temporary, path)?;
    leftover.disarm();
    Ok(())
}

/// 创建即私有的 NamedTempFile。Unix 用 mode 0600 打开；tmp 名带 pid+原子序号，同进程并发不撞名。
/// persist 仍走 NamedTempFile，以便 Windows 覆盖已有目标。
///
/// Windows 无 POSIX 0600，沿用 NamedTempFile 默认 ACL（继承父目录），不声称已强化。
fn create_private_tempfile(
    parent: &Path,
    target_name: &str,
) -> std::io::Result<tempfile::NamedTempFile> {
    let seq = PRIVATE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let prefix = format!(".{target_name}.tmp.{}-{seq}-", std::process::id());
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix).rand_bytes(0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }
    builder.tempfile_in(parent)
}

fn persist_private_tempfile(temporary: tempfile::NamedTempFile, dest: &Path) -> Result<()> {
    match temporary.persist(dest) {
        Ok(_) => Ok(()),
        Err(error) => {
            let tempfile::PersistError { error, file } = error;
            let leftover = file.path().to_path_buf();
            drop(file);
            let _ = std::fs::remove_file(&leftover);
            Err(error.into())
        }
    }
}

/// persist / set_permissions 失败时清掉仍留在目录里的 tmp。
struct TmpLeftover {
    path: Option<PathBuf>,
}

impl TmpLeftover {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TmpLeftover {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ---------------------------------------------------------------------------
// 当前账号主动续期（issue #44，ADR-0003 第 2 条）：无官方锁平台查询前轮换
// ---------------------------------------------------------------------------

/// 主动续期判定（薄壳，只做 IO；纯判定在 [`proactive_renewal_wanted_decision`]）：
/// 开关（issue #42）开启 + 本平台组合**确定无**官方刷新锁协议
/// + access token 已过期或剩余不足 60 秒。任一不满足都不碰 live；
/// 有官方锁的平台维持官方锁协调（`recover_active_401`）不变。
/// 锁探测 fail-closed（F1）：探测失败/不可解析/版本不识一律视为可能有锁，不放行。
pub(crate) async fn proactive_renewal_wanted(blob: &str) -> bool {
    // 开关关闭时不付探测代价（`kimi --version` 实测可达数秒）。
    if !proactive_renewal_setting_enabled() {
        return false;
    }
    let lock_probe = installed_kimi_lock_probe().await;
    let exp = extract_access_token(blob)
        .as_deref()
        .and_then(kimi_files::jwt_exp_claim);
    proactive_renewal_wanted_decision(true, lock_probe, exp, chrono::Utc::now().timestamp())
}

/// 主动续期门的纯判定（开关 × 锁探测三态 × exp 临期）：无 IO、无 `OnceLock`，可重复测试。
/// fail-closed：只有「确定无锁」([`LockProbeOutcome::NoLock`]) 才放行；`Unknown`
/// （探测失败/不可解析/版本不识）与 `Locked` 一样不放行。exp 语义与
/// [`kimi_files::exp_needs_renewal`] 完全一致：已过期或剩余不足 60 秒；None → false。
pub(crate) fn proactive_renewal_wanted_decision(
    setting_on: bool,
    lock_probe: LockProbeOutcome,
    exp: Option<i64>,
    now: i64,
) -> bool {
    setting_on && lock_probe == LockProbeOutcome::NoLock && kimi_files::exp_needs_renewal(exp, now)
}

/// 开关直读 config.toml：GUI 进程从不 reload 全局设置（issue #42 提示），
/// 消费方统一现读文件。文件缺失/解析失败一律视为关闭（安全方向）。
fn proactive_renewal_setting_enabled() -> bool {
    let Ok(paths) = kimi_switch_core::paths::AppPaths::resolve() else {
        return false;
    };
    match kimi_switch_core::settings::load_from(&paths.config_file()) {
        Ok(settings) => settings.proactive_renewal.enabled,
        Err(error) => {
            tracing::warn!(err = %error, "proactive renewal setting unreadable; treating as disabled");
            false
        }
    }
}

/// 解析指纹集合：一行一个哈希；旧版「整文件一个哈希」读取时仍算成员。
fn parse_dead_refresh_fingerprints(recorded: &str) -> HashSet<String> {
    recorded
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn load_dead_refresh_fingerprints(path: &Path) -> HashSet<String> {
    match std::fs::read_to_string(path) {
        Ok(recorded) => parse_dead_refresh_fingerprints(&recorded),
        Err(_) => HashSet::new(),
    }
}

fn persist_dead_refresh_fingerprints(path: &Path, fingerprints: &HashSet<String>) -> Result<()> {
    if fingerprints.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    } else {
        let mut lines: Vec<&str> = fingerprints.iter().map(String::as_str).collect();
        lines.sort_unstable();
        atomic_private_write(path, &lines.join("\n"))
    }
}

fn insert_dead_refresh_fingerprint(path: &Path, fingerprint: String) -> Result<()> {
    let mut fingerprints = load_dead_refresh_fingerprints(path);
    fingerprints.insert(fingerprint);
    persist_dead_refresh_fingerprints(path, &fingerprints)
}

fn remove_dead_refresh_fingerprint(path: &Path, fingerprint: &str) -> Result<()> {
    let mut fingerprints = load_dead_refresh_fingerprints(path);
    if fingerprints.remove(fingerprint) {
        persist_dead_refresh_fingerprints(path, &fingerprints)?;
    }
    Ok(())
}

fn blob_refresh_fingerprint(blob: &str) -> Option<String> {
    extract_refresh_token(blob).map(|token| refresh_fingerprint(&token))
}

/// DeadToken 纪律检查：blob 的 refresh token 指纹已在集合中（曾被官方 invalid_grant
/// 拒绝）→ 停止重试。与协调恢复路径共用同一指纹文件与哈希。
pub(crate) fn proactive_renewal_blocked(home: &Path, blob: &str) -> bool {
    let Some(fingerprint) = blob_refresh_fingerprint(blob) else {
        return false;
    };
    load_dead_refresh_fingerprints(&paths::dead_refresh_fingerprint_path(home))
        .contains(&fingerprint)
}

/// DeadToken 纪律处置：把被拒 refresh token 的指纹加入集合（幂等）并停止重试；
/// 本地凭证不删除、不墓碑化。返回本次查询应呈现的「凭证过期型 401」引导错误
/// （文案复用 issue #41，不新造）。
pub(crate) fn proactive_renewal_reject(home: &Path, blob: &str) -> Result<Error> {
    if let Some(fingerprint) = blob_refresh_fingerprint(blob) {
        insert_dead_refresh_fingerprint(&paths::dead_refresh_fingerprint_path(home), fingerprint)?;
    }
    Ok(Error::quota_credential_expired(
        crate::kimi_usage::CREDENTIAL_EXPIRED_GUIDANCE,
    ))
}

/// 轮换成功收尾：只从集合去掉这次轮换前 blob 的指纹，不得清空其他账号的拒止记录。
/// 集合空了再删文件。
pub(crate) fn proactive_renewal_rotated(home: &Path, blob: &str) -> Result<()> {
    let Some(fingerprint) = blob_refresh_fingerprint(blob) else {
        return Ok(());
    };
    remove_dead_refresh_fingerprint(&paths::dead_refresh_fingerprint_path(home), &fingerprint)
}

fn refresh_fingerprint(refresh_token: &str) -> String {
    format!("{:x}", Sha256::digest(refresh_token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::recovery_runtime;
    use chrono::Utc;
    use kimi_switch_core::AccountId;

    #[test]
    fn recognizes_first_lock_capable_version() {
        assert_eq!(parse_version("kimi-cli 1.31.0"), Some((1, 31, 0)));
        assert_eq!(parse_version("kimi 1.49.0\n"), Some((1, 49, 0)));
        assert_eq!(parse_version("kimi-cli 0.28.1"), Some((0, 28, 1)));
        assert_eq!(lock_protocol_from_version_output("kimi-cli 0.28.1"), None);
        assert_eq!(lock_protocol_from_version_output("kimi 0.28.1"), None);
        assert_eq!(
            lock_protocol_for_version((1, 31, 0)),
            Some(RefreshLockProtocol::PythonFile)
        );
        assert_eq!(lock_protocol_for_version((1, 30, 9)), None);
        assert_eq!(lock_protocol_for_version((2, 0, 0)), None);
        assert_eq!(lock_protocol_for_version((3, 1, 0)), None);
        #[cfg(not(windows))]
        {
            assert_eq!(
                lock_protocol_from_version_output("0.28.1"),
                Some(RefreshLockProtocol::TypeScriptDirectory)
            );
            assert_eq!(
                protocol_with_lock_setting(RefreshLockProtocol::TypeScriptDirectory, true),
                None
            );
        }
        #[cfg(windows)]
        assert_eq!(lock_protocol_for_version((0, 28, 1)), None);
    }

    /// F1：探测结论必须区分「确定无锁」与「不确定」——只有前者允许主动续期放行。
    #[test]
    fn provably_lock_free_distinguishes_determined_from_unknown() {
        // 输出不可解析 / 旧 Python 0.x 前缀噪音 → 不确定（绝不许当成无锁放行）。
        assert!(!provably_lock_free("garbage"));
        assert!(!provably_lock_free("kimi-cli 0.28.1"));
        // 2.x+ 主版本不识 → 不确定（fail-closed，不臆断无锁）。
        assert!(!provably_lock_free("2.0.0"));
        // Python 1.x 早于 1.31（协调锁尚未引入）→ 确定无锁。
        assert!(provably_lock_free("kimi 1.30.9"));
        // TS 0.x：仅 Windows 确定无锁（ADR-0003）；Unix 有 proper-lockfile 目录锁。
        #[cfg(windows)]
        assert!(provably_lock_free("0.28.1"));
        #[cfg(not(windows))]
        assert!(!provably_lock_free("0.28.1"));
    }

    /// F1/F2：续期门纯判定钉住全部分支（无 IO、无 OnceLock，进程内可重复测）。
    #[test]
    fn proactive_renewal_wanted_decision_pins_all_branches() {
        let wanted = proactive_renewal_wanted_decision;
        let no_lock = LockProbeOutcome::NoLock;
        let unknown = LockProbeOutcome::Unknown;
        let locked = LockProbeOutcome::Locked(RefreshLockProtocol::PythonFile);
        // 开关关 → false（其余条件再满足也不放行）。
        assert!(!wanted(false, no_lock, Some(1000), 2000));
        // 确定有锁 → false（红线方向：有锁平台走官方锁协调，不自创并行刷新）。
        assert!(!wanted(true, locked, Some(1000), 2000));
        // 探测不确定 → false（F1 fail-closed：探测失败/不可解析/版本不识都不放行）。
        assert!(!wanted(true, unknown, Some(1000), 2000));
        // 开关开 + 确定无锁 + 已过期（now >= exp）→ true。
        assert!(wanted(true, no_lock, Some(1000), 2000));
        // 剩余不足 60 秒 → true。
        assert!(wanted(true, no_lock, Some(2059), 2000));
        // 恰好剩 60 秒 → false（沿用 `jwt_needs_renewal` 既有边界：不足 60 秒才轮换）。
        assert!(!wanted(true, no_lock, Some(2060), 2000));
        // exp 无法确证（None）→ false：不能确证临期就不消耗一次性 refresh token。
        assert!(!wanted(true, no_lock, None, 2000));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn waits_for_official_lock_then_reuses_rotated_credentials() {
        let temporary = tempfile::tempdir().unwrap();
        let credential_path = paths::active_cred_path(temporary.path());
        let lock_path = paths::credentials_lock_path(temporary.path());
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        let old_access = jwt("OLD");
        std::fs::write(&credential_path, blob(&old_access, "R1")).unwrap();

        let mut official_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        official_lock.write_all(&[0]).unwrap();
        official_lock.lock_exclusive().unwrap();

        let home = temporary.path().to_path_buf();
        let account = active_account();
        let attempted = old_access.clone();
        let runtime = recovery_runtime(temporary.path());
        let recovery = tokio::spawn(async move {
            recover_active_401_at(
                &attempted,
                &account,
                &home,
                "http://127.0.0.1:1",
                RefreshLockProtocol::PythonFile,
                &runtime,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!recovery.is_finished());

        let new_access = jwt("NEW");
        std::fs::write(&credential_path, blob(&new_access, "R2")).unwrap();
        official_lock.unlock().unwrap();

        assert_eq!(recovery.await.unwrap().unwrap(), Some(new_access));
        assert_eq!(
            extract_refresh_token(&std::fs::read_to_string(credential_path).unwrap()).as_deref(),
            Some("R2")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(paths::credentials_lock_path(temporary.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_typescript_client_has_no_active_refresh_lock_protocol() {
        assert_eq!(lock_protocol_from_version_output("0.28.1"), None);
        assert_eq!(lock_protocol_for_version((0, 99, 0)), None);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn waits_for_proper_lock_directory_then_reuses_rotated_credentials() {
        let temporary = tempfile::tempdir().unwrap();
        let credential_path = paths::active_cred_path(temporary.path());
        let sentinel_path = paths::oauth_lock_sentinel_path(temporary.path());
        let lock_dir = paths::oauth_lock_dir_path(temporary.path());
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(sentinel_path.parent().unwrap()).unwrap();
        std::fs::write(&sentinel_path, "").unwrap();
        std::fs::create_dir(&lock_dir).unwrap();
        let old_access = jwt("OLD");
        std::fs::write(&credential_path, blob(&old_access, "R1")).unwrap();

        let home = temporary.path().to_path_buf();
        let account = active_account();
        let attempted = old_access.clone();
        let runtime = recovery_runtime(temporary.path());
        let recovery = tokio::spawn(async move {
            recover_active_401_at(
                &attempted,
                &account,
                &home,
                "http://127.0.0.1:1",
                RefreshLockProtocol::TypeScriptDirectory,
                &runtime,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!recovery.is_finished());

        let new_access = jwt("NEW");
        std::fs::write(&credential_path, blob(&new_access, "R2")).unwrap();
        std::fs::remove_dir(&lock_dir).unwrap();

        assert_eq!(recovery.await.unwrap().unwrap(), Some(new_access));
        assert!(sentinel_path.is_file());
        assert!(!lock_dir.exists());
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn proper_lock_heartbeat_keeps_directory_fresh_beyond_stale_window() {
        let temporary = tempfile::tempdir().unwrap();
        let credential_path = paths::active_cred_path(temporary.path());
        let lock_dir = paths::oauth_lock_dir_path(temporary.path());
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        std::fs::write(&credential_path, blob(&jwt("OLD"), "R1")).unwrap();

        let locked = acquire_and_read(
            temporary.path().to_path_buf(),
            credential_path,
            RefreshLockProtocol::TypeScriptDirectory,
        )
        .await
        .unwrap()
        .unwrap();
        let initial_mtime = std::fs::metadata(&lock_dir).unwrap().modified().unwrap();
        tokio::time::sleep(Duration::from_millis(5_200)).await;
        let refreshed_mtime = std::fs::metadata(&lock_dir).unwrap().modified().unwrap();

        assert!(refreshed_mtime > initial_mtime);
        assert!(refreshed_mtime.elapsed().unwrap() < Duration::from_secs(3));
        assert_eq!(
            std::fs::create_dir(&lock_dir).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        release_lock(locked.guard).await;
        assert!(!lock_dir.exists());
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn stale_proper_lock_is_atomically_reaped_but_fresh_lock_is_not() {
        let stale_home = tempfile::tempdir().unwrap();
        let stale_credentials = paths::active_cred_path(stale_home.path());
        let stale_lock = paths::oauth_lock_dir_path(stale_home.path());
        std::fs::create_dir_all(stale_credentials.parent().unwrap()).unwrap();
        std::fs::create_dir_all(stale_lock.parent().unwrap()).unwrap();
        std::fs::write(&stale_credentials, blob(&jwt("OLD"), "R1")).unwrap();
        std::fs::create_dir(&stale_lock).unwrap();
        filetime::set_file_mtime(
            &stale_lock,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - Duration::from_secs(20),
            ),
        )
        .unwrap();
        let stale_acquired = acquire_and_read_with_timeout(
            stale_home.path().to_path_buf(),
            stale_credentials,
            RefreshLockProtocol::TypeScriptDirectory,
            Duration::from_secs(1),
        )
        .await
        .unwrap()
        .unwrap();
        release_lock(stale_acquired.guard).await;
        assert!(!stale_lock.exists());

        let fresh_home = tempfile::tempdir().unwrap();
        let fresh_credentials = paths::active_cred_path(fresh_home.path());
        let fresh_lock = paths::oauth_lock_dir_path(fresh_home.path());
        std::fs::create_dir_all(fresh_credentials.parent().unwrap()).unwrap();
        std::fs::create_dir_all(fresh_lock.parent().unwrap()).unwrap();
        std::fs::write(&fresh_credentials, blob(&jwt("OLD"), "R1")).unwrap();
        std::fs::create_dir(&fresh_lock).unwrap();
        let fresh_acquired = acquire_and_read_with_timeout(
            fresh_home.path().to_path_buf(),
            fresh_credentials,
            RefreshLockProtocol::TypeScriptDirectory,
            Duration::from_millis(250),
        )
        .await
        .unwrap();
        assert!(fresh_acquired.is_none());
        assert!(fresh_lock.is_dir());
        std::fs::remove_dir(fresh_lock).unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn dead_refresh_fingerprint_prevents_repeated_http_requests() {
        use crate::test_support::MockServer;

        let temporary = tempfile::tempdir().unwrap();
        let credential_path = paths::active_cred_path(temporary.path());
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        let access = jwt("OLD");
        std::fs::write(&credential_path, blob(&access, "R1")).unwrap();
        let server = MockServer::start(vec![("401 Unauthorized", r#"{"error":"invalid_grant"}"#)]);
        let runtime = recovery_runtime(temporary.path());

        assert!(recover_active_401_at(
            &access,
            &active_account(),
            temporary.path(),
            server.base_url(),
            RefreshLockProtocol::TypeScriptDirectory,
            &runtime,
        )
        .await
        .unwrap()
        .is_none());
        assert!(recover_active_401_at(
            &access,
            &active_account(),
            temporary.path(),
            server.base_url(),
            RefreshLockProtocol::TypeScriptDirectory,
            &runtime,
        )
        .await
        .unwrap()
        .is_none());

        assert_eq!(server.finish().len(), 1);
        let guard_path = paths::dead_refresh_fingerprint_path(temporary.path());
        let fingerprint = std::fs::read_to_string(&guard_path).unwrap();
        assert_eq!(fingerprint, refresh_fingerprint("R1"));
        assert!(!fingerprint.contains("R1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(guard_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn cancelling_refresh_releases_proper_lock_and_stops_heartbeat() {
        use std::io::Read;
        use std::net::TcpListener;

        let temporary = tempfile::tempdir().unwrap();
        let credential_path = paths::active_cred_path(temporary.path());
        let lock_dir = paths::oauth_lock_dir_path(temporary.path());
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        let access = jwt("OLD");
        std::fs::write(&credential_path, blob(&access, "R1")).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let oauth_base = format!("http://{}", listener.local_addr().unwrap());
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            accepted_tx.send(()).unwrap();
            let _ = finish_rx.recv_timeout(Duration::from_secs(2));
        });
        let home = temporary.path().to_path_buf();
        let account = active_account();
        let runtime = recovery_runtime(temporary.path());
        let recovery = tokio::spawn(async move {
            recover_active_401_at(
                &access,
                &account,
                &home,
                &oauth_base,
                RefreshLockProtocol::TypeScriptDirectory,
                &runtime,
            )
            .await
        });
        tokio::task::spawn_blocking(move || accepted_rx.recv_timeout(Duration::from_secs(2)))
            .await
            .unwrap()
            .unwrap();
        assert!(lock_dir.is_dir());
        recovery.abort();
        assert!(recovery.await.unwrap_err().is_cancelled());
        assert!(!lock_dir.exists());
        finish_tx.send(()).unwrap();
        server.join().unwrap();
    }

    /// 401 协调恢复在途时切走（spec #185 用户故事 7、8）：刷新请求已发出、服务端已消耗 R1 时，
    /// 用户把当前账号切到 u-456。放行后当前登录文件仍是 u-456 的凭证，原账号 u-123 的库存持
    /// 轮换出的新 token。Python 文件锁协议在 Windows 上同样可用，本用例不加平台门控。
    #[tokio::test]
    async fn recovery_in_flight_then_switch_keeps_live_and_stores_new_token() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().to_path_buf();
        let credential_path = paths::active_cred_path(&home);
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        let runtime = recovery_runtime(&home);
        let old_access = jwt("OLD");
        let original = blob(&old_access, "R1");
        std::fs::write(&credential_path, &original).unwrap();
        // 额度查询读取当前账号凭证时已把 live 回灌进库存。
        runtime
            .store
            .set("kimi", "u-123", "blob", &original)
            .unwrap();
        runtime.registry.upsert(active_account()).unwrap();
        let other = parked_account("u-456");
        runtime.registry.upsert(other.clone()).unwrap();
        let other_blob = blob(
            &crate::test_support::jwt_with_payload(r#"{"user_id":"u-456","client_id":"c-1"}"#),
            "R9",
        );
        let new_access = jwt("NEW");
        let server = HeldRefreshServer::start(format!(
            r#"{{"access_token":"{new_access}","refresh_token":"R2","expires_in":3600,"scope":"kimi-code","token_type":"Bearer"}}"#
        ));

        let recovery = {
            let attempted = old_access.clone();
            let home = home.clone();
            let oauth_base = server.base_url.clone();
            let runtime = runtime.clone();
            tokio::spawn(async move {
                recover_active_401_at(
                    &attempted,
                    &active_account(),
                    &home,
                    &oauth_base,
                    RefreshLockProtocol::PythonFile,
                    &runtime,
                )
                .await
            })
        };
        let server = server.wait_received().await;
        // 恢复在途时切到 u-456：与 activate 提交时一样，在交换锁内写当前登录文件、改 registry 当前标记。
        kimi_switch_core::swap::with_swap_lock(&home, || {
            std::fs::write(&credential_path, &other_blob)?;
            runtime.registry.set_active("kimi", &other.id)
        })
        .unwrap();
        server.release_and_finish();

        assert_eq!(recovery.await.unwrap().unwrap(), Some(new_access));
        assert_eq!(
            std::fs::read_to_string(&credential_path).unwrap(),
            other_blob,
            "恢复在途时已切走：恢复结果绝不覆盖当前登录文件"
        );
        let stored = runtime.store.get("kimi", "u-123", "blob").unwrap().unwrap();
        assert_eq!(
            extract_refresh_token(&stored).as_deref(),
            Some("R2"),
            "当前登录文件写入放弃后，新凭证必须落进原账号库存"
        );
    }

    /// 未发生切换时恢复照常写当前登录文件（Python 文件锁协议，Windows 本机可跑）：交换锁内
    /// 比对通过，库存仍持被消耗的 R1，新凭证先替换库存、再原子写进当前登录文件。
    #[tokio::test]
    async fn recovery_without_switch_writes_live_and_store() {
        use crate::test_support::MockServer;

        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let credential_path = paths::active_cred_path(home);
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        let runtime = recovery_runtime(home);
        let old_access = jwt("OLD");
        let original = blob(&old_access, "R1");
        std::fs::write(&credential_path, &original).unwrap();
        runtime
            .store
            .set("kimi", "u-123", "blob", &original)
            .unwrap();
        runtime.registry.upsert(active_account()).unwrap();
        let server = MockServer::start(vec![(
            "200 OK",
            r#"{"access_token":"NEW","refresh_token":"R2","expires_in":3600}"#,
        )]);

        let fresh = recover_active_401_at(
            &old_access,
            &active_account(),
            home,
            server.base_url(),
            RefreshLockProtocol::PythonFile,
            &runtime,
        )
        .await
        .unwrap();

        assert_eq!(fresh.as_deref(), Some("NEW"));
        let live = std::fs::read_to_string(&credential_path).unwrap();
        assert_eq!(
            extract_refresh_token(&live).as_deref(),
            Some("R2"),
            "未发生切换：恢复照常写当前登录文件"
        );
        let stored = runtime.store.get("kimi", "u-123", "blob").unwrap().unwrap();
        assert_eq!(extract_refresh_token(&stored).as_deref(), Some("R2"));
        assert_eq!(server.finish(), vec!["POST /api/oauth/token HTTP/1.1"]);
    }

    /// 等官方刷新锁可能用掉本次额度查询 attempt 的大半预算（锁等待上限 15 秒，单次预算默认
    /// 20 秒）。拿到锁后剩余时间盖不住轮换请求的超时上限加 2 秒余量时不发刷新：放锁，按未恢复的
    /// 401 返回，留给下次查询（ADR-0019 决策 7）。照发的话外层超时可能在响应返回前取消查询，
    /// 丢掉服务端已消耗的 refresh token。
    #[tokio::test]
    async fn recovery_too_late_in_attempt_leaves_refresh_to_next_query() {
        use crate::test_support::MockServer;

        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let credential_path = paths::active_cred_path(home);
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        let runtime = recovery_runtime(home);
        let old_access = jwt("OLD");
        let original = blob(&old_access, "R1");
        std::fs::write(&credential_path, &original).unwrap();
        runtime
            .store
            .set("kimi", "u-123", "blob", &original)
            .unwrap();
        runtime.registry.upsert(active_account()).unwrap();
        let server = MockServer::start(vec![(
            "200 OK",
            r#"{"access_token":"NEW","refresh_token":"R2","expires_in":3600}"#,
        )]);
        // 本次 attempt 只剩轮换超时上限 + 1 秒，不够 2 秒余量。
        let deadline = tokio::time::Instant::now() + REFRESH_HTTP_TIMEOUT + Duration::from_secs(1);

        let fresh = kimi_switch_core::quota_query::with_attempt_deadline(
            deadline,
            recover_active_401_at(
                &old_access,
                &active_account(),
                home,
                server.base_url(),
                RefreshLockProtocol::PythonFile,
                &runtime,
            ),
        )
        .await
        .expect("不发刷新，attempt 远在超时前结束")
        .unwrap();

        assert_eq!(fresh, None, "剩余时间不够：不发刷新，按未恢复的 401 呈现");
        assert_eq!(server.received(), 0, "刷新请求不得发出");
        assert_eq!(std::fs::read_to_string(&credential_path).unwrap(), original);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(paths::credentials_lock_path(home))
            .unwrap();
        lock.try_lock_exclusive()
            .expect("放弃刷新时必须放开官方刷新锁");
    }

    /// 本地 OAuth 替身：收下刷新请求后先停住（此刻服务端已消耗旧 refresh token），测试放行后
    /// 才回 200 与 `body`。用它构造「刷新在途」的交错，不靠 sleep 赌时序。
    struct HeldRefreshServer {
        base_url: String,
        received: tokio::sync::oneshot::Receiver<()>,
        release: Sender<()>,
        handle: JoinHandle<()>,
    }

    /// 已收到刷新请求、停在放行前的替身。
    struct HeldRequest {
        release: Sender<()>,
        handle: JoinHandle<()>,
    }

    impl HeldRefreshServer {
        fn start(body: String) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let (received_tx, received) = tokio::sync::oneshot::channel();
            let (release, release_rx) = mpsc::channel::<()>();
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_full_request(&mut stream);
                let _ = received_tx.send(());
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("测试应在 5 秒内放行刷新响应");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });
            Self {
                base_url,
                received,
                release,
                handle,
            }
        }

        /// 等到刷新请求已到达替身、停在放行前。
        async fn wait_received(self) -> HeldRequest {
            tokio::time::timeout(Duration::from_secs(5), self.received)
                .await
                .expect("刷新请求应在 5 秒内到达")
                .unwrap();
            HeldRequest {
                release: self.release,
                handle: self.handle,
            }
        }
    }

    impl HeldRequest {
        /// 放行刷新响应，并等替身写完。
        fn release_and_finish(self) {
            self.release.send(()).unwrap();
            self.handle.join().unwrap();
        }
    }

    /// 读完一个 HTTP 请求（请求头 + Content-Length 指明的 body），避免关连接时残留未读字节。
    fn read_full_request(stream: &mut std::net::TcpStream) {
        use std::io::Read;
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            if count == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..count]);
            let text = String::from_utf8_lossy(&request);
            let Some(head_end) = text.find("\r\n\r\n") else {
                continue;
            };
            let body_length = text[..head_end]
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if request.len() >= head_end + 4 + body_length {
                return;
            }
        }
    }

    fn jwt(suffix: &str) -> String {
        format!("header.eyJ1c2VyX2lkIjoidS0xMjMiLCJjbGllbnRfaWQiOiJjLTEifQ.{suffix}")
    }

    fn blob(access: &str, refresh: &str) -> String {
        format!(r#"{{"access_token":"{access}","refresh_token":"{refresh}"}}"#)
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

    fn parked_account(id: &str) -> Account {
        Account {
            id: AccountId(id.into()),
            label: id.into(),
            active: false,
            ..active_account()
        }
    }

    /// issue #81：OAuth 刷新 HTTP 错误只带状态码，不得回显响应 body（body 可能含 token / 内部细节）。
    #[tokio::test]
    async fn oauth_refresh_http_error_omits_response_body() {
        use crate::test_support::MockServer;

        let leak = "should-not-leak-refresh-body";
        let server = MockServer::start(vec![(
            "500 Internal Server Error",
            r#"{"error":"server_exploded","secret":"should-not-leak-refresh-body"}"#,
        )]);
        let blob = r#"{"access_token":"header.eyJ1c2VyX2lkIjoidS0xMjMiLCJjbGllbnRfaWQiOiJjLTEifQ.sig","refresh_token":"R1"}"#;

        let err = match refresh_blob_at(blob, server.base_url()).await {
            Ok(_) => panic!("非 2xx 刷新应失败"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert_eq!(err.http_status(), Some(500), "{msg}");
        assert!(msg.contains("500"), "错误文案须含状态码：{msg}");
        assert!(!msg.contains(leak), "错误文案不得含响应 body：{msg}");
        assert!(
            !msg.contains("server_exploded"),
            "错误文案不得含响应 body：{msg}"
        );
        assert_eq!(server.finish(), vec!["POST /api/oauth/token HTTP/1.1"]);
    }

    #[test]
    fn oauth_host_from_pins_official_constant() {
        assert_eq!(oauth_host_from(None), "https://auth.kimi.com");
        assert_eq!(oauth_host_from(Some("")), "https://auth.kimi.com");
        assert_eq!(oauth_host_from(Some("  ")), "https://auth.kimi.com");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn oauth_host_from_honors_override_in_debug() {
        assert_eq!(
            oauth_host_from(Some("http://127.0.0.1:9/")),
            "http://127.0.0.1:9"
        );
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn oauth_host_from_ignores_override_in_release() {
        assert_eq!(
            oauth_host_from(Some("http://evil.example")),
            "https://auth.kimi.com"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_tempfile_is_0600_at_creation() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let tmp = create_private_tempfile(dir.path(), "kimi-code.json").unwrap();
        assert_eq!(
            tmp.as_file().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn private_tempfile_names_are_unique_under_concurrency() {
        use std::collections::HashSet;

        let dir = tempfile::tempdir().unwrap();
        let paths = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..32 {
                scope.spawn(|| {
                    let tmp = create_private_tempfile(dir.path(), "kimi-code.json").unwrap();
                    paths.lock().unwrap().push(tmp.path().to_path_buf());
                });
            }
        });
        let paths = paths.into_inner().unwrap();
        let unique: HashSet<_> = paths.iter().cloned().collect();
        assert_eq!(unique.len(), paths.len(), "同进程并发不得写同一 tmp 名");
    }

    #[test]
    fn atomic_private_write_cleans_tmp_when_persist_fails() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("not-a-file");
        std::fs::create_dir(&target).unwrap();

        let err = atomic_private_write(&target, "secret-token-value").unwrap_err();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "not-a-file")
            .collect();
        assert!(
            leftovers.is_empty(),
            "persist 失败须清理 tmp 残留：{leftovers:?}; {err}"
        );
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn atomic_private_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cred.json");
        atomic_private_write(&path, "first").unwrap();
        atomic_private_write(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    /// 主动续期的 DeadToken 纪律（issue #44）：与协调恢复共用同一指纹文件与哈希，
    /// 拒绝后记录在案 → 停止重试 → 成功轮换后清除。全平台可跑（不触碰锁）。
    #[test]
    fn proactive_renewal_reject_records_fingerprint_and_blocks_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let blob = r#"{"access_token":"a","refresh_token":"R1"}"#;

        assert!(!proactive_renewal_blocked(home, blob));
        let err = proactive_renewal_reject(home, blob).unwrap();
        assert_eq!(
            err.quota_failure().map(|f| f.kind),
            Some(kimi_switch_core::error::QuotaFailureKind::CredentialExpired)
        );
        assert_eq!(err.http_status(), Some(401));
        assert!(err.is_authentication_failure());
        assert!(!err.is_retryable());
        assert!(
            err.to_string()
                .contains(crate::kimi_usage::CREDENTIAL_EXPIRED_GUIDANCE),
            "复用 #41 引导文案，不新造：{err}"
        );
        let recorded = std::fs::read_to_string(paths::dead_refresh_fingerprint_path(home)).unwrap();
        assert_eq!(recorded, refresh_fingerprint("R1"));
        assert!(!recorded.contains("R1"), "指纹不得含 token 原文");
        assert!(proactive_renewal_blocked(home, blob), "指纹在案 → 停止重试");
        // 其他 refresh token 不在案。
        assert!(!proactive_renewal_blocked(
            home,
            r#"{"access_token":"a","refresh_token":"R2"}"#
        ));
        // 轮换成功收尾只清掉这次 blob 的指纹；重复清除是 no-op。
        proactive_renewal_rotated(home, blob).unwrap();
        assert!(!paths::dead_refresh_fingerprint_path(home).exists());
        proactive_renewal_rotated(home, blob).unwrap();
        // 无 refresh token 的 blob：不写指纹，仍给出引导错误。
        let err = proactive_renewal_reject(home, r#"{"access_token":"a"}"#).unwrap();
        assert_eq!(err.http_status(), Some(401));
        assert!(!paths::dead_refresh_fingerprint_path(home).exists());
    }

    /// 旧版整文件一个哈希仍须挡住对应 token（集合读取兼容）。
    #[test]
    fn legacy_single_hash_dead_refresh_file_still_blocks() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let path = paths::dead_refresh_fingerprint_path(home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, refresh_fingerprint("R1")).unwrap();
        assert!(proactive_renewal_blocked(
            home,
            r#"{"access_token":"a","refresh_token":"R1"}"#
        ));
        assert!(!proactive_renewal_blocked(
            home,
            r#"{"access_token":"a","refresh_token":"R2"}"#
        ));
    }

    /// 两账号先后 reject：后写加入集合，不得覆盖前一个指纹。
    #[test]
    fn proactive_renewal_reject_keeps_both_fingerprints() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let blob1 = r#"{"access_token":"a","refresh_token":"R1"}"#;
        let blob2 = r#"{"access_token":"a","refresh_token":"R2"}"#;
        proactive_renewal_reject(home, blob1).unwrap();
        proactive_renewal_reject(home, blob2).unwrap();
        assert!(proactive_renewal_blocked(home, blob1));
        assert!(proactive_renewal_blocked(home, blob2));
        let recorded = std::fs::read_to_string(paths::dead_refresh_fingerprint_path(home)).unwrap();
        let h1 = refresh_fingerprint("R1");
        let h2 = refresh_fingerprint("R2");
        assert!(
            recorded.lines().any(|line| line.trim() == h1),
            "R1 指纹必须仍在：{recorded}"
        );
        assert!(
            recorded.lines().any(|line| line.trim() == h2),
            "R2 指纹必须写入且不得覆盖 R1：{recorded}"
        );
        assert!(!recorded.contains("R1"));
        assert!(!recorded.contains("R2"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(paths::dead_refresh_fingerprint_path(home))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    /// 轮换成功只去掉这次 blob 的指纹，其它停泊死号仍停试。
    #[test]
    fn proactive_renewal_rotated_removes_only_that_fingerprint() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let blob1 = r#"{"access_token":"a","refresh_token":"R1"}"#;
        let blob2 = r#"{"access_token":"a","refresh_token":"R2"}"#;
        proactive_renewal_reject(home, blob1).unwrap();
        proactive_renewal_reject(home, blob2).unwrap();
        proactive_renewal_rotated(home, blob1).unwrap();
        assert!(!proactive_renewal_blocked(home, blob1));
        assert!(proactive_renewal_blocked(home, blob2));
        assert!(paths::dead_refresh_fingerprint_path(home).exists());
    }
}
