//! 用户显式删除过的账号墓碑。
//!
//! 默认入口会把各客户端当前登录账号自动 import 进 registry。若用户刚 `kimi-switch rm`
//! 掉的正好是那个正在登录的号，下次无参 `kimi-switch` 会把它当「没记录过」再导入回来，
//! 删除看起来没生效。墓碑让自动导入跳过这些 id，直到用户再跑一次 `kimi-switch login`。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Default, Serialize, Deserialize)]
struct RemovedFile {
    /// provider → 已删除的账号 id。
    #[serde(default)]
    accounts: HashMap<String, Vec<String>>,
}

/// 显式 `rm` 留下的账号墓碑，阻止默认入口把当前登录账号再自动加回来。
#[derive(Debug, Clone)]
pub struct RemovedAccounts {
    path: PathBuf,
    by_provider: HashMap<String, HashSet<String>>,
}

impl RemovedAccounts {
    /// 从文件加载；文件不存在或解析失败视为没有墓碑。
    pub fn load(path: &Path) -> Self {
        let by_provider = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<RemovedFile>(&raw).ok())
            .map(|file| {
                file.accounts
                    .into_iter()
                    .map(|(provider, ids)| (provider, ids.into_iter().collect()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            by_provider,
        }
    }

    /// 该账号是否被用户显式删除过、且尚未被 `login` 解除。
    pub fn contains(&self, provider: &str, id: &str) -> bool {
        self.by_provider
            .get(provider)
            .is_some_and(|ids| ids.contains(id))
    }

    /// 记下一次显式删除。已存在时幂等。
    pub fn add(&mut self, provider: &str, id: &str) -> Result<()> {
        self.by_provider
            .entry(provider.to_string())
            .or_default()
            .insert(id.to_string());
        self.save()
    }

    /// `kimi-switch login` 成功导入后解除墓碑，允许该账号重新出现在列表里。
    pub fn clear(&mut self, provider: &str, id: &str) -> Result<()> {
        let mut changed = false;
        if let Some(ids) = self.by_provider.get_mut(provider) {
            changed = ids.remove(id);
            if ids.is_empty() {
                self.by_provider.remove(provider);
            }
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = RemovedFile {
            accounts: self
                .by_provider
                .iter()
                .map(|(provider, ids)| {
                    let mut ids: Vec<String> = ids.iter().cloned().collect();
                    ids.sort();
                    (provider.clone(), ids)
                })
                .collect(),
        };
        let json = serde_json::to_string_pretty(&file)?;
        // tmp + rename：崩溃时要么仍是旧墓碑，要么已是完整新文件，避免半截 JSON。
        // 后缀带 pid，避免并发 save 互踩固定的 `removed.json.tmp`。
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, json.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_contains_and_clear_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("removed.json");
        let mut removed = RemovedAccounts::load(&path);
        assert!(!removed.contains("gamma", "auth0|user_a"));

        removed.add("gamma", "auth0|user_a").unwrap();
        assert!(removed.contains("gamma", "auth0|user_a"));
        assert!(!removed.contains("gamma", "auth0|user_b"));

        let reloaded = RemovedAccounts::load(&path);
        assert!(reloaded.contains("gamma", "auth0|user_a"));

        let mut removed = reloaded;
        removed.clear("gamma", "auth0|user_a").unwrap();
        assert!(!removed.contains("gamma", "auth0|user_a"));
        let reloaded = RemovedAccounts::load(&path);
        assert!(!reloaded.contains("gamma", "auth0|user_a"));
    }

    /// issue #76：墓碑落盘后必须是完整 JSON，不能留下半截文件。
    #[test]
    fn add_replaces_corrupt_file_with_complete_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("removed.json");
        std::fs::write(&path, "{not-json").unwrap();

        RemovedAccounts::load(&path).add("kimi", "alice").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("墓碑必须是完整 JSON");
        assert_eq!(parsed["accounts"]["kimi"], serde_json::json!(["alice"]));

        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("removed.json")],
            "成功写入后不得留下 tmp 残留"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tombstone_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("removed.json");
        RemovedAccounts::load(&path).add("kimi", "alice").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "tombstone file must be 0600");
    }

    /// tmp 不得固定为 `removed.json.tmp`：该路径被占用时 save 仍须成功。
    #[test]
    fn save_tmp_does_not_use_fixed_json_tmp_literal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("removed.json");
        let colliding = dir.path().join("removed.json.tmp");
        std::fs::create_dir(&colliding).unwrap();

        RemovedAccounts::load(&path).add("kimi", "alice").unwrap();

        assert!(
            RemovedAccounts::load(&path).contains("kimi", "alice"),
            "占用 removed.json.tmp 不得挡住墓碑落盘"
        );
        assert!(colliding.is_dir(), "不得把碰撞名目录改写成墓碑内容");
    }

    /// rename 失败时残留 tmp 必须带本进程 pid，避免与其它进程互踩。
    #[test]
    fn save_tmp_name_includes_process_id_when_rename_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("removed.json");
        std::fs::create_dir(&path).unwrap();

        let err = RemovedAccounts::load(&path)
            .add("kimi", "alice")
            .expect_err("目标是目录时 rename 应失败并留下 tmp");
        let _ = err;

        let pid = std::process::id().to_string();
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|name| name != "removed.json")
            .collect();
        assert!(
            leftovers
                .iter()
                .any(|name| name.contains("tmp") && name.contains(&pid)),
            "残留 tmp 名必须含 pid={pid}，实际={leftovers:?}"
        );
    }
}
