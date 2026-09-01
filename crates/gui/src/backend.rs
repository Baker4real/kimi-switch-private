//! 账号库读写与额度快照渲染。所有出口都是 DTO，不含凭证。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::{
    display_name, membership_expiry_from_extra, membership_period_days,
    membership_remaining_days, membership_start_from_extra, official_nickname_known,
    prettify_membership, Account, AccountId, AccountRegistry, AuditEvent, AuditLog,
    CredentialStore, FileStore, KeyringStore, Provider, Quota, QuotaCollectReason,
    QuotaCollectStats, QuotaFailureKind, QuotaSnapshot, QuotaState, QuotaWindow, RemovedAccounts,
    WEB_STORE_FIELD,
};
use kimi_switch_kimi::device_flow::{self, DeviceAuthorization};
use kimi_switch_kimi::web_membership::{self, WebBinding};
use kimi_switch_kimi::KimiProvider;

use crate::dto::{AccountRow, AuthLink, QuotaView, Snapshot, ThemeId, UiMessage};

pub struct Backend {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    kimi: Arc<KimiProvider>,
    audit: AuditLog,
    /// 会员到期「仅刷新」的生产替身（issue #52）：只在 UserInitiated 行列组装时被调用。
    web_membership: Arc<LiveWebMembershipRefresh>,
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
        let web_membership = Arc::new(LiveWebMembershipRefresh {
            registry: registry.clone(),
            store: store.clone(),
        });
        Ok(Self {
            store,
            registry,
            kimi,
            audit,
            web_membership,
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

    /// 导入/授权后顺带打 `/me` 缓存官方昵称；失败忽略，不挡主流程。
    async fn cache_official_nickname(&self, account: &Account) {
        let Ok(Some(blob)) = self
            .store
            .get("kimi", account.id.0.as_str(), self.kimi.store_field())
        else {
            return;
        };
        kimi_switch_kimi::cache_official_nickname_from_blob(
            self.registry.as_ref(),
            &account.id,
            &blob,
        )
        .await;
    }

    pub async fn snapshot(
        &self,
        message: Option<UiMessage>,
        reason: QuotaCollectReason,
    ) -> Snapshot {
        let (accounts, stats) = self.load_rows(reason).await;
        let urgent_count = accounts.iter().filter(|a| a.is_urgent()).count() as u32;
        let current_label = accounts
            .iter()
            .find(|a| a.active)
            .map(|a| a.display_name.clone())
            .unwrap_or_else(|| "--".to_string());
        Snapshot {
            account_count: accounts.len() as u32,
            urgent_count,
            current_label,
            accounts,
            theme: Self::load_theme(),
            proactive_renewal: Self::load_proactive_renewal(),
            message: attach_skip_message(message, reason, &stats),
        }
    }

    async fn load_rows(&self, reason: QuotaCollectReason) -> (Vec<AccountRow>, QuotaCollectStats) {
        let mut accounts = self.registry.list_by_provider("kimi").unwrap_or_default();
        accounts.sort_by_key(|a| !a.active);
        let cache_path = AppPaths::resolve()
            .map(|p| p.quota_cache_file())
            .unwrap_or_else(|_| std::path::PathBuf::from("quota-cache-missing.json"));
        collect_account_rows(
            self.kimi.clone(),
            Some(self.web_membership.clone()),
            &accounts,
            &cache_path,
            reason,
        )
        .await
    }

    pub async fn import(&self) -> Result<Snapshot> {
        let account = self
            .kimi
            .import_active(None)
            .map_err(anyhow::Error::from)
            .context("导入失败：请先在 Kimi Code 里登录账号")?;
        self.registry.set_active("kimi", &account.id)?;
        self.cache_official_nickname(&account).await;
        let account = account_for_notice(self.registry.as_ref(), account);
        if let Ok(mut removed) =
            AppPaths::resolve().map(|p| RemovedAccounts::load(&p.removed_file()))
        {
            let _ = removed.clear("kimi", account.id.0.as_str());
        }
        self.audit
            .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
        Ok(self
            .snapshot(
                Some(UiMessage {
                    text: import_success_message(&account),
                    tone: "ok".into(),
                }),
                QuotaCollectReason::Scheduled,
            )
            .await)
    }

    pub async fn swap(&self, id: &str) -> Result<Snapshot> {
        let id = AccountId(id.to_string());
        let account = self
            .registry
            .find("kimi", &id)?
            .ok_or_else(|| anyhow::anyhow!("账号 {id} 不存在"))?;
        self.kimi
            .activate(&id)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| swap_failure_message(&account))?;
        self.audit
            .append(AuditEvent::ok("activate", "kimi", Some(id.0.as_str())));
        let plan = AppPaths::resolve().ok().and_then(|paths| {
            let cache = kimi_switch_core::QuotaCache::load(&paths.quota_cache_file());
            cached_membership_plan(&cache, &id.0)
        });
        Ok(self
            .snapshot(
                Some(UiMessage {
                    text: swap_success_message(&account, plan.as_deref()),
                    tone: "ok".into(),
                }),
                QuotaCollectReason::Scheduled,
            )
            .await)
    }

    pub async fn remove(&self, id: &str) -> Result<Snapshot> {
        let id = AccountId(id.to_string());
        let account = self
            .registry
            .find("kimi", &id)?
            .ok_or_else(|| anyhow::anyhow!("账号 {id} 不存在"))?;
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
            .snapshot(
                Some(UiMessage {
                    text: delete_success_message(&account),
                    tone: "ok".into(),
                }),
                QuotaCollectReason::Scheduled,
            )
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
                .snapshot(
                    Some(UiMessage {
                        text: "别名未变化".into(),
                        tone: "info".into(),
                    }),
                    QuotaCollectReason::Scheduled,
                )
                .await);
        }
        let text = rename_success_message(&account, label);
        account.label = label.to_string();
        self.registry.upsert(account)?;
        Ok(self
            .snapshot(
                Some(UiMessage {
                    text,
                    tone: "ok".into(),
                }),
                QuotaCollectReason::Scheduled,
            )
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
            self.cache_official_nickname(&account).await;
            let account = account_for_notice(self.registry.as_ref(), account);
            self.audit
                .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
            return Ok(self
                .snapshot(
                    Some(UiMessage {
                        text: auth_success_message(&account),
                        tone: "ok".into(),
                    }),
                    QuotaCollectReason::Scheduled,
                )
                .await);
        };
        // 按卡重新授权：写库与否、官方身份是否对得上全部由引擎判定；
        // GUI 只把 ReauthError 映成文案，不重解析凭证。
        match self.kimi.replace_account_blob(&target, blob) {
            Ok(account) => {
                self.cache_official_nickname(&account).await;
                let account = account_for_notice(self.registry.as_ref(), account);
                self.audit.append(AuditEvent::ok(
                    "reauth",
                    "kimi",
                    Some(account.id.0.as_str()),
                ));
                Ok(self
                    .snapshot(
                        Some(UiMessage {
                            text: reauth_success_message(&account),
                            tone: "ok".into(),
                        }),
                        QuotaCollectReason::Scheduled,
                    )
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

    /// 给账号绑定网页登录（issue #51）：token 来自 GUI 绑定对话框粘贴，只交给
    /// #50 的 kimi 管道（域名固定 `https://www.kimi.com`），不落日志、不进 Snapshot。
    /// 成功后重取 Snapshot（Scheduled，#45 链约定）：该卡 `membership_expiry` 出现、
    /// 绑定弱提示消失。失败映射为简洁中文（[`bind_web_user_message`]），不是额度
    /// 错误：不写账号行 `error`、不置 `credential_expired`、不动 `urgent_count`，
    /// 只经错误通道（前端 toast）表达，不影响切换。
    pub async fn bind_web(&self, id: &str, web_token: &str) -> Result<Snapshot> {
        let target = AccountId(id.to_string());
        let outcome = web_membership::bind_web(
            self.registry.as_ref(),
            self.store.as_ref(),
            &target,
            web_token,
        )
        .await;
        match outcome {
            Ok(binding) => {
                self.audit
                    .append(AuditEvent::ok("bind-web", "kimi", Some(target.0.as_str())));
                let message = bind_web_success_message(&binding);
                Ok(self
                    .snapshot(Some(message), QuotaCollectReason::Scheduled)
                    .await)
            }
            Err(error) => {
                // 审计与 CLI bind-web 同源：英文 detail，管道保证不含 token。
                self.audit.append(AuditEvent::err(
                    "bind-web",
                    "kimi",
                    Some(target.0.as_str()),
                    &error.to_string(),
                ));
                Err(anyhow::anyhow!(bind_web_user_message(&error)))
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

/// 会员到期「仅刷新」的替身面（issue #52）：行列组装在用户发起刷新
/// （[`QuotaCollectReason::UserInitiated`]）时对每张卡调用一次。生产实现
/// [`LiveWebMembershipRefresh`] 发真实 `GetSubscription`；测试注入计数 +
/// 脚本化替身（见 lib.rs tests）。
#[async_trait::async_trait]
pub(crate) trait WebMembershipRefresh: Send + Sync {
    /// 刷新目标账号的会员到期：
    /// - `None` = 未绑定网页登录（没有网页凭证，本次不发请求、不产生刷新失败语义）；
    /// - `Some(Ok(..))` = 已绑定且刷新成功（extra 已由管道重写，携带新到期展示日）；
    /// - `Some(Err(..))` = 已绑定但刷新失败（extra 原样不动，展示层缓存降级）。
    async fn refresh(&self, target: &AccountId) -> Option<kimi_switch_core::Result<WebBinding>>;
}

/// 生产实现（issue #52 触发口径的网页侧）：store 里有网页凭证才算已绑定，
/// 刷新走 kimi 适配器（域名固定 `https://www.kimi.com`，超时 15s）。探测读
/// 失败按未绑定处理——宁可少一次网页请求，也不在计划外路径制造提示。
struct LiveWebMembershipRefresh {
    registry: Arc<AccountRegistry>,
    store: Arc<dyn CredentialStore>,
}

#[async_trait::async_trait]
impl WebMembershipRefresh for LiveWebMembershipRefresh {
    async fn refresh(&self, target: &AccountId) -> Option<kimi_switch_core::Result<WebBinding>> {
        let bound = matches!(
            self.store.get("kimi", target.0.as_str(), WEB_STORE_FIELD),
            Ok(Some(_))
        );
        if !bound {
            return None;
        }
        Some(
            web_membership::refresh_web_membership(
                self.registry.as_ref(),
                self.store.as_ref(),
                target,
            )
            .await,
        )
    }
}

/// 会员刷新 join 阶段的总预算（issue #52 补充）：额度收集被 5 秒节流窗毫秒级
/// 返回，而网页 `GetSubscription` 单请求超时 15s——无预算时 www.kimi.com 挂起
/// 会让一次刷新等满 15s，前端 busy 闸门把切换在内全部操作锁死。2s 足够覆盖
/// 正常的亚秒级会员请求，超出即视为当次不可达。
const MEMBERSHIP_JOIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// 列出与刷新共用：走 core 额度快照（节流 / 退避 / 并发），GUI 只做展示映射。
///
/// 会员到期刷新（issue #52 触发口径）：仅 [`QuotaCollectReason::UserInitiated`]
/// （用户点「刷新」）对已绑定账号并发刷新会员到期（与额度收集并行发起，单请求
/// 超时 15s；join 阶段另受 [`MEMBERSHIP_JOIN_BUDGET`] 总预算约束，网页挂起
/// 不得拖死整次刷新），缓存日历日无论新旧（含已过期）都请求；Scheduled（启动
/// 加载 / 静默收集 / swap / import / 绑定等操作后再列）完全不发网页请求。会员
/// 刷新失败只降级为行内绑定提示（日期 + hint 共存），不影响额度行、不计告急、
/// 不置凭证待续期。后端快照不写会员 toast；GUI 用户点刷新后自行聚合瞬时提示。
pub(crate) async fn collect_account_rows<P>(
    provider: Arc<P>,
    membership: Option<Arc<dyn WebMembershipRefresh>>,
    accounts: &[Account],
    cache_path: &std::path::Path,
    reason: QuotaCollectReason,
) -> (Vec<AccountRow>, QuotaCollectStats)
where
    P: Provider + 'static,
{
    let ids: Vec<AccountId> = accounts.iter().map(|account| account.id.clone()).collect();

    // 会员刷新与额度收集并行发起（账号序即结果序），额度结果语义不被会员拖慢。
    let membership_jobs = match (&membership, reason) {
        (Some(source), QuotaCollectReason::UserInitiated) => accounts
            .iter()
            .map(|account| {
                let source = source.clone();
                let id = account.id.clone();
                tokio::spawn(async move { source.refresh(&id).await })
            })
            .collect::<Vec<_>>(),
        // Scheduled / 静默路径连替身都不调用：网页会员请求恒为 0。
        _ => Vec::new(),
    };

    let outcome = kimi_switch_core::quota_snapshot::collect_detailed(
        provider, "kimi", &ids, cache_path, reason,
    )
    .await;

    let mut refreshed: Vec<Option<kimi_switch_core::Result<WebBinding>>> =
        Vec::with_capacity(membership_jobs.len());
    // 预算是整个 join 阶段的总闸（不是每任务各 2s）：deadline 固定，谁先完成
    // 谁先收，已完成的正常使用，超预算才降级。
    let join_deadline = tokio::time::Instant::now() + MEMBERSHIP_JOIN_BUDGET;
    for job in membership_jobs {
        // 替身任务 panic 按「已绑定但刷新失败」降级，不拖垮整次列出。
        refreshed.push(match tokio::time::timeout_at(join_deadline, job).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(join)) => Some(Err(kimi_switch_core::Error::provider(format!(
                "membership refresh task failed: {join}"
            )))),
            // 预算耗尽：未完成的任务按「刷新失败」降级（行内保留 extra 旧日期 +
            // 绑定提示再现）。底层任务不 cancel——JoinHandle 被丢弃即 detach，
            // refresh_web_membership 在任务内自行写 extra，放任其在后台完成后
            // 由下次列出自然呈现。
            Err(_elapsed) => Some(Err(kimi_switch_core::Error::provider(
                "membership refresh join timed out",
            ))),
        });
    }

    let now = chrono::Utc::now();
    let rows = accounts
        .iter()
        .zip(outcome.snapshots.iter())
        .enumerate()
        .map(|(idx, (account, snapshot))| {
            account_row_at(
                account,
                snapshot,
                refreshed.get(idx).and_then(|outcome| outcome.as_ref()),
                now,
            )
        })
        .collect();
    (rows, outcome.stats)
}

/// persist 官方昵称后，用 registry 里已带 `official_nickname` 的 Account 生成 toast。
pub(crate) fn account_for_notice(registry: &AccountRegistry, account: Account) -> Account {
    registry
        .find(&account.provider, &account.id)
        .ok()
        .flatten()
        .unwrap_or(account)
}

pub(crate) fn import_success_message(account: &Account) -> String {
    format!("已导入账号 {}", display_name(account))
}

pub(crate) fn swap_success_message(account: &Account, plan: Option<&str>) -> String {
    let display = display_name(account);
    match plan {
        Some(plan) if !plan.is_empty() => format!("已切换到 {display}（{plan}）"),
        _ => format!("已切换到 {display}"),
    }
}

/// 只读本地额度缓存里的会员等级，映射成官网档位名。读取失败或无缓存则 `None`，不打网。
pub(crate) fn cached_membership_plan(
    cache: &kimi_switch_core::QuotaCache,
    account_id: &str,
) -> Option<String> {
    let entry = cache.get("kimi", account_id)?;
    let raw = entry
        .quotas
        .iter()
        .find_map(|quota| quota.membership.as_deref())?;
    let pretty = prettify_membership(raw);
    if pretty.is_empty() {
        None
    } else {
        Some(pretty)
    }
}

pub(crate) fn swap_failure_message(account: &Account) -> String {
    format!("切换到 {} 失败", display_name(account))
}

pub(crate) fn delete_success_message(account: &Account) -> String {
    format!("已删除 {}", display_name(account))
}

pub(crate) fn rename_success_message(account: &Account, new_label: &str) -> String {
    format!("已把 {} 重命名为「{new_label}」", display_name(account))
}

pub(crate) fn auth_success_message(account: &Account) -> String {
    format!("授权成功，已添加账号 {}", display_name(account))
}

pub(crate) fn reauth_success_message(account: &Account) -> String {
    format!("已更新账号 {} 的登录凭证", display_name(account))
}

/// 用户发起且没有任何账号真正查询时，把整批跳过原因写成快照提示。
/// 计划内收集不弹这些提示；调用方已有文案时不覆盖。
pub(crate) fn attach_skip_message(
    message: Option<UiMessage>,
    reason: QuotaCollectReason,
    stats: &QuotaCollectStats,
) -> Option<UiMessage> {
    if message.is_some() {
        return message;
    }
    if reason != QuotaCollectReason::UserInitiated || stats.queried > 0 {
        return None;
    }
    match (
        stats.skipped_success_throttle,
        stats.skipped_rate_limit,
        stats.skipped_auth,
    ) {
        (n, 0, 0) if n > 0 => Some(UiMessage {
            text: "刚刚更新过".into(),
            tone: "info".into(),
        }),
        (0, n, 0) if n > 0 => Some(UiMessage {
            text: "请求过于频繁，请稍后再试".into(),
            tone: "info".into(),
        }),
        _ => None,
    }
}

/// 绑定成功的 toast 文案（issue #51）。订阅无到期瞬间时到期展示回退弱提示，
/// 文案说明「暂无到期信息」，避免用户误以为绑定失败。
pub(crate) fn bind_web_success_message(binding: &WebBinding) -> UiMessage {
    let text = match binding.expiry_text.as_deref() {
        Some(expiry) => format!("绑定成功，有效期至 {expiry}"),
        None => "绑定成功，暂无会员到期信息".into(),
    };
    UiMessage {
        text,
        tone: "ok".into(),
    }
}

/// 绑定失败的 GUI 用户文案（issue #51）：简洁中文，按结构化载荷映射。
/// GetSubscription 的网络 / HTTP / 解析失败整条替换为固定中文（即使管道文案
/// 意外夹带 token 也不会透到前端）；意外库存 / IO 失败保留细节便于排查。
pub(crate) fn bind_web_user_message(error: &kimi_switch_core::Error) -> String {
    match error.bind_web_error() {
        Some(kimi_switch_core::BindWebError::IdentityMismatch { .. }) => {
            "网页身份与该账号不一致，未绑定".into()
        }
        Some(kimi_switch_core::BindWebError::Unidentifiable) => {
            "无法从该 token 辨认网页身份，未绑定".into()
        }
        // #50 管道里 GetSubscription 的请求 / HTTP / 解析失败都是 Provider 载荷。
        None if matches!(error, kimi_switch_core::Error::Provider(_)) => {
            "会员信息获取失败，请稍后再试".into()
        }
        None if matches!(error, kimi_switch_core::Error::AccountNotFound { .. }) => {
            "账号不存在，请刷新后再试".into()
        }
        None => format!("绑定失败：{error}"),
    }
}

/// 组装单行展示 DTO（时钟取组装时刻的 UTC now）。测试便捷包装。
#[cfg(test)]
pub(crate) fn account_row(
    account: &Account,
    snapshot: &QuotaSnapshot,
    membership_refresh: Option<&kimi_switch_core::Result<WebBinding>>,
) -> AccountRow {
    account_row_at(account, snapshot, membership_refresh, chrono::Utc::now())
}

/// 组装单行展示 DTO。
///
/// `membership_refresh` 是本次用户发起刷新中该账号的会员刷新结果（#52）：
/// - `None` = 本次没有刷新（Scheduled / 静默路径，或该账号未绑定）；
/// - `Some(Ok(..))` = 刷新成功：行内直接用新到期日（extra 已由管道写回），
///   订阅无到期数据时日期为 `None` 并回到绑定弱提示（与绑定语义一致）；
/// - `Some(Err(..))` = 刷新失败（缓存降级）：行内保留 extra 缓存旧日期，
///   且 hint 再现——**日期与 hint 共存**，网页失败不单独把日期藏起来。
///
/// `now` 是组装这批账号行时的 UTC 时钟，转到东八区日期后算剩余天数。
pub(crate) fn account_row_at(
    account: &Account,
    snapshot: &QuotaSnapshot,
    membership_refresh: Option<&kimi_switch_core::Result<WebBinding>>,
    now: chrono::DateTime<chrono::Utc>,
) -> AccountRow {
    let membership = snapshot.membership.as_deref().map(prettify_membership);
    // 到期日基线来自账号 extra（绑定 / 上次成功刷新写入）；刷新成功时用刚拿到
    // 的新值，不等下次列出。
    let (membership_expiry, membership_refresh_failed) = match membership_refresh {
        Some(Ok(binding)) => (binding.expiry_text.clone(), false),
        Some(Err(_)) => (membership_expiry_from_extra(&account.extra), true),
        None => (membership_expiry_from_extra(&account.extra), false),
    };
    // 档位在且（无到期日，或本次用户发起刷新中会员获取失败）→ 绑定弱提示；
    // 可与日期共存（#52 缓存降级）。档位缺失（无额度数据）保持现状不强加。
    // hint 布尔算法不变：有档位且（无到期日或本次刷新会员获取失败）。
    let membership_expiry_hint =
        membership.is_some() && (membership_expiry.is_none() || membership_refresh_failed);
    let membership_remaining_days = membership_expiry
        .as_deref()
        .and_then(|day| membership_remaining_days(day, now));
    // 周期天数与到期日同一基线：刷新成功用刚拿到的值，缓存降级/未刷新读 extra。
    // 刷新成功但新订阅缺开始键时不回退旧 extra——旧开始配新到期日会算错周期。
    let membership_period_days = match membership_refresh {
        Some(Ok(binding)) => binding.period_days,
        _ => membership_expiry.as_deref().and_then(|day| {
            membership_start_from_extra(&account.extra)
                .and_then(|start| membership_period_days(start, day))
                .map(i64::from)
        }),
    };
    let (quotas, error, credential_expired, cached_at) = match &snapshot.state {
        QuotaState::Ready(quotas) | QuotaState::Cached(quotas) => (
            quota_views(quotas),
            None,
            false,
            Some(format_fetched_at(snapshot.fetched_at, snapshot.stale)),
        ),
        QuotaState::Failed(failure) => (
            Vec::new(),
            Some(compact_error(&failure.message)),
            failure.kind == QuotaFailureKind::CredentialExpired,
            // 查询失败且无缓存回退：没有额度取得时间可标，不带时间文案。
            None,
        ),
    };
    AccountRow {
        id: account.id.0.clone(),
        label: account.label.clone(),
        active: account.active,
        membership,
        membership_expiry,
        membership_remaining_days,
        membership_period_days,
        membership_expiry_hint,
        quotas,
        error,
        credential_expired,
        cached_at,
        display_name: display_name(account),
        official_nickname_known: official_nickname_known(&account.extra),
    }
}

fn format_fetched_at(fetched_at: chrono::DateTime<chrono::Utc>, stale: bool) -> String {
    let prefix = if stale { "缓存于" } else { "更新于" };
    format!(
        "{} {}",
        prefix,
        fetched_at
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
    )
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
            let reset = q
                .reset_at
                .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string());
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
