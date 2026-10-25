//! 本地凭证失效信号判定（issue #158，spec #152 实现决策 15；口径经 #158 续跑裁定收紧）。
//!
//! 「已确认失效」= 凭证 blob 的 refresh token SHA-256 指纹命中被官方 invalid_grant 拒止的
//! 本地集合——这是唯一可纯本地确证的死信号。裁定只认指纹、不认 JWT exp（偏离 spec
//! 决策 15 原措辞「JWT 已过期或指纹」），理由：
//! ① 停泊账号 access token 只在额度轮询时轮换保鲜，GUI 长期关闭后全部自然过期——恰是
//!   #162 被动切换的主场景，exp 口径会把全部候选排除、被动切换静默落空；
//! ② access 过期但 refresh 有效的账号激活后下次额度查询自愈，不是真死卡；
//! ③ 与 ADR-0009「无法确证不算失效」同向。#156（手动死卡提示）将复用同一口径。
//!
//! 判定纯本地：只读本机文件与内存字符串，零网络、零定时器；无法确证（文件缺失、blob
//! 非 JSON、无 refresh_token、指纹未命中）一律不算失效——unknown ≠ 失效。
//!
//! 语义与 `crates/providers/kimi` 的既有实现保持一致（那边是写入方与 owner，此处只读
//! 复刻，若 owner 侧调整布局或口径需同步本模块）：
//! - 指纹哈希与集合文件格式：`oauth::{refresh_fingerprint, parse_dead_refresh_fingerprints}`；
//! - Kimi 工作目录与拒止指纹文件路径：`paths::{kimi_home, dead_refresh_fingerprint_path}`。
//!
//! 复用方：自动切换候选过滤（[`crate::auto_swap`]，issue #158）、手动切换死卡提示（#156）、
//! 配额失败被动切换（#162）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::model::{Account, AccountId};

/// Kimi 凭证 blob 里 refresh token 的键名。
const REFRESH_TOKEN_KEY: &str = "refresh_token";

/// Kimi 账号在本工具凭证仓库（`credentials.json`）里的字段名，对应
/// `FileBlobRuntime::store_field` 的 Kimi 取值（默认 "blob"）。core 不依赖 provider crate，
/// 此处按该取值只读；其它 provider 的字段布局未知 → 视为信号未知，不参与判定。
const KIMI_STORE_FIELD: &str = "blob";
/// 当前唯一走文件型 OAuth 切换引擎的 provider id。
const KIMI_PROVIDER_ID: &str = "kimi";

/// 本地凭证失效判定（共享原语）：refresh token 指纹命中拒止集合即失效；其余一律
/// 不失效——包括 access token 已过 JWT exp（不构成死信号，见模块级裁定说明①②）。
/// 纯函数。
pub fn is_locally_invalid(blob: &str, dead_refresh_fingerprints: &HashSet<String>) -> bool {
    blob_refresh_fingerprint(blob)
        .is_some_and(|fingerprint| dead_refresh_fingerprints.contains(&fingerprint))
}

/// refresh token 的拒止指纹：SHA-256 十六进制（小写、无前缀），与
/// `oauth::refresh_fingerprint` 逐字节一致——指纹文件按字符串比较成员，口径必须相同。
pub fn refresh_fingerprint(refresh_token: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(refresh_token.as_bytes()))
}

/// 从凭证 blob 取 refresh token 指纹；blob 无法解析出 refresh token 时返回 `None`。
pub fn blob_refresh_fingerprint(blob: &str) -> Option<String> {
    let token = blob_string_field(blob, REFRESH_TOKEN_KEY)?;
    Some(refresh_fingerprint(&token))
}

/// 读取拒止指纹文件：一行一个哈希（旧版整文件单哈希读取时同样算成员）。
/// 文件缺失或不可读 → 空集合（unknown ≠ 失效）。只读，绝不写。
pub fn load_dead_refresh_fingerprints(path: &Path) -> HashSet<String> {
    match std::fs::read_to_string(path) {
        Ok(recorded) => recorded
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => HashSet::new(),
    }
}

/// 收集账号库中「本地凭证已确认失效」的账号 id 集合，供自动切换决策一次取用
/// （issue #158：耗尽 / pace / 回驻 / 重置前抢用与回程 / 被动路径共用）。
///
/// 只读两个本机文件：`<data_dir>/credentials.json`（由 state 文件路径反推，见
/// [`credentials_file_from_state_path`]）与 Kimi 工作目录下的拒止指纹文件；判定只认
/// 指纹命中（见模块级裁定）。任一读取失败、解析失败或账号非 Kimi → 该账号不进集合
/// （fail-open，零网络）。
///
/// `_now` 为决策基准时刻：上一口径（JWT exp）消费它，当前指纹口径不消费；保留参数
/// 以稳定调用方契约（裁定可被推翻，恢复时间依赖口径时无需波及 auto_swap 门面）。
pub fn locally_invalid_accounts(
    state_path: &Path,
    accounts: &[Account],
    _now: DateTime<Utc>,
) -> HashSet<AccountId> {
    let dead = load_dead_refresh_fingerprints(&dead_refresh_fingerprint_path(&kimi_home()));
    let blobs = load_credential_blobs(&credentials_file_from_state_path(state_path), accounts);
    invalid_account_ids(&blobs, &dead)
}

/// 纯核：给定各 Kimi 账号的凭证 blob 与拒止指纹集合，产出「已确认失效」账号 id 集合。
fn invalid_account_ids(
    blobs: &HashMap<AccountId, String>,
    dead_refresh_fingerprints: &HashSet<String>,
) -> HashSet<AccountId> {
    blobs
        .iter()
        .filter(|(_, blob)| is_locally_invalid(blob, dead_refresh_fingerprints))
        .map(|(id, _)| id.clone())
        .collect()
}

/// `<data_dir>/state/auto_swap.json` → `<data_dir>/credentials.json`
/// （[`crate::paths::AppPaths`] 的布局：state 挂在 data 下）。路径形态不符时得到
/// 一个不存在的路径，后续读取 fail-open。
fn credentials_file_from_state_path(state_path: &Path) -> PathBuf {
    match state_path.parent().and_then(Path::parent) {
        Some(data_dir) => data_dir.join("credentials.json"),
        None => PathBuf::from("credentials.json"),
    }
}

/// 从凭证仓库文件（[`crate::store::FileStore`] 的磁盘格式：`{ "provider:account:field": value }`）
/// 只读取出各 Kimi 账号的 blob。文件缺失 / 损坏 / 账号无条目 → 该账号无 blob（不进判定）。
/// 直接按格式读文件而非走 `FileStore::get`：后者会创建锁文件与目录，决策路径不应有写副作用。
fn load_credential_blobs(
    credentials_file: &Path,
    accounts: &[Account],
) -> HashMap<AccountId, String> {
    let Ok(raw) = std::fs::read_to_string(credentials_file) else {
        return HashMap::new();
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&raw) else {
        return HashMap::new();
    };
    accounts
        .iter()
        .filter(|account| account.provider == KIMI_PROVIDER_ID)
        .filter_map(|account| {
            let key = format!("{}:{}:{}", account.provider, account.id.0, KIMI_STORE_FIELD);
            map.get(&key)
                .cloned()
                .map(|blob| (account.id.clone(), blob))
        })
        .collect()
}

/// Kimi 工作目录解析：`KIMI_CODE_HOME` > `~/.kimi-code` > `.kimi-code`。
/// 只读镜像 `crates/providers/kimi/src/paths.rs::kimi_home`（owner 在那边）；同进程内
/// env 与用户目录一致，解析结果按构造相同。
fn kimi_home() -> PathBuf {
    if let Ok(value) = std::env::var("KIMI_CODE_HOME") {
        return PathBuf::from(value);
    }
    if let Some(dirs) = directories::UserDirs::new() {
        return dirs.home_dir().join(".kimi-code");
    }
    PathBuf::from(".kimi-code")
}

/// 拒止指纹文件：`<kimi_home>/credentials/.kimi-switch-dead-refresh`
/// （只读镜像 `crates/providers/kimi/src/paths.rs::dead_refresh_fingerprint_path`）。
fn dead_refresh_fingerprint_path(home: &Path) -> PathBuf {
    home.join("credentials").join(".kimi-switch-dead-refresh")
}

fn blob_string_field(blob: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(blob)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
pub(crate) mod test_env {
    //! 跨测试模块共用的夹具：KIMI_CODE_HOME 隔离（同一把锁，避免并发 set_var 竞态，
    //! 模式与 `swap.rs` 的 KIMI_SWITCH_HOME 夹具一致）与测试 JWT/blob 构造。

    use std::path::Path;
    use std::sync::Mutex;

    static KIMI_HOME_LOCK: Mutex<()> = Mutex::new(());

    /// 把 `KIMI_CODE_HOME` 临时指到独立 tempdir 执行 `f`，结束后恢复原值。
    pub(crate) fn with_isolated_kimi_home<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = KIMI_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("KIMI_CODE_HOME");
        std::env::set_var("KIMI_CODE_HOME", tmp.path());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.0 {
                    Some(value) => std::env::set_var("KIMI_CODE_HOME", value),
                    None => std::env::remove_var("KIMI_CODE_HOME"),
                }
            }
        }
        let _restore = Restore(previous);
        f(tmp.path())
    }

    /// 测试用 base64url 编码（无 padding）。
    pub(crate) fn b64url(input: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in input.as_bytes().chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
            let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 63) as usize] as char);
            }
        }
        out
    }

    /// 构造测试 JWT：payload 只含 exp（Unix 秒）。token 值不进断言文本。
    /// 当前口径不消费 exp（见模块级裁定），exp 用于构造「access 已过期但非死卡」的
    /// 反例夹具，钉住裁定边界。
    pub(crate) fn jwt_with_exp(exp: i64) -> String {
        let payload = format!(r#"{{"exp":{}}}"#, exp);
        format!("h.{}.s", b64url(&payload))
    }

    /// 构造测试凭证 blob：access_token 为给定 exp 的 JWT，refresh_token 为给定值。
    pub(crate) fn blob_with_exp(access_exp: i64, refresh_token: &str) -> String {
        format!(
            r#"{{"access_token":"{}","refresh_token":"{}"}}"#,
            jwt_with_exp(access_exp),
            refresh_token
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_env::{b64url, blob_with_exp, jwt_with_exp, with_isolated_kimi_home};
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 3, 12, 0, 0).unwrap()
    }

    fn dead_set(tokens: &[&str]) -> HashSet<String> {
        tokens.iter().map(|t| refresh_fingerprint(t)).collect()
    }

    #[test]
    fn fingerprint_hit_means_invalid() {
        let blob = blob_with_exp(now().timestamp() + 3600, "refresh-a");
        assert!(is_locally_invalid(&blob, &dead_set(&["refresh-a"])));
    }

    /// 裁定核心反例（#158 续跑）：access token 已过 exp 但无指纹命中 → 不失效。
    /// exp 不再是死信号（停泊号轮询停摆时自然过期；激活后下次查询自愈）。
    #[test]
    fn expired_access_without_fingerprint_is_not_invalid() {
        let expired = blob_with_exp(now().timestamp() - 1, "refresh-a");
        assert!(!is_locally_invalid(&expired, &HashSet::new()));
        assert!(!is_locally_invalid(&expired, &dead_set(&["other"])));
        // 恰好等于 now（旧口径的 now >= exp 边界）同样不算。
        let at_boundary = blob_with_exp(now().timestamp(), "refresh-a");
        assert!(!is_locally_invalid(&at_boundary, &HashSet::new()));
    }

    #[test]
    fn fingerprint_hit_stands_without_provable_access_token() {
        // 信号只看 refresh 指纹：access token 不可解析不影响指纹命中。
        let blob = r#"{"access_token":"no-dots","refresh_token":"refresh-a"}"#.to_string();
        assert!(is_locally_invalid(&blob, &dead_set(&["refresh-a"])));
    }

    #[test]
    fn unprovable_blob_is_not_invalid() {
        // payload 是合法 base64url JSON 但没有 exp claim（对当前口径与有无 exp 无关，
        // 一并钉住「无法解析 access 不影响判定」）。
        let no_exp_jwt = format!("h.{}.s", b64url(r#"{"sub":"u"}"#));
        let cases = [
            "not json".to_string(),
            r#"{"refresh_token":"refresh-a"}"#.to_string(),
            r#"{"access_token":"no-dots","refresh_token":"refresh-a"}"#.to_string(),
            format!(r#"{{"access_token":"{no_exp_jwt}","refresh_token":"refresh-a"}}"#),
        ];
        for blob in &cases {
            // 拒止集合不含该 refresh token：指纹未命中 → 不失效。
            assert!(
                !is_locally_invalid(blob, &dead_set(&["other"])),
                "无法确证不得判失效"
            );
        }
    }

    #[test]
    fn missing_refresh_token_is_not_invalid() {
        let blob = format!(
            r#"{{"access_token":"{}"}}"#,
            jwt_with_exp(now().timestamp() - 1)
        );
        // 无 refresh token → 无指纹可查；即使 access 已过期也不失效（裁定口径）。
        assert!(!is_locally_invalid(&blob, &dead_set(&["refresh-a"])));
    }

    #[test]
    fn refresh_fingerprint_matches_sha256_vectors() {
        // 与 oauth::refresh_fingerprint 逐字节一致（小写 hex、无前缀）。
        assert_eq!(
            refresh_fingerprint(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            refresh_fingerprint("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn load_dead_fingerprints_parses_lines_and_legacy_single() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead");
        std::fs::write(&path, "aa\n bb \n\ncc\n").unwrap();
        let loaded = load_dead_refresh_fingerprints(&path);
        assert_eq!(
            loaded,
            HashSet::from(["aa".to_string(), "bb".to_string(), "cc".to_string()])
        );

        // 旧版整文件单哈希：单行读取同样算成员。
        let legacy = dir.path().join("legacy-dead");
        std::fs::write(&legacy, "aa").unwrap();
        assert_eq!(
            load_dead_refresh_fingerprints(&legacy),
            HashSet::from(["aa".to_string()])
        );

        // 缺失文件 → 空集合。
        assert!(load_dead_refresh_fingerprints(&dir.path().join("nope")).is_empty());
    }

    fn acc(id: &str, provider: &str, active: bool) -> Account {
        Account {
            provider: provider.into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: now(),
            last_used_at: Some(now() - chrono::Duration::seconds(120)),
            extra: Default::default(),
        }
    }

    /// 布置 `<tmp>/data/{state/auto_swap.json, credentials.json}`，返回 state 文件路径。
    fn seed_data_dir(dir: &Path, entries: &[(String, String)]) -> PathBuf {
        let data = dir.join("data");
        std::fs::create_dir_all(data.join("state")).unwrap();
        let map: serde_json::Map<String, serde_json::Value> = entries
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        std::fs::write(
            data.join("credentials.json"),
            serde_json::to_string(&serde_json::Value::Object(map)).unwrap(),
        )
        .unwrap();
        data.join("state").join("auto_swap.json")
    }

    #[test]
    fn locally_invalid_accounts_fingerprint_hit_via_dead_file() {
        with_isolated_kimi_home(|home| {
            let dir = tempfile::tempdir().unwrap();
            // bob 的 refresh 指纹在拒止文件中 → 失效；carol 干净。
            let state_path = seed_data_dir(
                dir.path(),
                &[
                    (
                        format!("kimi:bob:{KIMI_STORE_FIELD}"),
                        blob_with_exp(now().timestamp() + 3600, "refresh-bob"),
                    ),
                    (
                        format!("kimi:carol:{KIMI_STORE_FIELD}"),
                        blob_with_exp(now().timestamp() + 3600, "refresh-carol"),
                    ),
                ],
            );
            std::fs::create_dir_all(home.join("credentials")).unwrap();
            std::fs::write(
                dead_refresh_fingerprint_path(home),
                format!("{}\n", refresh_fingerprint("refresh-bob")),
            )
            .unwrap();

            let accounts = vec![acc("bob", "kimi", false), acc("carol", "kimi", false)];
            let invalid = locally_invalid_accounts(&state_path, &accounts, now());
            assert_eq!(invalid.len(), 1);
            assert!(invalid.contains(&AccountId("bob".into())));
        });
    }

    /// 裁定核心反例在收集层的钉：access 全部已过 exp、无拒止文件 → 空集合。
    #[test]
    fn locally_invalid_accounts_ignores_expired_access_without_fingerprint() {
        with_isolated_kimi_home(|_home| {
            let dir = tempfile::tempdir().unwrap();
            let state_path = seed_data_dir(
                dir.path(),
                &[
                    (
                        format!("kimi:bob:{KIMI_STORE_FIELD}"),
                        blob_with_exp(now().timestamp() - 1, "refresh-bob"),
                    ),
                    (
                        format!("kimi:carol:{KIMI_STORE_FIELD}"),
                        blob_with_exp(now().timestamp() - 3600, "refresh-carol"),
                    ),
                ],
            );
            let accounts = vec![acc("bob", "kimi", false), acc("carol", "kimi", false)];
            assert!(locally_invalid_accounts(&state_path, &accounts, now()).is_empty());
        });
    }

    #[test]
    fn locally_invalid_accounts_fail_open_on_missing_or_foreign() {
        with_isolated_kimi_home(|_home| {
            let dir = tempfile::tempdir().unwrap();
            // credentials.json 缺失 → 空集合。
            let state_path = dir.path().join("data").join("state").join("auto_swap.json");
            let accounts = vec![acc("bob", "kimi", false)];
            assert!(locally_invalid_accounts(&state_path, &accounts, now()).is_empty());

            // 非 Kimi provider 不参与判定（字段布局未知 → 信号未知）。
            let other = acc("zoe", "beta", false);
            let state_path = seed_data_dir(
                dir.path(),
                &[(
                    format!("beta:zoe:{KIMI_STORE_FIELD}"),
                    blob_with_exp(now().timestamp() - 1, "r"),
                )],
            );
            assert!(locally_invalid_accounts(&state_path, &[other], now()).is_empty());

            // 损坏的 credentials.json → 空集合。
            let data = dir.path().join("data2");
            std::fs::create_dir_all(data.join("state")).unwrap();
            std::fs::write(data.join("credentials.json"), "not-json").unwrap();
            let corrupt_state = data.join("state").join("auto_swap.json");
            assert!(locally_invalid_accounts(&corrupt_state, &accounts, now()).is_empty());
        });
    }
}
