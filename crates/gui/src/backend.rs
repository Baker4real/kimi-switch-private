//! 账号库读写与额度查询。所有出口都是 DTO，不含凭证。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::{
    AccountId, AccountRegistry, AuditEvent, AuditLog, CredentialStore, FileStore, KeyringStore,
    Provider, Quota, QuotaCache, QuotaWindow, RemovedAccounts,
};
use kimi_switch_kimi::device_flow::{self, DeviceAuthorization};
use kimi_switch_kimi::KimiProvider;

use crate::dto::{
    AccountRow, AuthLink, QuotaView, Snapshot, ThemeId, UiMessage,
};

pub struct Backend {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    kimi: Arc<KimiProvider>,
    audit: AuditLog,
    /// 进行中的设备码授权；第三项是「按卡授权」的目标账号 id（None = 添加新账号）。
    pending_auth: Mutex<Option<(DeviceAuthorization, Arc<AtomicBool>, Option<AccountId>)>>,
}

impl Backend {
    pub fn new() -> Result<Self> {
        let paths = AppPaths::resolve()?;
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::with_legacy_keyring(
            paths.credentials_file(),
            KeyringStore::new(),
        ));
        let registry = Arc::new(AccountRegistry::from_default_paths()?);
        let kimi = Arc::new(kimi_switch_kimi::new(store.clone(), registry.clone()));
        let audit = AuditLog::from_default_paths()?;
        Ok(Self {
            store,
            registry,
            kimi,
            audit,
            pending_auth: Mutex::new(None),
        })
    }

    pub fn load_theme() -> ThemeId {
        AppPaths::resolve()
            .ok()
            .and_then(|p| std::fs::read_to_string(p.config_dir.join("gui-theme")).ok())
            .map(|s| ThemeId::parse(&s))
            .unwrap_or(ThemeId::Classic)
    }

    pub fn save_theme(theme: ThemeId) -> Result<()> {
        let paths = AppPaths::resolve()?;
        std::fs::write(paths.config_dir.join("gui-theme"), theme.as_str())
            .context("写入界面主题失败")?;
        Ok(())
    }

    fn load_removed() -> RemovedAccounts {
        AppPaths::resolve()
            .map(|p| RemovedAccounts::load(&p.removed_file()))
            .unwrap_or_else(|_| RemovedAccounts::load(std::path::Path::new("removed-missing.json")))
    }

    /// 扫本地 Kimi Code；当前激活账号没记录过就入库（已删除的有墓碑，跳过）。
    pub fn sync_local_active(&self) {
        let removed = Self::load_removed();
        let Ok(id) = self.kimi.live_account_id() else {
            return;
        };
        if removed.contains("kimi", &id.0) {
            return;
        }
        if let Ok(account) = self.kimi.sync_active_metadata(None) {
            let _ = self.registry.set_active("kimi", &account.id);
        }
    }

    pub async fn snapshot(&self, message: Option<UiMessage>) -> Snapshot {
        let accounts = self.load_rows().await;
        let urgent_count = accounts.iter().filter(|a| a.is_urgent()).count() as u32;
        let current_label = accounts
            .iter()
            .find(|a| a.active)
            .map(|a| a.label.clone())
            .unwrap_or_else(|| "--".to_string());
        Snapshot {
            account_count: accounts.len() as u32,
            urgent_count,
            current_label,
            accounts,
            theme: Self::load_theme(),
            message,
        }
    }

    async fn load_rows(&self) -> Vec<AccountRow> {
        let mut accounts = self.registry.list_by_provider("kimi").unwrap_or_default();
        accounts.sort_by_key(|a| !a.active);
        let mut rows = Vec::with_capacity(accounts.len());
        for account in accounts {
            let (membership, quotas, error) =
                match kimi_switch_core::query_quota_with_retry(self.kimi.as_ref(), &account.id)
                    .await
                {
                    Ok(quotas) => {
                        let membership = quotas
                            .iter()
                            .find_map(|q| q.note.as_deref())
                            .map(prettify_membership);
                        (membership, quota_views(&quotas), None)
                    }
                    Err(e) => (None, Vec::new(), Some(compact_error(&e.to_string()))),
                };
            rows.push(AccountRow {
                label: account.label.clone(),
                id: account.id.0,
                active: account.active,
                membership,
                quotas,
                error,
            });
        }
        rows
    }

    pub async fn import(&self) -> Result<Snapshot> {
        let account = self
            .kimi
            .import_active(None)
            .map_err(anyhow::Error::from)
            .context("导入失败：请先在 Kimi Code 里登录账号")?;
        self.registry.set_active("kimi", &account.id)?;
        if let Ok(mut removed) = AppPaths::resolve().map(|p| RemovedAccounts::load(&p.removed_file()))
        {
            let _ = removed.clear("kimi", account.id.0.as_str());
        }
        self.audit
            .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
        Ok(self
            .snapshot(Some(UiMessage {
                text: format!("已导入账号 {}", account.id),
                tone: "ok".into(),
            }))
            .await)
    }

    pub async fn swap(&self, id: &str) -> Result<Snapshot> {
        let id = AccountId(id.to_string());
        self.kimi
            .activate(&id)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("切换到 {id} 失败"))?;
        self.audit
            .append(AuditEvent::ok("activate", "kimi", Some(id.0.as_str())));
        Ok(self
            .snapshot(Some(UiMessage {
                text: format!("已切换到 {id}"),
                tone: "ok".into(),
            }))
            .await)
    }

    pub async fn remove(&self, id: &str) -> Result<Snapshot> {
        let id = AccountId(id.to_string());
        self.registry.remove("kimi", &id)?;
        if let Ok(mut removed) =
            AppPaths::resolve().map(|p| RemovedAccounts::load(&p.removed_file()))
        {
            removed.add("kimi", id.0.as_str())?;
        }
        if let Err(e) = self.store.delete("kimi", id.0.as_str(), "blob") {
            tracing::warn!(err=%e, "credential store delete failed (continuing)");
        }
        if let Ok(paths) = AppPaths::resolve() {
            let mut cache = QuotaCache::load(&paths.quota_cache_file());
            cache.remove("kimi", &id.0);
            cache.save(&paths.quota_cache_file());
        }
        self.audit
            .append(AuditEvent::ok("rm", "kimi", Some(id.0.as_str())));
        Ok(self
            .snapshot(Some(UiMessage {
                text: format!("已删除 {id}"),
                tone: "ok".into(),
            }))
            .await)
    }

    pub async fn rename(&self, id: &str, label: &str) -> Result<Snapshot> {
        let id = AccountId(id.to_string());
        let mut account = self
            .registry
            .find("kimi", &id)?
            .ok_or_else(|| anyhow::anyhow!("账号 {id} 不存在"))?;
        let label = label.trim();
        if label.is_empty() || label == account.label {
            return Ok(self
                .snapshot(Some(UiMessage {
                    text: "别名未变化".into(),
                    tone: "info".into(),
                }))
                .await);
        }
        account.label = label.to_string();
        self.registry.upsert(account)?;
        Ok(self
            .snapshot(Some(UiMessage {
                text: format!("已把 {id} 重命名为「{label}」"),
                tone: "ok".into(),
            }))
            .await)
    }

    /// 发起设备码授权。
    ///
    /// `target = None`：现行「添加新账号」流程，行为不变；
    /// `target = Some(id)`：给指定账号卡重新授权（按卡授权）。这里先验证该卡存在，
    /// 让「不存在」类错误在打开浏览器之前就报出来；身份是否对得上由完成阶段的引擎判定。
    pub async fn start_device_auth(&self, target: Option<String>) -> Result<AuthLink> {
        self.cancel_device_auth();
        let target = match target {
            None => None,
            Some(t) => {
                let id = AccountId(t);
                self.registry
                    .find("kimi", &id)?
                    .ok_or_else(|| anyhow::anyhow!("账号 {} 不存在，无法给这张卡授权", id))?;
                Some(id)
            }
        };
        let auth = device_flow::request_device_code()
            .await
            .map_err(anyhow::Error::from)
            .context("获取授权链接失败")?;
        let url = auth
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| auth.verification_uri.clone());
        let user_code = auth.user_code.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        *self.pending_auth.lock().expect("pending_auth lock") = Some((auth, cancel, target));
        Ok(AuthLink { url, user_code })
    }

    pub fn cancel_device_auth(&self) {
        if let Some((_, cancel, _)) = self
            .pending_auth
            .lock()
            .expect("pending_auth lock")
            .as_ref()
        {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    pub async fn complete_device_auth(&self) -> Result<Snapshot> {
        let pending = self.pending_auth.lock().expect("pending_auth lock").clone();
        let Some((auth, cancel, target)) = pending else {
            anyhow::bail!("没有进行中的授权");
        };
        let result = device_flow::poll_for_token(&auth, cancel)
            .await
            .map_err(anyhow::Error::from);
        *self.pending_auth.lock().expect("pending_auth lock") = None;
        let blob = result.context("授权未完成")?;
        let Some(target) = target else {
            // 现行添加新账号路径，行为保持不变。
            let account = self
                .kimi
                .import_raw(blob, None, Some(false))
                .map_err(anyhow::Error::from)
                .context("授权成功但入库失败")?;
            self.audit
                .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
            return Ok(self
                .snapshot(Some(UiMessage {
                    text: format!("授权成功，已添加账号 {}", account.id),
                    tone: "ok".into(),
                }))
                .await);
        };
        // 按卡重新授权：写库与否、官方身份是否对得上全部由引擎判定；
        // 这里只借用同一套解析规则，把失败翻译成面向用户的原因（不含任何凭证内容）。
        let preview = kimi_switch_kimi::parse_authorized_user_id(&blob);
        match self.kimi.replace_account_blob(&target, blob) {
            Ok(account) => {
                self.audit.append(AuditEvent::ok(
                    "reauth",
                    "kimi",
                    Some(account.id.0.as_str()),
                ));
                Ok(self
                    .snapshot(Some(UiMessage {
                        text: format!("已更新账号 {} 的登录凭证", account.id),
                        tone: "ok".into(),
                    }))
                    .await)
            }
            Err(e) => {
                self.audit.append(AuditEvent::err(
                    "reauth",
                    "kimi",
                    Some(target.0.as_str()),
                    &e.to_string(),
                ));
                let reason = match preview {
                    None => "无法从授权结果辨认官方账号，未更新这张卡".to_string(),
                    Some(observed) if observed != target.0 => {
                        format!("登录的是账号 {observed}，不是这张卡（{target}），已拒绝更新凭证")
                    }
                    // 身份对上却仍失败（如存储层 IO）：透传原始原因帮助排查。
                    Some(_) => format!("重新授权 {target} 失败：{e}"),
                };
                Err(anyhow::anyhow!(reason))
            }
        }
    }

    pub fn open_url(url: &str) -> Result<()> {
        std::process::Command::new("explorer")
            .arg(url)
            .spawn()
            .context("无法打开浏览器")?;
        Ok(())
    }
}

fn prettify_membership(level: &str) -> String {
    let stripped = level.strip_prefix("LEVEL_").unwrap_or(level);
    let mut chars = stripped.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => stripped.to_string(),
    }
}

fn quota_views(quotas: &[Quota]) -> Vec<QuotaView> {
    quotas
        .iter()
        .map(|q| {
            let window = match q.window {
                QuotaWindow::FiveHour => "5小时",
                QuotaWindow::SevenDay => "7天",
                QuotaWindow::Month => "30天",
                _ => "其他",
            }
            .to_string();
            let ratio = q.usage_ratio().map(|r| r as f32);
            let text = match ratio {
                Some(r) => format!("{:.0}% ({}/{})", r * 100.0, q.used, q.limit),
                None => format!("{}/{}", q.used, q.limit),
            };
            let reset = q.reset_at.map(|t| {
                format!(
                    "重置 {}",
                    t.with_timezone(&chrono::Local).format("%m-%d %H:%M")
                )
            });
            QuotaView {
                tone: QuotaView::tone(ratio).to_string(),
                window,
                ratio,
                text,
                reset,
            }
        })
        .collect()
}

fn compact_error(error: &str) -> String {
    let one_line = error.replace(['\n', '\r'], " ");
    const MAX: usize = 80;
    if one_line.chars().count() > MAX {
        let mut s: String = one_line.chars().take(MAX).collect();
        s.push('…');
        s
    } else {
        one_line
    }
}
