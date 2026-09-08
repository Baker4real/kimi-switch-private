//! 文件型 provider 的 provider 无关机制。差异点见 [`crate::FileBlobRuntime`]。
//!
//! 关键约束（与 `crates/providers/beta/src/lib.rs` 保持一致）：
//! - `activate` 不依赖网络；切换 = flock → snapshot 旧文件 → 原子写新 blob → 任一步失败回滚。
//! - capture-on-leave（覆盖 live 前把 live 凭证回灌进 owner 账号 store）必须放在
//!   `spawn_blocking` 内部执行，不能用 `block_in_place`：本引擎的 activate 调用方既可能运行在
//!   多线程 runtime（cli/daemon）也可能是 current-thread runtime（测试），`block_in_place` 在
//!   后者会直接 panic。Beta 现有实现把 capture 调用放进 spawn_blocking 正是为了避开这个问题，
//!   本引擎沿用同样的取舍：`runtime`/`store`/`registry` 都以 `Arc` 形式 clone 进同一个
//!   `spawn_blocking` 闭包。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use kimi_switch_core::error::{Error, Result};
use kimi_switch_core::swap::{swap_with_snapshot, SwapTarget};
use kimi_switch_core::{Account, AccountId, AccountRegistry, CredentialStore, Provider, Quota};

use crate::json::{extract_access_token, extract_refresh_token};
use crate::runtime::{BlobMetadata, FileBlobRuntime, RefreshOutcome};

/// 文件型 OAuth 账号切换引擎：接一个 [`FileBlobRuntime`] adapter 即可获得完整 [`Provider`] 实现。
pub struct FileBlobProvider<A: FileBlobRuntime> {
    runtime: Arc<A>,
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    home: PathBuf,
}

// ---------------------------------------------------------------------------
// 构造 / 访问器
// ---------------------------------------------------------------------------

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    pub fn new(
        runtime: A,
        store: Arc<dyn CredentialStore>,
        registry: Arc<AccountRegistry>,
    ) -> Self {
        let runtime = Arc::new(runtime);
        let home = runtime.home();
        Self {
            runtime,
            store,
            registry,
            home,
        }
    }

    /// store 里该 provider 使用的凭证字段名。删除账号必须用这个名字，调用方不得硬编码。
    pub fn store_field(&self) -> &'static str {
        self.runtime.store_field()
    }

    fn live_path(&self) -> PathBuf {
        self.runtime.live_cred_path(&self.home)
    }

    /// 原子写 live 凭证：tmp + rename + 0o600。
    pub(crate) fn write_blob(path: &Path, contents: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn require_account(&self, id: &AccountId) -> Result<Account> {
        self.registry
            .find(self.runtime.id(), id)?
            .ok_or_else(|| Error::AccountNotFound {
                provider: self.runtime.id().into(),
                id: id.to_string(),
            })
    }

    /// 元数据是否指向给定账号：primary_id/label 命中 account.id 或 account.label 任一值，
    /// 或跨主键去重键命中（去重键的 extra 字段名由 `dedup_extra_key` 决定，与原始 Beta
    /// 实现的 `auth_metadata_matches_account`/`string_matches_account` 保持完全一致的比较范围）。
    fn metadata_matches(meta: &BlobMetadata, account: &Account, dedup_extra_key: &str) -> bool {
        let id_hit = meta
            .primary_id
            .as_deref()
            .is_some_and(|v| Self::value_matches_account(v, account))
            || meta
                .label
                .as_deref()
                .is_some_and(|v| Self::value_matches_account(v, account));
        let dedup_hit = match (
            meta.dedup_key.as_deref(),
            account.extra.get(dedup_extra_key),
        ) {
            (Some(dk), Some(v)) => v.as_str() == Some(dk),
            _ => false,
        };
        id_hit || dedup_hit
    }

    /// 单个候选值是否命中账号：等于 account.id，或（label 非空时）等于 account.label。
    fn value_matches_account(value: &str, account: &Account) -> bool {
        value == account.id.0 || (!account.label.trim().is_empty() && value == account.label)
    }

    /// 官方身份辨认：`primary_id`，缺失则回退 `label`。
    /// 导入、切换、重新授权共用这一处定义，不得在调用方再解析凭证。
    fn official_identity(meta: &BlobMetadata) -> Option<&str> {
        meta.primary_id.as_deref().or(meta.label.as_deref())
    }
}

// ---------------------------------------------------------------------------
// 存账号 / 导入
// ---------------------------------------------------------------------------

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// 从 blob 落库：blob 进 store，元数据进 registry.toml。`active_override=None` 时保留原 active。
    /// 官方身份由 [`Self::official_identity`] 从 `parse_metadata` 得出。
    fn store_account(
        &self,
        raw: String,
        label_hint: Option<String>,
        active_override: Option<bool>,
    ) -> Result<Account> {
        let meta = self.runtime.parse_metadata(&raw);
        self.store_account_with_metadata(raw, meta, label_hint, active_override)
    }

    /// 同 [`Self::store_account`]，但接收调用方提供的、已解析好的 metadata，不再从 `raw` 重新派生。
    fn store_account_with_metadata(
        &self,
        raw: String,
        meta: BlobMetadata,
        label_hint: Option<String>,
        active_override: Option<bool>,
    ) -> Result<Account> {
        let id_string = Self::official_identity(&meta)
            .map(str::to_string)
            .or_else(|| label_hint.clone())
            .ok_or_else(|| {
                Error::provider(
                    "cannot parse account id from credentials; pass --label to set it explicitly",
                )
            })?;
        let id = AccountId(id_string);
        let label = label_hint
            .or_else(|| meta.label.clone())
            .unwrap_or_else(|| id.0.clone());

        self.store.set(
            self.runtime.id(),
            id.0.as_str(),
            self.runtime.store_field(),
            &raw,
        )?;

        let mut extra = meta.extra.clone();
        if let Some(dk) = meta.dedup_key.clone() {
            extra.insert(
                self.runtime.dedup_extra_key().into(),
                serde_json::Value::String(dk),
            );
        }

        let existing = self
            .registry
            .find(self.runtime.id(), &id)?
            .or_else(|| self.find_by_dedup(&meta));
        if let Some(ex) = existing.as_ref() {
            if ex.id != id {
                let _ = self.store.delete(
                    self.runtime.id(),
                    ex.id.0.as_str(),
                    self.runtime.store_field(),
                );
                self.registry.remove(self.runtime.id(), &ex.id)?;
            }
        }

        let account = Account {
            provider: self.runtime.id().into(),
            id: id.clone(),
            label,
            active: active_override.unwrap_or_else(|| existing.as_ref().is_some_and(|a| a.active)),
            created_at: existing
                .as_ref()
                .map(|a| a.created_at)
                .unwrap_or_else(Utc::now),
            last_used_at: existing.and_then(|a| a.last_used_at),
            extra,
        };
        self.registry.upsert(account.clone())?;
        Ok(account)
    }

    fn find_by_dedup(&self, meta: &BlobMetadata) -> Option<Account> {
        let target = meta.dedup_key.as_deref()?;
        let dedup_extra_key = self.runtime.dedup_extra_key();
        self.registry
            .list_by_provider(self.runtime.id())
            .ok()?
            .into_iter()
            .find(|a| a.extra.get(dedup_extra_key).and_then(|v| v.as_str()) == Some(target))
    }

    /// 从当前 live 文件导入（可切换）。
    pub fn import_active(&self, label_hint: Option<String>) -> Result<Account> {
        let raw = fs::read_to_string(self.live_path())
            .map_err(|e| Error::provider(format!("read live credentials failed: {e}")))?;
        self.store_account(raw, label_hint, Some(true))
    }

    /// 只对齐当前 live 的元数据 active 标记，不写凭证（默认入口用，避免弹钥匙串）。
    pub fn sync_active_metadata(&self, label_hint: Option<String>) -> Result<Account> {
        let raw = fs::read_to_string(self.live_path())
            .map_err(|e| Error::provider(format!("read live credentials failed: {e}")))?;
        // 复用 store_account，但不覆盖 store 里已有的可切换 blob：仅当 store 尚无该账号时才写。
        let meta = self.runtime.parse_metadata(&raw);
        if let Some(pid) = Self::official_identity(&meta) {
            let has_blob = self
                .store
                .get(self.runtime.id(), pid, self.runtime.store_field())
                .ok()
                .flatten()
                .is_some();
            if has_blob {
                // 已有可切换副本：只更新 active 标记，不重写 blob。
                let id = AccountId(pid.to_string());
                self.registry.set_active(self.runtime.id(), &id)?;
                return self.registry.find(self.runtime.id(), &id)?.ok_or_else(|| {
                    Error::AccountNotFound {
                        provider: self.runtime.id().into(),
                        id: id.to_string(),
                    }
                });
            }
        }
        self.store_account(raw, label_hint, Some(true))
    }

    /// 当前 live 凭证对应的 registry id。默认入口用它对照墓碑，避免 `rm` 后被自动导入加回。
    pub fn live_account_id(&self) -> Result<AccountId> {
        let raw = fs::read_to_string(self.live_path())
            .map_err(|e| Error::provider(format!("read live credentials failed: {e}")))?;
        let meta = self.runtime.parse_metadata(&raw);
        if let Some(existing) = self.find_by_dedup(&meta) {
            return Ok(existing.id);
        }
        let id_string = Self::official_identity(&meta)
            .ok_or_else(|| Error::provider("cannot parse account id from live credentials"))?;
        Ok(AccountId(id_string.to_string()))
    }

    /// 从原始 blob 导入（设备码授权入库）。
    pub fn import_raw(
        &self,
        raw: String,
        label_hint: Option<String>,
        active: Option<bool>,
    ) -> Result<Account> {
        self.store_account(raw, label_hint, active)
    }
}

// ---------------------------------------------------------------------------
// 取 blob / capture-on-leave 守卫 / reconcile / 隔离导出导入
// ---------------------------------------------------------------------------

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// 取账号 blob：active 账号优先读 live（并顺手修复 store 副本），parked 读 store，最后 legacy。
    ///
    /// 「顺手修复 store 副本」是 ADR-0002 修订记录的第二条回灌路径：重授权 live 写失败
    /// 窗口内对活跃账号一次读取（查额度/导出）即把旧 live 回灌库存、撤销新凭证——
    /// 这是已接受的对外行为（窗口期一律以 live 为准），勿在此加防御。
    ///
    /// 错误处理顺序与原始 Beta 实现一致：store 读取失败时不立即冒泡，而是先捕获错误、
    /// 继续尝试 `recover_legacy`；只有 legacy 也拿不到时才把原始 store 错误抛出去。这样即使
    /// keyring 后端本身故障，仍有机会从 legacy 布局恢复凭证。
    fn raw_blob_for_account(&self, account: &Account) -> Result<String> {
        if let Some(raw) = self.read_live_if_matches(account) {
            let _ = self.store.set(
                self.runtime.id(),
                account.id.0.as_str(),
                self.runtime.store_field(),
                &raw,
            );
            return Ok(raw);
        }
        let store_error = match self.store.get(
            self.runtime.id(),
            account.id.0.as_str(),
            self.runtime.store_field(),
        ) {
            Ok(Some(raw)) => return Ok(raw),
            Ok(None) => None,
            Err(e) => Some(e),
        };
        if let Some(raw) = self.runtime.recover_legacy(&self.home, account) {
            let _ = self.store.set(
                self.runtime.id(),
                account.id.0.as_str(),
                self.runtime.store_field(),
                &raw,
            );
            return Ok(raw);
        }
        if let Some(e) = store_error {
            return Err(e);
        }
        Err(Error::Credential(format!(
            "no stored credentials for {}:{}; run `kimi-switch login {}` or re-import",
            self.runtime.id(),
            account.id,
            self.runtime.id()
        )))
    }

    fn read_live_if_matches(&self, account: &Account) -> Option<String> {
        let raw = fs::read_to_string(self.live_path()).ok()?;
        let meta = self.runtime.parse_metadata(&raw);
        Self::metadata_matches(&meta, account, self.runtime.dedup_extra_key()).then_some(raw)
    }

    /// 覆盖 live 前把 live 凭证回灌进其 owner 账号 store。
    /// 只接收借用参数，供 `activate` 的 `spawn_blocking` 闭包与同步 reconcile 共用一份实现。
    ///
    /// 守卫：live 缺 refresh 且 store 已有 refresh 时跳过（防静默写死账号）。
    fn capture_live_into_store_with(
        runtime: &A,
        store: &dyn CredentialStore,
        registry: &AccountRegistry,
        home: &Path,
    ) -> Result<()> {
        let live_raw = match fs::read_to_string(runtime.live_cred_path(home)) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        let meta = runtime.parse_metadata(&live_raw);
        let dedup_extra_key = runtime.dedup_extra_key();
        let owner = registry
            .list_by_provider(runtime.id())?
            .into_iter()
            .find(|a| Self::metadata_matches(&meta, a, dedup_extra_key));
        let Some(owner) = owner else { return Ok(()) };

        if extract_refresh_token(&live_raw).is_none() {
            if let Some(existing) =
                store.get(runtime.id(), owner.id.0.as_str(), runtime.store_field())?
            {
                if extract_refresh_token(&existing).is_some() {
                    tracing::warn!(
                        account = %owner.id,
                        "live capture missing refresh_token; skipped overwrite to keep existing store copy"
                    );
                    return Ok(());
                }
            }
        }
        store.set(
            runtime.id(),
            owner.id.0.as_str(),
            runtime.store_field(),
            &live_raw,
        )?;
        Ok(())
    }

    /// 只 live→store，不碰 active 标记。
    #[cfg(test)]
    fn reconcile_active_from_live(&self) -> Result<()> {
        Self::capture_live_into_store_with(
            self.runtime.as_ref(),
            self.store.as_ref(),
            self.registry.as_ref(),
            &self.home,
        )
    }

    /// 按卡重新授权原语：用一次新的授权结果 blob 覆盖指定账号的库存凭证。
    ///
    /// 官方身份辨认走 [`Self::official_identity`]（`primary_id`，缺失时回退 `label`），
    /// 与导入、切换同一回退顺序：
    /// - 解析出的官方账号 id 与目标不同、或无法辨认 → 返回 [`kimi_switch_core::ReauthError`]；不写库存、不写 live、不增删账号；
    /// - 相同 → 替换该账号的库存 blob；registry 原样保留（别名、active 标记不变，
    ///   同一官方身份仍是一条）；JSON 非法的 blob 解析不出主键，自然落入「无法辨认」拒绝路径；
    /// - 目标是当前活跃账号 → 再把新 blob 原子写进 live 登录文件；非当前则完全不碰 live。
    ///
    /// 注意：这里严禁调用 capture-on-leave 之类的旧态回灌——它们会把旧 live
    /// 盖进刚替换的库存副本。写顺序固定「先库存后 live」：
    /// live 写失败时新凭证已安全落在库存，不会丢号。
    ///
    /// 窗口期（store=新/live=旧）对外一律以 live 为准：其后的查询或切换都会把旧 live
    /// 回灌库存、静默撤销新凭证（已接受行为，见 ADR-0002 修订与 issue #5）；补救是重试重新授权。
    ///
    /// 错误文本只含账号 id 与原因，不含任何凭证内容。
    pub fn replace_account_blob(&self, target: &AccountId, raw: String) -> Result<Account> {
        let meta = self.runtime.parse_metadata(&raw);
        let observed = Self::official_identity(&meta).ok_or_else(Error::reauth_unidentifiable)?;
        if observed != target.0 {
            return Err(Error::reauth_identity_mismatch(observed, &target.0));
        }
        let account = self.require_account(target)?;

        // 先落库存（唯一真本），再按 active 决定是否同步 live。
        self.store.set(
            self.runtime.id(),
            account.id.0.as_str(),
            self.runtime.store_field(),
            &raw,
        )?;
        if account.active {
            // 原子写（tmp + rename）：失败时旧 live 未被破坏，新凭证已在库存可重试。
            // 窗口期对外行为见 ADR-0002 修订：live 仍为旧凭证，其后的查询/切换都以旧 live 为准，
            // 错误文案必须把状态与补救讲清楚。
            Self::write_blob(&self.live_path(), &raw).map_err(Error::reauth_live_write_failed)?;
        }
        Ok(account)
    }
}

// ---------------------------------------------------------------------------
// Provider 实现：activate + query_quota + 凭证恢复信号
// ---------------------------------------------------------------------------

#[async_trait]
impl<A: FileBlobRuntime> Provider for FileBlobProvider<A> {
    async fn activate(&self, id: &AccountId) -> Result<()> {
        // 阶段 1：异步预处理（仅查 registry + store，无网络调用），与 Beta 现有实现一致。
        let account = self.require_account(id)?;
        let target_raw = self.raw_blob_for_account(&account)?;

        // 阶段 2：把需要阻塞 IO 的部分（含 capture-on-leave）整体搬进 spawn_blocking。
        // 不用 `block_in_place`：调用方可能运行在 current-thread runtime（如测试），
        // `block_in_place` 在那种 runtime 下会直接 panic；`spawn_blocking` 没有这个限制。
        let home = self.home.clone();
        let live_path = self.live_path();
        let provider_id = self.runtime.id();
        let runtime = self.runtime.clone();
        let store = self.store.clone();
        let registry = self.registry.clone();
        let id_owned = id.clone();

        tokio::task::spawn_blocking(move || {
            // capture-on-leave：覆盖 live 前，先把当前 live 凭证回灌进它所属账号的 store。
            // 否则切走的账号 store 副本会停在旧 refresh token，下次切回写回旧 token → "already used"。
            if let Err(e) = FileBlobProvider::<A>::capture_live_into_store_with(
                runtime.as_ref(),
                store.as_ref(),
                registry.as_ref(),
                &home,
            ) {
                tracing::warn!(err = %e, "capture-on-leave failed; continuing swap");
            }

            let blob = target_raw;
            let targets = vec![SwapTarget {
                snapshot_name: "credentials",
                live_path,
                writer: Box::new(move |p: &Path| FileBlobProvider::<A>::write_blob(p, &blob)),
            }];
            let lock_dir = registry
                .path()
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.clone());
            swap_with_snapshot(provider_id, &home, &lock_dir, targets, || {
                registry.set_active(provider_id, &id_owned)
            })
        })
        .await
        .map_err(|e| Error::provider(format!("spawn_blocking join failed: {e}")))?
    }

    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        let account = self.require_account(id)?;
        let mut raw = self.raw_blob_for_account(&account)?;

        if account.active {
            // 当前账号主动续期（issue #44）：仅当 runtime 判定该轮换
            // （开关开启 + 本平台无官方锁 + access token 临期）才介入；
            // 否则与旧版行为一致——不主动轮换，401 走协调恢复/凭证过期归类。
            if self.runtime.proactive_renewal_wanted(&raw).await {
                raw = self.renew_active_proactively(&account, raw).await?;
            }
        } else if let Some(fresh) = self.refresh_parked_if_needed(&account, &raw).await? {
            // parked 账号：access token 大概率过期，先按需刷新。
            raw = fresh;
        }
        let access = extract_access_token(&raw).ok_or_else(|| {
            Error::quota_missing_credential(
                "no access_token in credentials; schema may have changed",
            )
        })?;
        self.runtime.fetch_quota(&access, &account).await
    }

    /// 当前账号：当前登录文件 mtime 晚于 `failed_at` 即凭证恢复信号；停泊账号恒无信号。
    fn credential_changed_since(&self, account_id: &AccountId, failed_at: DateTime<Utc>) -> bool {
        let Ok(Some(account)) = self.registry.find(self.runtime.id(), account_id) else {
            return false;
        };
        if !account.active {
            return false;
        }
        let Ok(modified) = fs::metadata(self.live_path()).and_then(|meta| meta.modified()) else {
            return false;
        };
        DateTime::<Utc>::from(modified) > failed_at
    }
}

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// 当前账号主动续期（issue #44，ADR-0003 第 2 条）：查询前轮换临期 live 凭证并写回。
    /// 仅当 runtime 报告 [`FileBlobRuntime::proactive_renewal_wanted`] 时由 `query_quota` 调用。
    ///
    /// 写序遵守 ADR-0002：账号库为唯一真本，先落账号库槽位，再原子写 live（tmp+rename，
    /// 沿用现有 swap 写序）。轮换被官方拒绝（invalid_grant）时记录指纹并停止重试，
    /// 本地凭证不删除不墓碑化，本次查询按「凭证过期型 401」（issue #41）呈现。
    ///
    /// 如实收录：live 原子写失败（本地 IO）**不是**可恢复窗口。live 保持旧内容，
    /// 但下一次当前账号查询会经 [`Self::raw_blob_for_account`] 把旧 live 无条件回灌
    /// 库槽位，销毁新轮换 refresh token 的唯一副本；此后轮换必被官方 invalid_grant
    /// 拒绝，收敛到凭证过期型 401 + 指纹停试，需用户按引导重新授权
    /// （ADR-0002 修订明文接受「窗口期一律以 live 为准」，ADR-0003 已接受边界）。
    async fn renew_active_proactively(&self, account: &Account, raw: String) -> Result<String> {
        // 即用即读：轮换前现读 live 文件（官方即用即读，我们也即用即读），
        // 把与官方轮换的竞态窗口压到最小，不用查询开始时的旧副本。
        let Some(live) = self.read_live_if_matches(account) else {
            // live 读不出或已改属其他账号：不碰 live 与账号库，沿用原凭证查询。
            return Ok(raw);
        };
        if extract_access_token(&live).as_deref() != extract_access_token(&raw).as_deref() {
            // 官方已抢先轮换（与官方 100ms 重读采纳机制同款）：直接采纳最新 live，
            // 顺手回灌账号库保持真本新鲜；本次查询不再消耗 refresh token。
            // 回灌只是顺手保鲜：库写失败不应让手握有效 live 的查询整体失败，
            // 与 `raw_blob_for_account` 的回灌容错（`let _ =`）一致。
            let _ = self.store.set(
                self.runtime.id(),
                account.id.0.as_str(),
                self.runtime.store_field(),
                &live,
            );
            return Ok(live);
        }
        // DeadToken 纪律：指纹在案 → 停止重试，本次查询按凭证过期呈现。
        if self.runtime.proactive_renewal_blocked(&self.home, &live) {
            return Err(self.runtime.proactive_renewal_reject(&self.home, &live)?);
        }
        match self.runtime.refresh(&live).await? {
            RefreshOutcome::Rotated(new_blob) => {
                // ADR-0002 写序：先落账号库槽位，再原子写 live。live 写失败时
                // live 保持旧内容不被破坏——但此窗口不可恢复（如实收录，见函数 doc）：
                // 下一次当前账号查询的 live 回灌会销毁库中新凭证副本，随后收敛到
                // invalid_grant → 记指纹停试 → 凭证过期型 401 引导重新授权。
                self.store.set(
                    self.runtime.id(),
                    account.id.0.as_str(),
                    self.runtime.store_field(),
                    &new_blob,
                )?;
                Self::write_blob(&self.live_path(), &new_blob)?;
                self.runtime.proactive_renewal_rotated(&self.home, &live)?;
                Ok(new_blob)
            }
            RefreshOutcome::DeadToken => {
                Err(self.runtime.proactive_renewal_reject(&self.home, &live)?)
            }
            RefreshOutcome::Unsupported => Ok(live),
        }
    }
}

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// parked 账号刷新一次并写回 store。返回 `Some(新 blob)` 表示已轮换。
    ///
    /// DeadToken（issue #80）：复用当前账号主动续期的指纹纪律——发前若指纹在案则跳过
    /// refresh；被拒时记录指纹并停止后续重试。查询仍用现有 access token 继续（不把
    /// 停泊路径改成凭证过期硬失败）。
    async fn refresh_parked_if_needed(
        &self,
        account: &Account,
        raw: &str,
    ) -> Result<Option<String>> {
        // DeadToken 纪律：指纹在案 → 停止重试，继续用现有 access token 查询。
        if self.runtime.proactive_renewal_blocked(&self.home, raw) {
            tracing::warn!(account = %account.id, "refresh token dead; skip retry");
            return Ok(None);
        }
        match self.runtime.refresh(raw).await? {
            RefreshOutcome::Rotated(new_blob) => {
                self.store.set(
                    self.runtime.id(),
                    account.id.0.as_str(),
                    self.runtime.store_field(),
                    &new_blob,
                )?;
                Ok(Some(new_blob))
            }
            RefreshOutcome::DeadToken => {
                tracing::warn!(account = %account.id, "refresh token dead; needs re-login");
                // 记录指纹后仍返回 None：本次查询继续用旧 access token。
                let _ = self.runtime.proactive_renewal_reject(&self.home, raw)?;
                Ok(None)
            }
            RefreshOutcome::Unsupported => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// 测试专用最小可见性访问器（不进正式公共 API）。
// ---------------------------------------------------------------------------

#[cfg(test)]
impl<A: FileBlobRuntime> FileBlobProvider<A> {
    pub(crate) fn test_home(&self) -> PathBuf {
        self.home.clone()
    }

    pub(crate) fn test_live_path(&self) -> PathBuf {
        self.live_path()
    }

    pub(crate) fn test_store_get(&self, account_id: &str) -> Option<String> {
        self.store
            .get(self.runtime.id(), account_id, self.runtime.store_field())
            .ok()
            .flatten()
    }

    pub(crate) fn test_runtime(&self) -> &A {
        self.runtime.as_ref()
    }

    pub(crate) fn test_registry_upsert(&self, account: Account) -> Result<()> {
        self.registry.upsert(account)
    }

    pub(crate) fn test_list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(self.runtime.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use kimi_switch_core::error::{QuotaFailureKind, ReauthError};
    use kimi_switch_core::FileStore;

    #[derive(Clone, Copy)]
    enum RefreshBehavior {
        Unsupported,
        Rotate,
        Dead,
    }

    struct FakeRuntime {
        home: PathBuf,
        refresh_behavior: RefreshBehavior,
        refresh_calls: AtomicUsize,
        last_access_token: Mutex<Option<String>>,
        legacy_blob: Option<String>,
        /// 默认 "dedup_key"；部分测试覆盖成非默认名，模拟 Beta 把去重键落在
        /// 一个迁移前就存在、不叫 "dedup_key" 的字段名下。
        dedup_extra_key: &'static str,
        /// 主动续期门（issue #44）：默认 false = 开关关闭/有锁平台，当前账号不轮换。
        proactive_wanted: bool,
        /// 模拟官方在判定期间抢先改写 live：wanted 判定时把 live 改成这份 blob。
        official_live_during_wanted: Option<String>,
        /// 这些 uid 的 refresh 一律 DeadToken，便于同一 runtime 里混测「当前号轮换 / 停泊死号」。
        dead_uids: HashSet<String>,
    }

    impl FakeRuntime {
        fn new(home: PathBuf) -> Self {
            Self::with_behavior(home, RefreshBehavior::Unsupported)
        }

        fn with_behavior(home: PathBuf, refresh_behavior: RefreshBehavior) -> Self {
            Self {
                home,
                refresh_behavior,
                refresh_calls: AtomicUsize::new(0),
                last_access_token: Mutex::new(None),
                legacy_blob: None,
                dedup_extra_key: "dedup_key",
                proactive_wanted: false,
                official_live_during_wanted: None,
                dead_uids: HashSet::new(),
            }
        }

        fn with_proactive(home: PathBuf, refresh_behavior: RefreshBehavior) -> Self {
            Self {
                proactive_wanted: true,
                ..Self::with_behavior(home, refresh_behavior)
            }
        }

        fn with_legacy(home: PathBuf, legacy_blob: &str) -> Self {
            Self {
                legacy_blob: Some(legacy_blob.to_string()),
                ..Self::new(home)
            }
        }

        fn with_dedup_extra_key(home: PathBuf, dedup_extra_key: &'static str) -> Self {
            Self {
                dedup_extra_key,
                ..Self::new(home)
            }
        }
    }

    #[async_trait]
    impl FileBlobRuntime for FakeRuntime {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn home(&self) -> PathBuf {
            self.home.clone()
        }
        fn live_cred_path(&self, home: &Path) -> PathBuf {
            home.join("live.json")
        }
        fn dedup_extra_key(&self) -> &'static str {
            self.dedup_extra_key
        }
        fn parse_metadata(&self, blob: &str) -> BlobMetadata {
            let v: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
            BlobMetadata {
                primary_id: v.get("uid").and_then(|x| x.as_str()).map(String::from),
                // "label" 字段独立可控，缺省时回退到 uid（沿用既有测试的默认行为）；
                // "dedup" 字段模拟去重键（如某些客户端的 account id 字段）。
                label: v
                    .get("label")
                    .and_then(|x| x.as_str())
                    .map(String::from)
                    .or_else(|| v.get("uid").and_then(|x| x.as_str()).map(String::from)),
                dedup_key: v.get("dedup").and_then(|x| x.as_str()).map(String::from),
                extra: serde_json::Map::new(),
            }
        }
        async fn refresh(&self, blob: &str) -> Result<RefreshOutcome> {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            let uid = serde_json::from_str::<serde_json::Value>(blob)
                .ok()
                .and_then(|v| v.get("uid")?.as_str().map(str::to_string));
            if uid.as_deref().is_some_and(|id| self.dead_uids.contains(id)) {
                return Ok(RefreshOutcome::DeadToken);
            }
            Ok(match self.refresh_behavior {
                RefreshBehavior::Unsupported => RefreshOutcome::Unsupported,
                RefreshBehavior::Dead => RefreshOutcome::DeadToken,
                RefreshBehavior::Rotate => {
                    let v: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
                    let uid = v.get("uid").and_then(|x| x.as_str()).unwrap_or_default();
                    RefreshOutcome::Rotated(format!(
                        r#"{{"uid":"{uid}","access_token":"ROTATED"}}"#
                    ))
                }
            })
        }
        async fn fetch_quota(&self, access_token: &str, _account: &Account) -> Result<Vec<Quota>> {
            *self.last_access_token.lock().unwrap() = Some(access_token.to_string());
            Ok(vec![])
        }
        fn recover_legacy(&self, _home: &Path, _account: &Account) -> Option<String> {
            self.legacy_blob.clone()
        }

        async fn proactive_renewal_wanted(&self, _blob: &str) -> bool {
            if let Some(blob) = &self.official_live_during_wanted {
                // 模拟官方 100ms 采纳：判定期间 live 已被官方改写成新凭证。
                fs::write(self.live_cred_path(&self.home), blob).unwrap();
            }
            self.proactive_wanted
        }

        // 测试版 DeadToken 纪律：指纹文件 = home/fake-dead-refresh，一行一个 refresh
        // token 原文（测试不需要哈希）；旧版整文件一个 token 读取时仍算成员。
        fn proactive_renewal_blocked(&self, home: &Path, blob: &str) -> bool {
            let Some(refresh) = extract_refresh_token(blob) else {
                return false;
            };
            load_fake_dead_refresh(home).contains(&refresh)
        }

        fn proactive_renewal_reject(&self, home: &Path, blob: &str) -> Result<Error> {
            if let Some(refresh) = extract_refresh_token(blob) {
                let mut fingerprints = load_fake_dead_refresh(home);
                fingerprints.insert(refresh);
                persist_fake_dead_refresh(home, &fingerprints)?;
            }
            Ok(Error::quota_credential_expired("凭证已过期（fake 引导）"))
        }

        fn proactive_renewal_rotated(&self, home: &Path, blob: &str) -> Result<()> {
            let Some(refresh) = extract_refresh_token(blob) else {
                return Ok(());
            };
            let mut fingerprints = load_fake_dead_refresh(home);
            if fingerprints.remove(&refresh) {
                persist_fake_dead_refresh(home, &fingerprints)?;
            }
            Ok(())
        }
    }

    fn fake_dead_refresh_path(home: &Path) -> PathBuf {
        home.join("fake-dead-refresh")
    }

    fn load_fake_dead_refresh(home: &Path) -> HashSet<String> {
        match fs::read_to_string(fake_dead_refresh_path(home)) {
            Ok(recorded) => recorded
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
            Err(_) => HashSet::new(),
        }
    }

    fn persist_fake_dead_refresh(home: &Path, fingerprints: &HashSet<String>) -> Result<()> {
        let path = fake_dead_refresh_path(home);
        if fingerprints.is_empty() {
            match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        } else {
            let mut lines: Vec<&str> = fingerprints.iter().map(String::as_str).collect();
            lines.sort_unstable();
            fs::write(&path, lines.join("\n"))?;
            Ok(())
        }
    }

    /// 包一层 `CredentialStore`，`get` 恒定失败，模拟 keyring 后端故障；`set`/`delete` 照常转发到内层
    /// `FileStore`。用于验证读 blob 的错误处理顺序：store 错误不应抢先冒泡，必须先尝试 `recover_legacy`。
    struct FailingGetStore {
        inner: FileStore,
    }

    impl kimi_switch_core::CredentialStore for FailingGetStore {
        fn set(&self, provider: &str, account: &str, field: &str, value: &str) -> Result<()> {
            self.inner.set(provider, account, field, value)
        }
        fn get(&self, _provider: &str, _account: &str, _field: &str) -> Result<Option<String>> {
            Err(Error::Credential("simulated store failure".into()))
        }
        fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()> {
            self.inner.delete(provider, account, field)
        }
    }

    fn provider(tmp: &Path) -> FileBlobProvider<FakeRuntime> {
        provider_with(tmp, FakeRuntime::new(tmp.join("home")))
    }

    fn provider_with(tmp: &Path, runtime: FakeRuntime) -> FileBlobProvider<FakeRuntime> {
        let store = Arc::new(FileStore::new(tmp.join("creds.json")));
        let registry = Arc::new(AccountRegistry::new(tmp.join("registry.toml")));
        FileBlobProvider::new(runtime, store, registry)
    }

    fn fake_account(id: &str) -> Account {
        Account {
            provider: "fake".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active: false,
            created_at: chrono::Utc::now(),
            last_used_at: None,
            extra: serde_json::Map::new(),
        }
    }

    // --- capture-on-leave 守卫三态 ---

    #[test]
    fn capture_skips_when_live_missing_refresh_but_store_has_it() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();
        // 注册 owner + store 有 refresh。
        p.import_raw(
            r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#.into(),
            None,
            Some(false),
        )
        .unwrap();
        // live 缺 refresh。
        fs::write(p.test_live_path(), r#"{"uid":"u1","access_token":"A2"}"#).unwrap();
        p.reconcile_active_from_live().unwrap();
        let stored = p.test_store_get("u1").unwrap();
        assert!(stored.contains("R1"), "store 应保留带 refresh 的旧副本");
    }

    #[test]
    fn capture_overwrites_when_live_has_refresh_and_store_lacks_it() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();
        p.import_raw(
            r#"{"uid":"u1","access_token":"A1"}"#.into(),
            None,
            Some(false),
        )
        .unwrap();
        let live = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2"}"#;
        fs::write(p.test_live_path(), live).unwrap();
        p.reconcile_active_from_live().unwrap();
        let stored = p.test_store_get("u1").unwrap();
        assert_eq!(stored, live, "live 带 refresh 时应正常覆盖 store");
    }

    #[test]
    fn capture_overwrites_when_neither_side_has_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();
        p.import_raw(
            r#"{"uid":"u1","access_token":"A1"}"#.into(),
            None,
            Some(false),
        )
        .unwrap();
        let live = r#"{"uid":"u1","access_token":"A2"}"#;
        fs::write(p.test_live_path(), live).unwrap();
        p.reconcile_active_from_live().unwrap();
        let stored = p.test_store_get("u1").unwrap();
        assert_eq!(stored, live, "两边都没有 refresh 时维持原行为正常覆盖");
    }

    #[test]
    fn capture_skips_when_no_owner_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), r#"{"uid":"unmanaged"}"#).unwrap();
        // 无匹配账号 → 不写 store。
        p.reconcile_active_from_live().unwrap();
        assert!(p.test_store_get("unmanaged").is_none());
    }

    // --- Finding 1 回归：账号匹配不应比迁移前的 Beta 实现窄 ---

    /// 回归测试：迁移前已存在的账号（`extra` 只有旧键名 "legacy_account_id"，没有通用
    /// "dedup_key"）在 primary_id 发生轮换（如 Beta account_key 轮换）后，
    /// capture-on-leave 仍必须能靠去重键（用 runtime 声明的 extra 键名读取）找到 owner，
    /// 否则该账号的 store 副本会停留在旧 refresh token 上，导致下次切回后报
    /// "refresh token already used"。
    ///
    /// 修复前：引擎硬编码只读 `extra["dedup_key"]`，这个键名对该账号不存在 → 找不到 owner → 断言失败。
    #[test]
    fn capture_finds_owner_via_dedup_key_under_runtime_specific_extra_key_name() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime =
            FakeRuntime::with_dedup_extra_key(tmp.path().join("home"), "legacy_account_id");
        let p = provider_with(tmp.path(), runtime);
        fs::create_dir_all(p.test_home()).unwrap();

        // 模拟迁移前已存在的账号：id = 旧 account_key，label = 邮箱，
        // extra 只有旧键名 "legacy_account_id"，没有 "dedup_key"。
        let mut account = fake_account("old-key");
        account.label = "user@example.com".into();
        account.extra.insert(
            "legacy_account_id".into(),
            serde_json::Value::String("stable-id".into()),
        );
        p.test_registry_upsert(account).unwrap();

        // live blob：account_key 已轮换成新值，label 缺失，但去重键（dedup）仍是同一个稳定值。
        fs::write(
            p.test_live_path(),
            r#"{"uid":"new-key","dedup":"stable-id"}"#,
        )
        .unwrap();
        p.reconcile_active_from_live().unwrap();

        let stored = p
            .test_store_get("old-key")
            .expect("primary_id 轮换后仍应通过去重键找到 owner 并回灌 store");
        assert!(stored.contains("new-key"));
    }

    /// 回归测试：metadata 的 label 字段应对照 account.label 比较（而不是只对照 account.id），
    /// 与迁移前 `string_matches_account` 检查 `account.id.0` 或 `account.label` 的范围一致。
    ///
    /// 修复前：`id_hit` 只把 `meta.label` 与 `account.id` 比较，从不查 `account.label` → 断言失败。
    #[test]
    fn label_metadata_field_matches_account_label_when_primary_id_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let mut account = fake_account("some-id");
        account.label = "user@example.com".into();
        p.test_registry_upsert(account).unwrap();

        // primary_id 与 account.id/account.label 都不命中，但 label 字段命中 account.label。
        fs::write(
            p.test_live_path(),
            r#"{"uid":"unrelated-id","label":"user@example.com"}"#,
        )
        .unwrap();
        p.reconcile_active_from_live().unwrap();

        let stored = p
            .test_store_get("some-id")
            .expect("metadata.label 命中 account.label 时应能找到 owner");
        assert!(stored.contains("unrelated-id"));
    }

    // --- 导入 ---

    #[test]
    fn import_raw_stores_blob_and_marks_active() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        let account = p
            .import_raw(
                r#"{"uid":"u1","access_token":"A1"}"#.into(),
                None,
                Some(true),
            )
            .unwrap();
        assert_eq!(account.id.0, "u1");
        assert!(account.active);
        assert!(p.test_store_get("u1").unwrap().contains("A1"));
    }

    // --- 读 blob：store 错误处理顺序（不抢先冒泡，先试 legacy）---

    #[tokio::test]
    async fn query_quota_falls_back_to_legacy_when_store_get_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_legacy(
            tmp.path().join("home"),
            r#"{"uid":"u1","access_token":"FROM_LEGACY"}"#,
        );
        let store = Arc::new(FailingGetStore {
            inner: FileStore::new(tmp.path().join("creds.json")),
        });
        let registry = Arc::new(AccountRegistry::new(tmp.path().join("registry.toml")));
        let p = FileBlobProvider::new(runtime, store, registry);
        fs::create_dir_all(p.test_home()).unwrap();

        let account = fake_account("u1");
        p.test_registry_upsert(account.clone()).unwrap();
        p.query_quota(&account.id).await.unwrap();
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("FROM_LEGACY"),
            "store.get 失败时应继续尝试 recover_legacy 而不是直接冒泡错误"
        );
        // legacy 命中后应顺手修复 store 副本（即使底层 get 会失败，set 走 FileStore 正常写入）。
        assert!(std::fs::read_to_string(tmp.path().join("creds.json"))
            .unwrap()
            .contains("FROM_LEGACY"));
    }

    #[tokio::test]
    async fn query_quota_surfaces_store_error_when_legacy_also_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::new(tmp.path().join("home")); // legacy_blob = None
        let store = Arc::new(FailingGetStore {
            inner: FileStore::new(tmp.path().join("creds.json")),
        });
        let registry = Arc::new(AccountRegistry::new(tmp.path().join("registry.toml")));
        let p = FileBlobProvider::new(runtime, store, registry);
        fs::create_dir_all(p.test_home()).unwrap();

        let account = fake_account("u1");
        p.test_registry_upsert(account.clone()).unwrap();
        let err = p.query_quota(&account.id).await.unwrap_err();
        assert!(
            err.to_string().contains("simulated store failure"),
            "legacy 也拿不到时应把原始 store 错误抛出去，而不是泛化成\"未找到凭证\"：{err}"
        );
    }

    // --- query_quota：parked-only 刷新 ---

    // --- 按卡重新授权原语 replace_account_blob：只断言库存内容、live 内容与成功/失败 ---

    /// 对上且非当前账号：库存 blob 被新凭证替换，别名与单卡保持，live 登录文件不动。
    #[test]
    fn reauth_replaces_parked_account_without_touching_live() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let old_u1 = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let new_u1 = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2"}"#;
        let u2_live = r#"{"uid":"u2","refresh_token":"RX","access_token":"AX"}"#;

        p.import_raw(old_u1.into(), Some("工作号".into()), Some(false))
            .unwrap();
        p.import_raw(u2_live.into(), None, Some(true)).unwrap();
        fs::write(p.test_live_path(), u2_live).unwrap();

        let account = p
            .replace_account_blob(&AccountId("u1".into()), new_u1.into())
            .unwrap();

        assert_eq!(
            p.test_store_get("u1").unwrap(),
            new_u1,
            "非当前账号应只替换库存 blob"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            u2_live,
            "目标不是当前账号时不得改 live 登录文件"
        );
        assert_eq!(account.label, "工作号", "重授权后应保留原别名");
        assert!(!account.active, "非当前账号的 active 标记不得被改");

        let ids: Vec<String> = p
            .test_list_accounts()
            .unwrap()
            .into_iter()
            .map(|a| a.id.0)
            .collect();
        assert_eq!(
            ids.len(),
            2,
            "同一官方身份仍是一条，不得新增重复卡：{ids:?}"
        );
    }

    /// 对上且是当前账号：库存与 live 都换成新凭证；严禁先把旧 live 回灌库存
    /// （任何把旧 live 写回 store 的顺序错误都会让 store 断言失败）。
    #[test]
    fn reauth_replaces_active_account_and_writes_new_blob_to_live() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let old_u1 = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let new_u1 = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2"}"#;

        p.import_raw(old_u1.into(), None, Some(true)).unwrap();
        fs::write(p.test_live_path(), old_u1).unwrap();

        let account = p
            .replace_account_blob(&AccountId("u1".into()), new_u1.into())
            .unwrap();

        assert_eq!(
            p.test_store_get("u1").unwrap(),
            new_u1,
            "库存必须是新凭证；若旧 live 被误回灌则这里仍是 R1"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            new_u1,
            "当前账号的新凭证必须原子写入 Kimi Code 正在使用的登录文件"
        );
        assert!(account.active, "当前账号身份保持不变");
        let accounts = p.test_list_accounts().unwrap();
        assert_eq!(accounts.len(), 1, "同一官方身份仍是一条：{accounts:?}");
    }

    /// 重授权当前账号时 live 原子写失败：错误文案必须明示状态与补救——新凭证已保存到
    /// 账号库、live 仍是旧凭证、尽快重试重新授权——并带上底层 io 原因；
    /// 库存已落新凭证，live 保持旧内容不被破坏。
    #[test]
    fn reauth_active_live_write_failure_reports_state_and_keeps_store_new() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let old_u1 = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let new_u1 = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2"}"#;

        p.import_raw(old_u1.into(), None, Some(true)).unwrap();
        fs::write(p.test_live_path(), old_u1).unwrap();

        // 注入 live 写失败：把 write_blob 的 tmp 目标预制为目录，迫使 fs::write 失败
        // （tmp 命名与 write_blob 内部一致；live 文件本身保持旧内容）。
        let tmp_target = p
            .test_live_path()
            .with_extension(format!("tmp.{}", std::process::id()));
        fs::create_dir_all(&tmp_target).unwrap();

        let err = p
            .replace_account_blob(&AccountId("u1".into()), new_u1.into())
            .unwrap_err();
        match err.reauth_error() {
            Some(ReauthError::LiveWriteFailed { cause }) => {
                assert!(
                    cause.contains("os error"),
                    "LiveWriteFailed 应带上底层 io 原因：{cause}"
                );
            }
            other => panic!("live 写失败应透出 LiveWriteFailed，得到 {other:?} / {err}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("已保存到账号库"),
            "错误必须说明新凭证已保存到账号库：{msg}"
        );
        assert!(
            msg.contains("旧凭证"),
            "错误必须说明当前仍在使用旧凭证：{msg}"
        );
        assert!(
            msg.contains("重试重新授权"),
            "错误必须给出补救动作（重试重新授权）：{msg}"
        );
        assert!(msg.contains("os error"), "错误必须带上底层 io 原因：{msg}");

        assert_eq!(
            p.test_store_get("u1").unwrap(),
            new_u1,
            "live 写失败时新凭证必须已经落库"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            old_u1,
            "live 原子写失败不得破坏旧登录文件"
        );
    }

    /// 钉住已接受行为（ADR-0002 修订）：重授权 live 写失败留下 store=新/live=旧 窗口后，
    /// 其后一次切换的 capture-on-leave 会把旧 live 回灌库存、静默撤销新凭证——这是明文
    /// 接受的对外行为（对外一律以 live 为准），由本测试钉住防止未来无意改变；
    /// 若要改变，先重开 ADR-0002 决策。
    #[tokio::test]
    async fn reauth_live_write_failure_window_then_activate_backfills_old_live_into_store() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let old_u1 = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let new_u1 = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2"}"#;
        let u2_blob = r#"{"uid":"u2","refresh_token":"RX","access_token":"AX"}"#;

        p.import_raw(old_u1.into(), None, Some(true)).unwrap();
        p.import_raw(u2_blob.into(), None, Some(false)).unwrap();
        fs::write(p.test_live_path(), old_u1).unwrap();

        // 注入 live 写失败建立窗口（同上：预制 tmp 目标为目录）。
        let tmp_target = p
            .test_live_path()
            .with_extension(format!("tmp.{}", std::process::id()));
        fs::create_dir_all(&tmp_target).unwrap();
        assert!(p
            .replace_account_blob(&AccountId("u1".into()), new_u1.into())
            .is_err());
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            new_u1,
            "窗口成立：store 已是新凭证而 live 仍是旧凭证"
        );

        // 解除注入后切换到 u2：capture-on-leave 读到的仍是旧 live（u1）。
        fs::remove_dir_all(&tmp_target).unwrap();
        p.activate(&AccountId("u2".into())).await.unwrap();

        assert_eq!(
            p.test_store_get("u1").unwrap(),
            old_u1,
            "钉住已接受行为（ADR-0002 修订）：窗口内切换会把旧 live 回灌库存、撤销新凭证"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            u2_blob,
            "切换后 live 应为 u2 的 blob"
        );
    }

    /// 钉住已接受行为（ADR-0002 修订）：查询路径与切换路径一样会把旧 live 回灌库存。
    /// 重授权 live 写失败留下 store=新/live=旧 窗口后，对活跃账号一次额度查询
    /// 即把旧 live 回写 store、静默撤销新凭证——这是明文
    /// 接受的对外行为（窗口期一律以 live 为准），由本测试钉住防止未来无意改变；
    /// 若要改变，先重开 ADR-0002 决策。
    #[tokio::test]
    async fn reauth_live_write_failure_window_then_query_quota_backfills_old_live_into_store() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let old_u1 = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let new_u1 = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2"}"#;

        p.import_raw(old_u1.into(), None, Some(true)).unwrap();
        fs::write(p.test_live_path(), old_u1).unwrap();

        // 注入 live 写失败建立窗口（同上：预制 tmp 目标为目录）。
        let tmp_target = p
            .test_live_path()
            .with_extension(format!("tmp.{}", std::process::id()));
        fs::create_dir_all(&tmp_target).unwrap();
        assert!(p
            .replace_account_blob(&AccountId("u1".into()), new_u1.into())
            .is_err());
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            new_u1,
            "窗口成立：store 已是新凭证而 live 仍是旧凭证"
        );

        // 解除注入后对活跃账号查一次额度（FakeRuntime 无网络）。
        fs::remove_dir_all(&tmp_target).unwrap();
        p.query_quota(&AccountId("u1".into())).await.unwrap();

        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("A1"),
            "钉住已接受行为（ADR-0002 修订）：窗口内查询以 live 为准，用旧凭证"
        );
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            old_u1,
            "钉住已接受行为（ADR-0002 修订）：查询路径与切换路径一样会把旧 live 回灌库存、撤销新凭证"
        );
    }

    /// 官方身份辨认只有一处：`primary_id` 缺失时回退 `label`。
    /// 导入、切换（live_account_id）、重新授权共用同一回退顺序。
    #[test]
    fn official_identity_falls_back_to_label_on_import_swap_and_reauth() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let labeled = r#"{"label":"only-label","refresh_token":"R1","access_token":"A1"}"#;
        let account = p
            .import_raw(labeled.into(), None, Some(true))
            .expect("导入应能用 label 回退辨认官方身份");
        assert_eq!(account.id.0, "only-label");

        fs::write(p.test_live_path(), labeled).unwrap();
        assert_eq!(
            p.live_account_id().unwrap().0,
            "only-label",
            "切换路径辨认 live 官方身份应与导入同一回退"
        );

        let refreshed = r#"{"label":"only-label","refresh_token":"R2","access_token":"A2"}"#;
        p.replace_account_blob(&account.id, refreshed.into())
            .unwrap();
        assert_eq!(p.test_store_get("only-label").unwrap(), refreshed);

        let other = r#"{"label":"other-id","refresh_token":"R9","access_token":"A9"}"#;
        let err = p
            .replace_account_blob(&account.id, other.into())
            .unwrap_err();
        match err.reauth_error() {
            Some(ReauthError::IdentityMismatch { observed, target }) => {
                assert_eq!(observed, "other-id");
                assert_eq!(target, "only-label");
            }
            other => panic!("期望 IdentityMismatch，得到 {other:?} / {err}"),
        }
    }

    /// primary_id 在时不回退 label：三条路径都以 primary_id 为准。
    #[test]
    fn official_identity_prefers_primary_id_over_label() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let raw = r#"{"uid":"u-primary","label":"u-label","access_token":"A1"}"#;
        let account = p.import_raw(raw.into(), None, Some(true)).unwrap();
        assert_eq!(account.id.0, "u-primary");

        fs::write(p.test_live_path(), raw).unwrap();
        assert_eq!(p.live_account_id().unwrap().0, "u-primary");

        let err = p
            .replace_account_blob(&AccountId("u-label".into()), raw.into())
            .unwrap_err();
        match err.reauth_error() {
            Some(ReauthError::IdentityMismatch { observed, target }) => {
                assert_eq!(observed, "u-primary");
                assert_eq!(target, "u-label");
            }
            other => panic!("期望 IdentityMismatch，得到 {other:?} / {err}"),
        }
        assert!(p.test_store_get("u-label").is_none());
        assert_eq!(p.test_store_get("u-primary").unwrap(), raw);
    }

    /// 对不上（登录的是另一个官方身份）：失败，库存/live/账号列表一概不写。
    #[test]
    fn reauth_rejects_identity_mismatch_without_any_write() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let old_u1 = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let other = r#"{"uid":"u9","refresh_token":"R9","access_token":"A9"}"#;

        p.import_raw(old_u1.into(), None, Some(false)).unwrap();

        let err = p
            .replace_account_blob(&AccountId("u1".into()), other.into())
            .unwrap_err();
        match err.reauth_error() {
            Some(ReauthError::IdentityMismatch { observed, target }) => {
                assert_eq!(observed, "u9");
                assert_eq!(target, "u1");
            }
            other => panic!("身份不符应透出 IdentityMismatch，得到 {other:?} / {err}"),
        }
        assert!(
            err.to_string().contains("does not match"),
            "错误应说明官方身份对不上：{err}"
        );

        assert_eq!(p.test_store_get("u1").unwrap(), old_u1, "库存不得被改");
        assert!(p.test_store_get("u9").is_none(), "陌生身份不得被写入库存");
        assert!(!p.test_live_path().exists(), "失败路径不得写 live 登录文件");
        let accounts = p.test_list_accounts().unwrap();
        assert_eq!(accounts.len(), 1, "不得新增账号：{accounts:?}");
    }

    /// 凭证无法辨认官方 id（解析不出主键）：同样失败且不写任何地方。
    #[tokio::test]
    async fn reauth_rejects_unidentifiable_credentials_without_any_write() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let old_u1 = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        p.import_raw(old_u1.into(), None, Some(false)).unwrap();

        for bad in ["{}", "not-json"] {
            let err = p
                .replace_account_blob(&AccountId("u1".into()), bad.into())
                .unwrap_err();
            assert!(
                matches!(err.reauth_error(), Some(ReauthError::Unidentifiable)),
                "无法辨认应透出 Unidentifiable：{err}"
            );
            assert!(
                err.to_string().contains("cannot identify"),
                "无法辨认时应明确报错而不是改库：{err}"
            );
        }

        assert_eq!(
            p.test_store_get("u1").unwrap(),
            old_u1,
            "不可辨认 → 失败且不写库存"
        );
        assert!(!p.test_live_path().exists(), "不可辨认 → 不写 live");
    }

    #[tokio::test]
    async fn query_quota_skips_refresh_for_active_account() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let account = p
            .import_raw(
                r#"{"uid":"u1","access_token":"A1"}"#.into(),
                None,
                Some(true),
            )
            .unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("A1")
        );
    }

    #[tokio::test]
    async fn query_quota_refreshes_parked_account_and_persists_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let account = p
            .import_raw(
                r#"{"uid":"u1","access_token":"OLD"}"#.into(),
                None,
                Some(false),
            )
            .unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("ROTATED")
        );
        let stored = p.test_store_get("u1").unwrap();
        assert!(stored.contains("ROTATED"), "轮换后的 blob 应写回 store");
    }

    #[tokio::test]
    async fn query_quota_dead_token_leaves_store_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Dead);
        let p = provider_with(tmp.path(), runtime);
        let account = p
            .import_raw(
                r#"{"uid":"u1","access_token":"OLD"}"#.into(),
                None,
                Some(false),
            )
            .unwrap();

        // DeadToken 时引擎放弃刷新，继续用旧 access_token 查询（不会因刷新失败而报错）。
        p.query_quota(&account.id).await.unwrap();

        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("OLD")
        );
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            r#"{"uid":"u1","access_token":"OLD"}"#
        );
    }

    /// 停泊 DeadToken（issue #80）：被拒后记录指纹并停止重试，但本次及后续查询仍用旧 access token
    /// （与当前账号主动续期「查询按凭证过期呈现」不同）。
    #[tokio::test]
    async fn query_quota_parked_dead_token_records_fingerprint_and_stops_retrying() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Dead);
        let p = provider_with(tmp.path(), runtime);
        let old = r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD"}"#;
        let account = p.import_raw(old.into(), None, Some(false)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read_to_string(p.test_home().join("fake-dead-refresh")).unwrap(),
            "R1",
            "被拒 refresh token 的指纹必须记录在案"
        );
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("OLD"),
            "DeadToken 后仍用旧 access token 查询"
        );
        assert_eq!(p.test_store_get("u1").unwrap(), old, "账号库凭证保持不动");

        // 停止重试：再次查询不再发起 refresh，仍用旧 token 查询。
        p.query_quota(&account.id).await.unwrap();
        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            1,
            "指纹在案必须停止重试"
        );
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("OLD"),
            "停试后查询仍须发生且仍用旧 access token"
        );
    }

    /// 两停泊号先后 DeadToken：两者都停试；后写不得覆盖前一个指纹。
    #[tokio::test]
    async fn query_quota_two_parked_dead_tokens_both_stop_retrying() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Dead);
        let p = provider_with(tmp.path(), runtime);
        let blob1 = r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD1"}"#;
        let blob2 = r#"{"uid":"u2","refresh_token":"R2","access_token":"OLD2"}"#;
        let a1 = p.import_raw(blob1.into(), None, Some(false)).unwrap();
        let a2 = p.import_raw(blob2.into(), None, Some(false)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();

        p.query_quota(&a1.id).await.unwrap();
        p.query_quota(&a2.id).await.unwrap();
        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 2);

        let recorded = fs::read_to_string(p.test_home().join("fake-dead-refresh")).unwrap();
        let members: HashSet<&str> = recorded
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        assert!(
            members.contains("R1") && members.contains("R2"),
            "后写不得覆盖前一个指纹，两停泊号都应在案：{recorded}"
        );

        p.query_quota(&a1.id).await.unwrap();
        p.query_quota(&a2.id).await.unwrap();
        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            2,
            "两停泊死号都必须停试，不得因后写覆盖而再次 refresh"
        );
        assert_eq!(p.test_store_get("u1").unwrap(), blob1);
        assert_eq!(p.test_store_get("u2").unwrap(), blob2);
    }

    /// 当前账号轮换成功后，另一停泊死号不得再 refresh（不得 unlink 整份指纹文件）。
    #[tokio::test]
    async fn proactive_renewal_success_does_not_retry_other_parked_dead_token() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime =
            FakeRuntime::with_proactive(tmp.path().join("home"), RefreshBehavior::Rotate);
        runtime.dead_uids.insert("u2".into());
        let p = provider_with(tmp.path(), runtime);
        let active_blob = r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD"}"#;
        let parked_blob = r#"{"uid":"u2","refresh_token":"R2","access_token":"PARKED"}"#;
        let active = p.import_raw(active_blob.into(), None, Some(true)).unwrap();
        let parked = p.import_raw(parked_blob.into(), None, Some(false)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), active_blob).unwrap();

        p.query_quota(&parked.id).await.unwrap();
        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 1);
        let recorded = fs::read_to_string(p.test_home().join("fake-dead-refresh")).unwrap();
        assert!(
            recorded.lines().any(|line| line.trim() == "R2"),
            "停泊死号指纹必须在案：{recorded}"
        );

        p.query_quota(&active.id).await.unwrap();
        let recorded = fs::read_to_string(p.test_home().join("fake-dead-refresh")).unwrap();
        assert!(
            recorded.lines().any(|line| line.trim() == "R2"),
            "当前账号轮换成功不得清掉其他停泊死号指纹：{recorded}"
        );

        p.query_quota(&parked.id).await.unwrap();
        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            2,
            "当前账号轮换成功后，已死停泊号不得再 refresh（一次停泊 DeadToken + 一次当前号轮换）"
        );
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("PARKED"),
            "停泊死号后续查询仍用旧 access token"
        );
    }

    // --- query_quota：当前账号主动续期（issue #44，ADR-0003 第 2 条）---

    /// 三分支之「轮换成功」：runtime 判定临期 → 查询前先轮换，新 blob 先落库再写回
    /// live，查询使用轮换后的 access token，写回后账号库与 live 一致。
    #[tokio::test]
    async fn proactive_renewal_rotates_before_query_and_writes_back() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_proactive(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let old = r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD"}"#;
        let account = p.import_raw(old.into(), None, Some(true)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), old).unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("ROTATED"),
            "查询必须使用轮换后的 access token"
        );
        let rotated = r#"{"uid":"u1","access_token":"ROTATED"}"#;
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            rotated,
            "新凭证必须落账号库"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            rotated,
            "新凭证必须写回 live，写回后账号库与 live 一致"
        );
    }

    /// ADR-0002 写序钉住「先库后 live」：live 原子写失败时，账号库必须已是新凭证、
    /// live 保持旧凭证不被破坏（注入手法同重授权 live 写失败测试）。
    ///
    /// 注意此窗口**不可恢复**（如实收录，消除「可恢复」误导）：live 写失败后，下一次
    /// 当前账号查询会经 `raw_blob_for_account` 把旧 live 无条件回灌库槽位，新轮换
    /// refresh token 的唯一副本即被销毁；此后轮换必被官方 invalid_grant 拒绝 →
    /// 记指纹停试 → 收敛到凭证过期型 401，需用户按引导重新授权。该语义是 ADR-0002
    /// 修订「窗口期一律以 live 为准」的明文接受行为；本测试只钉失败瞬间的落库/落盘
    /// 状态，不暗示任何可恢复性。
    #[tokio::test]
    async fn proactive_renewal_live_write_failure_leaves_store_new_and_live_old() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_proactive(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let old = r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD"}"#;
        let account = p.import_raw(old.into(), None, Some(true)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), old).unwrap();

        // 注入 live 写失败：把 write_blob 的 tmp 目标预制为目录，迫使 fs::write 失败。
        let tmp_target = p
            .test_live_path()
            .with_extension(format!("tmp.{}", std::process::id()));
        fs::create_dir_all(&tmp_target).unwrap();

        p.query_quota(&account.id).await.unwrap_err();

        assert_eq!(
            p.test_store_get("u1").unwrap(),
            r#"{"uid":"u1","access_token":"ROTATED"}"#,
            "先库后 live：live 写失败时账号库必须已是新凭证"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            old,
            "live 原子写失败不得破坏旧登录文件"
        );
    }

    /// 三分支之「被拒停试」：轮换被官方拒绝 → 记录指纹、停止重试、本地凭证不动、
    /// 本次查询按「凭证过期型 401」呈现；再次查询指纹在案 → 不再发起轮换。
    #[tokio::test]
    async fn proactive_renewal_dead_token_records_fingerprint_and_stops_retrying() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_proactive(tmp.path().join("home"), RefreshBehavior::Dead);
        let p = provider_with(tmp.path(), runtime);
        let old = r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD"}"#;
        let account = p.import_raw(old.into(), None, Some(true)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), old).unwrap();

        let err = p.query_quota(&account.id).await.unwrap_err();
        assert_eq!(
            err.quota_failure().map(|f| f.kind),
            Some(QuotaFailureKind::CredentialExpired),
            "被拒后本次查询按「凭证过期型 401」（issue #41）呈现"
        );
        assert_eq!(err.http_status(), Some(401));
        assert!(err.is_authentication_failure());
        assert!(!err.is_retryable());
        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read_to_string(p.test_home().join("fake-dead-refresh")).unwrap(),
            "R1",
            "被拒 refresh token 的指纹必须记录在案"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            old,
            "本地 live 凭证不删除不墓碑化"
        );
        assert_eq!(p.test_store_get("u1").unwrap(), old, "账号库凭证保持不动");

        // 停止重试：再次查询不再发起轮换请求，错误呈现不变。
        let err = p.query_quota(&account.id).await.unwrap_err();
        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            1,
            "指纹在案必须停止重试"
        );
        assert_eq!(
            err.quota_failure().map(|f| f.kind),
            Some(QuotaFailureKind::CredentialExpired)
        );
    }

    /// 三分支之「开关关闭」：wanted=false 时与现版本行为一致——不轮换，
    /// live 与账号库不动，用原 access token 查询。
    #[tokio::test]
    async fn proactive_renewal_off_behaves_like_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let old = r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD"}"#;
        let account = p.import_raw(old.into(), None, Some(true)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), old).unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("OLD")
        );
        assert_eq!(fs::read_to_string(p.test_live_path()).unwrap(), old);
        assert_eq!(p.test_store_get("u1").unwrap(), old);
    }

    /// 官方在判定期间抢先轮换：采纳最新 live（顺手回灌账号库保持真本新鲜），
    /// 本次不再消耗 refresh token 重复轮换。
    #[tokio::test]
    async fn proactive_renewal_adopts_live_when_official_rotated_first() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime =
            FakeRuntime::with_proactive(tmp.path().join("home"), RefreshBehavior::Rotate);
        let official = r#"{"uid":"u1","refresh_token":"R9","access_token":"OFFICIAL"}"#;
        runtime.official_live_during_wanted = Some(official.into());
        let p = provider_with(tmp.path(), runtime);
        let old = r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD"}"#;
        let account = p.import_raw(old.into(), None, Some(true)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), old).unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            0,
            "官方已轮换则不重复消耗 refresh token"
        );
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("OFFICIAL"),
            "查询改用官方刚轮换的最新凭证"
        );
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            official,
            "采纳后回灌账号库"
        );
        assert_eq!(fs::read_to_string(p.test_live_path()).unwrap(), official);
    }

    /// live 在判定期间改属其他账号：放弃主动续期——不写 live、不改账号库、不轮换。
    #[tokio::test]
    async fn proactive_renewal_aborts_when_live_switches_to_another_account() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime =
            FakeRuntime::with_proactive(tmp.path().join("home"), RefreshBehavior::Rotate);
        let other_live = r#"{"uid":"u2","refresh_token":"R2","access_token":"A2"}"#;
        runtime.official_live_during_wanted = Some(other_live.into());
        let p = provider_with(tmp.path(), runtime);
        let old = r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD"}"#;
        let account = p.import_raw(old.into(), None, Some(true)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), old).unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("OLD"),
            "沿用原凭证查询，成败由既有 401 协调恢复/归类路径兜底"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            other_live,
            "live 已改属其他账号时不得改写"
        );
        assert_eq!(p.test_store_get("u1").unwrap(), old, "账号库保持不动");
    }

    /// 轮换成功收尾：只清掉这次轮换前 blob 的指纹；其它账号的拒止指纹保留。
    #[tokio::test]
    async fn proactive_renewal_success_clears_only_rotated_blob_fingerprint() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_proactive(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let old = r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD"}"#;
        let account = p.import_raw(old.into(), None, Some(true)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), old).unwrap();
        // 另一停泊死号的指纹；不命中当前 R1，不阻塞本次轮换。
        fs::write(p.test_home().join("fake-dead-refresh"), "R0").unwrap();

        p.query_quota(&account.id).await.unwrap();

        let recorded = fs::read_to_string(p.test_home().join("fake-dead-refresh")).unwrap();
        assert!(
            recorded.lines().any(|line| line.trim() == "R0"),
            "当前账号轮换成功不得清掉其他拒止指纹：{recorded}"
        );
        assert!(
            !recorded.lines().any(|line| line.trim() == "R1"),
            "不得把当前号指纹误写入：{recorded}"
        );
    }

    /// issue #50（ADR-0004）：swap（activate）路径不触碰网页会员绑定状态——
    /// 网页凭证 field 与会员到期 extra 键只由「绑定网页登录」管道写入/清理，
    /// 切换只动 Code blob、live 与 active 标记。引擎本身零 HTTP，网页接口
    /// （www.kimi.com GetSubscription）不属于切换路径。
    #[tokio::test]
    async fn activate_preserves_web_membership_binding_state() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let store = FileStore::new(tmp.path().join("creds.json"));
        // 两个账号都带网页绑定（web field + extra 会员键），u1 为当前账号。
        let mut expected_extra = serde_json::Map::new();
        expected_extra.insert(
            "membership_current_end_time".into(),
            serde_json::Value::String("2027-08-28T00:00:00Z".into()),
        );
        expected_extra.insert(
            "membership_next_billing_time".into(),
            serde_json::Value::String("2027-08-27T01:25:37.930341Z".into()),
        );
        for (uid, active) in [("u1", true), ("u2", false)] {
            let mut account = fake_account(uid);
            account.active = active;
            account.extra = expected_extra.clone();
            p.test_registry_upsert(account).unwrap();
            store
                .set(
                    "fake",
                    uid,
                    "blob",
                    &format!(r#"{{"uid":"{uid}","access_token":"A1"}}"#),
                )
                .unwrap();
            store
                .set("fake", uid, "web", &format!("web-token-{uid}"))
                .unwrap();
        }
        let u1_live = r#"{"uid":"u1","access_token":"A1"}"#;
        fs::write(p.test_live_path(), u1_live).unwrap();

        p.activate(&AccountId("u2".into())).await.unwrap();

        for (uid, active) in [("u1", false), ("u2", true)] {
            let account = p
                .test_list_accounts()
                .unwrap()
                .into_iter()
                .find(|a| a.id.0 == uid)
                .unwrap();
            assert_eq!(account.active, active);
            assert_eq!(
                account.extra, expected_extra,
                "swap 不得改动会员到期 extra 键"
            );
            assert_eq!(
                store.get("fake", uid, "web").unwrap().as_deref(),
                Some(format!("web-token-{uid}").as_str()),
                "swap 不得改动网页凭证 field"
            );
        }
    }

    fn live_mtime(p: &FileBlobProvider<FakeRuntime>) -> DateTime<Utc> {
        DateTime::<Utc>::from(
            fs::metadata(p.test_live_path())
                .unwrap()
                .modified()
                .unwrap(),
        )
    }

    fn seed_active_and_parked(p: &FileBlobProvider<FakeRuntime>) -> (Account, Account) {
        let active = p
            .import_raw(
                r#"{"uid":"u1","access_token":"A1"}"#.into(),
                None,
                Some(true),
            )
            .unwrap();
        let parked = p
            .import_raw(
                r#"{"uid":"u2","access_token":"A2"}"#.into(),
                None,
                Some(false),
            )
            .unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), r#"{"uid":"u1","access_token":"A1"}"#).unwrap();
        (active, parked)
    }

    /// issue #103：当前账号当前登录文件 mtime 晚于失败时间 → 凭证恢复信号。
    #[test]
    fn credential_changed_since_active_when_live_mtime_after_failed_at() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        let (active, parked) = seed_active_and_parked(&p);
        let mtime = live_mtime(&p);
        let failed_at = mtime - chrono::Duration::seconds(90);

        assert!(
            p.credential_changed_since(&active.id, failed_at),
            "当前账号 live mtime 晚于失败时间应有凭证恢复信号"
        );
        assert!(
            !p.credential_changed_since(&parked.id, failed_at),
            "停泊账号恒无恢复信号"
        );
        assert!(
            !p.credential_changed_since(&AccountId("missing".into()), failed_at),
            "未知账号无恢复信号"
        );
    }

    /// issue #103：信号一次性——失败时间刷新到不早于同一 mtime 后不再放行。
    #[test]
    fn credential_changed_since_same_mtime_is_one_shot_after_failed_at_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        let (active, _) = seed_active_and_parked(&p);
        let mtime = live_mtime(&p);

        assert!(p.credential_changed_since(&active.id, mtime - chrono::Duration::seconds(1)));
        assert!(
            !p.credential_changed_since(&active.id, mtime),
            "同一 mtime 不得晚于刷新后的失败时间"
        );
        assert!(!p.credential_changed_since(&active.id, mtime + chrono::Duration::seconds(1)));
    }

    #[test]
    fn credential_changed_since_missing_live_is_false() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        let active = p
            .import_raw(
                r#"{"uid":"u1","access_token":"A1"}"#.into(),
                None,
                Some(true),
            )
            .unwrap();
        let failed_at = Utc::now() - chrono::Duration::seconds(90);
        assert!(!p.credential_changed_since(&active.id, failed_at));
    }
}
