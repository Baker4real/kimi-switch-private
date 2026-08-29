//! 账号库读写与额度快照渲染。所有出口都是 DTO，不含凭证。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::{
    Account, AccountId, AccountRegistry, AuditEvent, AuditLog, CredentialStore, FileStore,
    KeyringStore, Provider, Quota, QuotaFailureKind, QuotaSnapshot, QuotaState, QuotaWindow,
    RemovedAccounts,
};
use kimi_switch_kimi::device_flow::{self, DeviceAuthorization};
use kimi_switch_kimi::KimiProvider;

use crate::dto::{AccountRow, AuthLink, QuotaView, Snapshot, ThemeId, UiMessage};

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

    /// 直读 config.toml 里的「当前账号主动续期」开关；文件缺失/损坏都回落默认关。
    pub fn load_proactive_renewal() -> bool {
        AppPaths::resolve()
            .and_then(|p| {
                Ok(kimi_switch_core::settings::load_from(&p.config_file())?
                    .proactive_renewal
                    .enabled)
            })
            .unwrap_or(false)
    }

    /// 持久化开关到 config.toml（保注释原子写），返回落盘后的生效值。
    pub fn save_proactive_renewal(enabled: bool) -> Result<bool> {
        let settings = kimi_switch_core::settings::set_proactive_renewal_enabled(enabled)
            .context("写入设置失败")?;
        Ok(settings.proactive_renewal.enabled)
    }

    /// 扫本地 Kimi Code；当前激活账号没记录过就入库（已删除的有墓碑，跳过）。
    pub fn sync_local_active(&self) {
        let Ok(paths) = AppPaths::resolve() else {
            return;
        };
        let removed_path = paths.removed_file();
        kimi_switch_core::sync_local_active(
            "kimi",
            &removed_path,
            self.registry.as_ref(),
            || self.kimi.live_account_id(),
            || self.kimi.sync_active_metadata(None),
        );
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
            proactive_renewal: Self::load_proactive_renewal(),
            message,
        }
    }

    async fn load_rows(&self) -> Vec<AccountRow> {
        let mut accounts = self.registry.list_by_provider("kimi").unwrap_or_default();
        accounts.sort_by_key(|a| !a.active);
        let cache_path = AppPaths::resolve()
            .map(|p| p.quota_cache_file())
            .unwrap_or_else(|_| std::path::PathBuf::from("quota-cache-missing.json"));
        collect_account_rows(self.kimi.clone(), &accounts, &cache_path).await
    }

    pub async fn import(&self) -> Result<Snapshot> {
        let account = self
            .kimi
            .import_active(None)
            .map_err(anyhow::Error::from)
            .context("导入失败：请先在 Kimi Code 里登录账号")?;
        self.registry.set_active("kimi", &account.id)?;
        if let Ok(mut removed) =
            AppPaths::resolve().map(|p| RemovedAccounts::load(&p.removed_file()))
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
        let paths = AppPaths::resolve()?;
        kimi_switch_core::remove_account(
            self.registry.as_ref(),
            self.store.as_ref(),
            self.kimi.store_field(),
            &paths.removed_file(),
            &paths.quota_cache_file(),
            &self.audit,
            "kimi",
            &id,
        )?;
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
            return Err(kimi_switch_core::Error::no_pending_auth().into());
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
        // GUI 只把 ReauthError 映成文案，不重解析凭证。
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
                Err(anyhow::Error::from(e))
            }
        }
    }

    pub fn open_url(url: &str) -> Result<()> {
        // explorer.exe 会把 URL 参数误判成文件夹路径，弹出文件资源管理器而不是浏览器；
        // rundll32 url.dll,FileProtocolHandler 走系统默认浏览器关联，行为可靠。
        // 用 status() 等退出码而不是 spawn() 立刻返回，失败才能真正报到前端。
        let status = std::process::Command::new("rundll32")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .status()
            .context("无法打开浏览器")?;
        anyhow::ensure!(status.success(), "无法打开浏览器");
        Ok(())
    }
}

/// 列出与刷新共用：走 core 额度快照（节流 / 退避 / 并发），GUI 只做展示映射。
pub(crate) async fn collect_account_rows<P>(
    provider: Arc<P>,
    accounts: &[Account],
    cache_path: &std::path::Path,
) -> Vec<AccountRow>
where
    P: Provider + 'static,
{
    let ids: Vec<AccountId> = accounts.iter().map(|account| account.id.clone()).collect();
    let snapshots =
        kimi_switch_core::quota_snapshot::collect(provider, "kimi", &ids, cache_path).await;
    accounts
        .iter()
        .zip(snapshots.iter())
        .map(|(account, snapshot)| account_row(account, snapshot))
        .collect()
}

pub(crate) fn account_row(account: &Account, snapshot: &QuotaSnapshot) -> AccountRow {
    let membership = snapshot.membership.as_deref().map(prettify_membership);
    let (quotas, error, credential_expired) = match &snapshot.state {
        QuotaState::Ready(quotas) | QuotaState::Cached(quotas) => (quota_views(quotas), None, false),
        QuotaState::Failed(failure) => (
            Vec::new(),
            Some(compact_error(&failure.message)),
            failure.kind == QuotaFailureKind::CredentialExpired,
        ),
    };
    AccountRow {
        id: account.id.0.clone(),
        label: account.label.clone(),
        active: account.active,
        membership,
        quotas,
        error,
        credential_expired,
        cached_at: snapshot
            .stale
            .then(|| format_cached_at(snapshot.fetched_at)),
    }
}

fn format_cached_at(fetched_at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "缓存于 {}",
        fetched_at
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
    )
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
                tone: QuotaView::tone(ratio),
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
