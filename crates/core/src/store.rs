//! 凭证仓库抽象。Token、refresh_token 等敏感字段由 [`CredentialStore`] 持有，
//! 元数据（label、过期时间、优先级等）走 [`crate::paths`] 下的明文 toml/json。
//!
//! 后端：[`KeyringStore`]（系统钥匙串）或 [`FileStore`]（data 目录下 `0600` 明文文件）。
//! macOS 上钥匙串读写会弹授权框，默认装配用 [`FileStore`] 规避（见 cli/daemon 装配处）。
//!
//! 命名约定：
//! - service: `kimi-switch`
//! - key:     `{provider_id}:{account_id}:{field}`，例如 `alpha:alice@example.com:access_token`

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use fs2::FileExt;

use crate::error::{Error, Result};

/// 抽象凭证仓库。实现：[`KeyringStore`]（系统钥匙串）、[`FileStore`]（明文文件）。
pub trait CredentialStore: Send + Sync {
    /// 保存一个键值对。
    fn set(&self, provider: &str, account: &str, field: &str, value: &str) -> Result<()>;

    /// 读取键值。不存在时返回 `Ok(None)`，仅 IO/平台错误时返回 `Err`。
    fn get(&self, provider: &str, account: &str, field: &str) -> Result<Option<String>>;

    /// 删除一个键值。不存在视为成功（幂等）。
    fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()>;
}

const SERVICE: &str = "kimi-switch";

fn compose_key(provider: &str, account: &str, field: &str) -> String {
    format!("{provider}:{account}:{field}")
}

/// 基于系统 keyring 的实现。
/// - macOS: Keychain
/// - Windows: Credential Manager
/// - Linux: secret-service / kernel keyutils
#[derive(Default, Clone)]
pub struct KeyringStore;

impl KeyringStore {
    pub fn new() -> Self {
        Self
    }
}

impl CredentialStore for KeyringStore {
    fn set(&self, provider: &str, account: &str, field: &str, value: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, &compose_key(provider, account, field))
            .map_err(|e| Error::Credential(e.to_string()))?;
        entry
            .set_password(value)
            .map_err(|e| Error::Credential(e.to_string()))
    }

    fn get(&self, provider: &str, account: &str, field: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(SERVICE, &compose_key(provider, account, field))
            .map_err(|e| Error::Credential(e.to_string()))?;
        match entry.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Credential(e.to_string())),
        }
    }

    fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, &compose_key(provider, account, field))
            .map_err(|e| Error::Credential(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::Credential(e.to_string())),
        }
    }
}

/// 基于明文 JSON 文件的实现。
///
/// - 单文件存储：`{ "provider:account:field": "value", ... }`。有值就是 string，删除就是 key 不在 map 里。
/// - Unix 下文件权限收紧为 `0600`，仅当前用户可读。
/// - 可挂一个 `legacy` keyring 回退：文件里查不到某项时，从旧钥匙串读出并写回文件，
///   实现 Keychain → 明文文件的「按需、一次性」迁移；迁移后该项永不再碰钥匙串。
/// - 迁移写回失败只记警告，本次 `get` 仍返回 legacy 命中值，不伪装成查询错误。
/// - 挂了 legacy 时，已删除 key 记在旁路 sibling `<path>.deleted`（JSON 字符串集合），
///   不写进 `credentials.json`。`get` 文件未命中且 key 在该集合中则 `Ok(None)`，不碰 legacy。
///
/// 并发：cli 与 daemon 可能同时读写，读写都经独立锁文件 `<path>.lock` 做 fs2 建议锁，
/// 写入走临时文件 + rename 保证原子性。
pub struct FileStore {
    path: PathBuf,
    legacy: Option<Box<dyn CredentialStore>>,
    #[cfg(test)]
    inject_write_error: AtomicBool,
}

impl FileStore {
    fn assemble(path: PathBuf, legacy: Option<Box<dyn CredentialStore>>) -> Self {
        Self {
            path,
            legacy,
            #[cfg(test)]
            inject_write_error: AtomicBool::new(false),
        }
    }

    /// 纯文件实现，不带迁移回退。
    pub fn new(path: PathBuf) -> Self {
        Self::assemble(path, None)
    }

    /// 带旧 keyring 回退：文件未命中时从钥匙串取出并落盘迁移。
    pub fn with_legacy_keyring(path: PathBuf, legacy: KeyringStore) -> Self {
        Self::assemble(path, Some(Box::new(legacy)))
    }

    /// 测试替身：注入可控的 legacy 后端（可失败的 get/delete）。
    #[cfg(test)]
    fn with_legacy_store(path: PathBuf, legacy: impl CredentialStore + 'static) -> Self {
        Self::assemble(path, Some(Box::new(legacy)))
    }

    fn sibling(&self, suffix: &str) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(suffix);
        PathBuf::from(p)
    }

    /// 打开（必要时创建）锁文件。
    fn open_lock(&self) -> Result<fs::File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::Credential(format!("create credentials dir failed: {e}")))?;
        }
        fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.sibling(".lock"))
            .map_err(|e| Error::Credential(format!("open credentials lock failed: {e}")))
    }

    fn read_map(&self) -> Result<BTreeMap<String, String>> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(Error::Credential(format!("read credentials failed: {e}"))),
        };
        if raw.trim().is_empty() {
            return Ok(BTreeMap::new());
        }
        serde_json::from_str(&raw)
            .map_err(|e| Error::Credential(format!("parse credentials failed: {e}")))
    }

    fn write_map(&self, map: &BTreeMap<String, String>) -> Result<()> {
        let raw = serde_json::to_string_pretty(map)
            .map_err(|e| Error::Credential(format!("serialize credentials failed: {e}")))?;
        self.write_atomic(&self.path, raw.as_bytes(), "credentials")
    }

    fn deleted_path(&self) -> PathBuf {
        self.sibling(".deleted")
    }

    fn read_deleted_keys(&self) -> Result<BTreeSet<String>> {
        let raw = match fs::read_to_string(self.deleted_path()) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(e) => {
                return Err(Error::Credential(format!(
                    "read deleted-key sidecar failed: {e}"
                )))
            }
        };
        if raw.trim().is_empty() {
            return Ok(BTreeSet::new());
        }
        match serde_json::from_str(&raw) {
            Ok(keys) => Ok(keys),
            Err(_) => {
                // 损坏 sidecar 自愈为合法空集合并原子写回（与墓碑损坏替换、quota_cache 坏 JSON 当空同类）。
                // 残余：空集合不再记得已删 key，legacy 可能再被看见——好过整仓凭证 IO 失败。
                let empty = BTreeSet::new();
                self.write_deleted_keys(&empty)?;
                Ok(empty)
            }
        }
    }

    fn write_deleted_keys(&self, keys: &BTreeSet<String>) -> Result<()> {
        let raw = serde_json::to_string_pretty(keys)
            .map_err(|e| Error::Credential(format!("serialize deleted-key sidecar failed: {e}")))?;
        self.write_atomic(&self.deleted_path(), raw.as_bytes(), "deleted-key sidecar")
    }

    fn write_atomic(&self, path: &Path, raw: &[u8], what: &str) -> Result<()> {
        let tmp = {
            let mut p = path.as_os_str().to_os_string();
            p.push(format!(".tmp.{}", std::process::id()));
            PathBuf::from(p)
        };
        fs::write(&tmp, raw).map_err(|e| Error::Credential(format!("write {what} failed: {e}")))?;
        restrict_permissions(&tmp)?;
        fs::rename(&tmp, path)
            .map_err(|e| Error::Credential(format!("commit {what} failed: {e}")))?;
        // 文件级 fsync（issue #153）：capture-on-leave 回灌等凭证写入在临时文件 +
        // rename 之后落盘。capture 路径上的失败维持 warn 不阻断（spec #152 决策 10）。
        crate::swap::sync_file(path)
            .map_err(|e| Error::Credential(format!("sync {what} failed: {e}")))
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::Credential(format!("chmod credentials failed: {e}")))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

impl CredentialStore for FileStore {
    fn set(&self, provider: &str, account: &str, field: &str, value: &str) -> Result<()> {
        #[cfg(test)]
        if self.inject_write_error.load(Ordering::SeqCst) {
            return Err(Error::Credential("injected write failure".into()));
        }
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock)
            .map_err(|e| Error::Credential(format!("lock credentials failed: {e}")))?;
        let result = (|| {
            let key = compose_key(provider, account, field);
            // 先读 sidecar：损坏时自愈，避免 set 在写完凭证后因旁路解析失败而整体报错。
            let mut deleted = self.read_deleted_keys()?;
            let mut map = self.read_map()?;
            map.insert(key.clone(), value.to_string());
            self.write_map(&map)?;
            // 重新导入/迁移成功后清掉旁路，允许该 key 再次作为有效凭证。
            if self.legacy.is_some() && deleted.remove(&key) {
                self.write_deleted_keys(&deleted)?;
            }
            Ok(())
        })();
        let _ = FileExt::unlock(&lock);
        result
    }

    fn get(&self, provider: &str, account: &str, field: &str) -> Result<Option<String>> {
        let key = compose_key(provider, account, field);
        // 先查文件（共享锁，读完即释放）。旁路与 map 同锁，避免「文件已删、旁路未读」窗口。
        let lookup = {
            let lock = self.open_lock()?;
            FileExt::lock_shared(&lock)
                .map_err(|e| Error::Credential(format!("lock credentials failed: {e}")))?;
            let looked: Result<Option<Option<String>>> = (|| {
                let map = self.read_map()?;
                if let Some(v) = map.get(&key).cloned() {
                    return Ok(Some(Some(v)));
                }
                // 未命中才读 sidecar：损坏时自愈，避免 get 整仓失败。
                let deleted = self.read_deleted_keys()?;
                if self.legacy.is_some() && deleted.contains(&key) {
                    return Ok(Some(None));
                }
                Ok(None)
            })();
            let _ = FileExt::unlock(&lock);
            looked?
        };
        match lookup {
            Some(Some(v)) => return Ok(Some(v)),
            // 旁路命中：已从本仓库删除，禁止再从 legacy 迁回。
            Some(None) => return Ok(None),
            None => {}
        }
        // 文件未命中 → 旧 keyring 回退（best-effort 迁移）；命中即落盘，下次不再读钥匙串。
        // 读失败（钥匙串报错 / 用户拒绝授权）一律当作「没有」，不让迁移源的故障污染查询。
        // 写回文件失败同样不伪装成查询错误：降级为警告，本次仍返回 legacy 命中的值。
        if let Some(legacy) = &self.legacy {
            match legacy.get(provider, account, field) {
                Ok(Some(v)) => {
                    if let Err(e) = self.set(provider, account, field, &v) {
                        tracing::warn!(
                            err = %e,
                            "legacy credential migrate-to-file failed; returning in-memory value"
                        );
                    }
                    return Ok(Some(v));
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(err = %e, "legacy keyring fallback failed; treating as missing");
                }
            }
        }
        Ok(None)
    }

    fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()> {
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock)
            .map_err(|e| Error::Credential(format!("lock credentials failed: {e}")))?;
        let result = (|| {
            let key = compose_key(provider, account, field);
            let mut deleted = self.read_deleted_keys()?;
            // 旁路必须先落盘：失败则 delete 失败，凭证文件保持原样，避免复活窗口。
            if self.legacy.is_some() {
                deleted.insert(key.clone());
                self.write_deleted_keys(&deleted)?;
            }
            let mut map = self.read_map()?;
            map.remove(&key);
            self.write_map(&map)
        })();
        let _ = FileExt::unlock(&lock);
        // 尽力清钥匙串旧副本；失败只记警告，不能推翻已经成功的旁路记录。
        if let Some(legacy) = &self.legacy {
            if let Err(e) = legacy.delete(provider, account, field) {
                tracing::warn!(
                    err = %e,
                    "legacy keyring delete failed; deleted-key sidecar remains"
                );
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, FileStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("credentials.json"));
        (dir, store)
    }

    /// 旧版/当前解析器契约：有值就是 string，删除就是 key 不在 map 里，不得出现 JSON null。
    fn assert_credentials_json_is_string_map(path: &Path) {
        if !path.exists() {
            return;
        }
        let raw = fs::read_to_string(path).unwrap();
        assert!(
            !raw.contains("null"),
            "credentials.json 不得含 JSON null: {raw}"
        );
        let parsed: BTreeMap<String, String> = serde_json::from_str(&raw).unwrap_or_else(|e| {
            panic!("须能按 BTreeMap<String, String> 解析 credentials.json: {e}; {raw}")
        });
        let _ = parsed;
    }

    #[test]
    fn set_get_delete_roundtrip() {
        let (_dir, store) = temp_store();
        assert_eq!(
            store.get("alpha", "a@x.com", "credentials_json").unwrap(),
            None
        );

        store
            .set("alpha", "a@x.com", "credentials_json", "{\"t\":1}")
            .unwrap();
        assert_eq!(
            store.get("alpha", "a@x.com", "credentials_json").unwrap(),
            Some("{\"t\":1}".to_string())
        );

        // 覆盖写。
        store
            .set("alpha", "a@x.com", "credentials_json", "{\"t\":2}")
            .unwrap();
        assert_eq!(
            store.get("alpha", "a@x.com", "credentials_json").unwrap(),
            Some("{\"t\":2}".to_string())
        );

        store
            .delete("alpha", "a@x.com", "credentials_json")
            .unwrap();
        assert_eq!(
            store.get("alpha", "a@x.com", "credentials_json").unwrap(),
            None
        );
        // 删不存在的项幂等。
        store
            .delete("alpha", "a@x.com", "credentials_json")
            .unwrap();
    }

    #[test]
    fn keys_are_namespaced_by_provider_account_field() {
        let (_dir, store) = temp_store();
        store.set("alpha", "a@x.com", "f", "alpha-val").unwrap();
        store.set("beta", "a@x.com", "f", "beta-val").unwrap();
        store.set("alpha", "b@x.com", "f", "other-acct").unwrap();

        assert_eq!(
            store.get("alpha", "a@x.com", "f").unwrap(),
            Some("alpha-val".to_string())
        );
        assert_eq!(
            store.get("beta", "a@x.com", "f").unwrap(),
            Some("beta-val".to_string())
        );
        assert_eq!(
            store.get("alpha", "b@x.com", "f").unwrap(),
            Some("other-acct".to_string())
        );
    }

    /// issue #153：凭证文件原子写的文件级 fsync 失败必须如实报错（Credential 错误），
    /// 不得静默成功；capture-on-leave 路径上的该错误维持 warn 不阻断切换。
    #[test]
    fn set_fsync_failure_returns_credential_error() {
        let (_dir, store) = temp_store();
        store.set("alpha", "a@x.com", "f", "first").unwrap();

        crate::swap::fail_next_sync();
        let err = store.set("alpha", "a@x.com", "f", "second").unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("injected fsync failure"),
            "fsync 失败必须上抛: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn credentials_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = temp_store();
        store.set("alpha", "a@x.com", "f", "secret").unwrap();
        let mode = std::fs::metadata(&store.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
    }

    /// 内存 legacy：可预置值，delete 可按需失败且保留旧值（模拟钥匙串清理失败）。
    struct StickyLegacy {
        values: std::sync::Mutex<BTreeMap<String, String>>,
        fail_delete: bool,
    }

    impl StickyLegacy {
        fn seeded(provider: &str, account: &str, field: &str, value: &str) -> Self {
            let mut values = BTreeMap::new();
            values.insert(compose_key(provider, account, field), value.to_string());
            Self {
                values: std::sync::Mutex::new(values),
                fail_delete: false,
            }
        }
    }

    impl CredentialStore for StickyLegacy {
        fn set(&self, provider: &str, account: &str, field: &str, value: &str) -> Result<()> {
            self.values
                .lock()
                .unwrap()
                .insert(compose_key(provider, account, field), value.to_string());
            Ok(())
        }

        fn get(&self, provider: &str, account: &str, field: &str) -> Result<Option<String>> {
            Ok(self
                .values
                .lock()
                .unwrap()
                .get(&compose_key(provider, account, field))
                .cloned())
        }

        fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()> {
            if self.fail_delete {
                return Err(Error::Credential("legacy delete failed".into()));
            }
            self.values
                .lock()
                .unwrap()
                .remove(&compose_key(provider, account, field));
            Ok(())
        }
    }

    #[test]
    fn get_returns_legacy_value_when_migration_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let legacy = StickyLegacy::seeded("alpha", "a@x.com", "credentials_json", "from-legacy");
        let store = FileStore::with_legacy_store(path.clone(), legacy);
        store.inject_write_error.store(true, Ordering::SeqCst);

        let got = store.get("alpha", "a@x.com", "credentials_json");
        assert!(got.is_ok(), "迁移写失败不得冒泡为查询错误，得到 {got:?}");
        assert_eq!(got.unwrap(), Some("from-legacy".to_string()));
        // 钉死语义：本次仍返回 legacy 命中值；文件不得被写成该值。
        assert!(
            !path.exists()
                || !fs::read_to_string(&path)
                    .unwrap_or_default()
                    .contains("from-legacy"),
            "写失败时不得把 legacy 值落盘"
        );
    }

    #[test]
    fn delete_does_not_resurrect_legacy_value_when_legacy_delete_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let mut legacy =
            StickyLegacy::seeded("alpha", "a@x.com", "credentials_json", "from-legacy");
        legacy.fail_delete = true;
        let store = FileStore::with_legacy_store(path.clone(), legacy);

        assert_eq!(
            store.get("alpha", "a@x.com", "credentials_json").unwrap(),
            Some("from-legacy".to_string()),
            "删前应能从 legacy 迁到文件"
        );

        store
            .delete("alpha", "a@x.com", "credentials_json")
            .unwrap();

        assert_credentials_json_is_string_map(&path);
        if path.exists() {
            let map: BTreeMap<String, String> =
                serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert!(
                !map.contains_key("alpha:a@x.com:credentials_json"),
                "delete 后凭证文件里不得再有该 key"
            );
        }

        assert_eq!(
            store.get("alpha", "a@x.com", "credentials_json").unwrap(),
            None,
            "delete 成功后即使 legacy 仍能读到旧值也不得返回"
        );
        let raw = fs::read_to_string(&path).unwrap_or_default();
        assert!(
            !raw.contains("from-legacy"),
            "已删凭证不得从 legacy 迁回文件，文件内容: {raw}"
        );
        assert_credentials_json_is_string_map(&path);
    }

    /// 损坏的 `.deleted` sidecar 必须自愈，不能把 get/set/delete 全部变成 Credential 错误。
    #[test]
    fn corrupt_deleted_sidecar_heals_and_store_stays_usable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let sidecar = dir.path().join("credentials.json.deleted");
        fs::write(&sidecar, "{not-json").unwrap();

        let store = FileStore::new(path.clone());
        assert_eq!(
            store.get("alpha", "a@x.com", "credentials_json").unwrap(),
            None,
            "损坏 sidecar 不得让 get 失败"
        );
        store
            .set("alpha", "a@x.com", "credentials_json", "{\"t\":1}")
            .expect("损坏 sidecar 不得让 set 失败");
        assert_eq!(
            store.get("alpha", "a@x.com", "credentials_json").unwrap(),
            Some("{\"t\":1}".to_string())
        );
        store
            .delete("alpha", "a@x.com", "credentials_json")
            .expect("损坏 sidecar 不得让 delete 失败");
        assert_eq!(
            store.get("alpha", "a@x.com", "credentials_json").unwrap(),
            None
        );

        let raw = fs::read_to_string(&sidecar).unwrap();
        serde_json::from_str::<BTreeSet<String>>(&raw)
            .unwrap_or_else(|e| panic!("sidecar 必须写成合法 JSON 集合: {e}; raw={raw}"));
    }
}
