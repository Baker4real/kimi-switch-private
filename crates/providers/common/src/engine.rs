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
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use kimi_switch_core::error::{Error, Result};
use kimi_switch_core::membership_expiry::{
    EXTRA_CURRENT_END_TIME, EXTRA_CURRENT_START_TIME, EXTRA_NEXT_BILLING_TIME,
};
use kimi_switch_core::swap::{swap_with_snapshot_protected, with_swap_lock, SwapTarget};
use kimi_switch_core::{
    Account, AccountId, AccountRegistry, ActivateOutcome, CredentialStore, Provider, Quota,
};

use crate::json::{extract_access_token, extract_refresh_token};
use crate::runtime::{BlobMetadata, CredentialExpiry, FileBlobRuntime, RefreshOutcome};

// 测试 seam（issue #153）：让下一次 `write_blob` 的 live fsync 失败（一次性），
// 覆盖「fsync 失败进入既有回滚路径而非静默成功」。
#[cfg(test)]
thread_local! {
    static FAIL_NEXT_LIVE_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn take_fail_next_live_sync() -> bool {
    FAIL_NEXT_LIVE_SYNC.with(|cell| cell.replace(false))
}

#[cfg(test)]
pub(crate) fn fail_next_live_sync() {
    FAIL_NEXT_LIVE_SYNC.with(|cell| cell.set(true));
}

// 测试 seam（ADR-0019）：activate 即将进入交换锁时，运行为该 provider home 登记的一次性
// 探针，供交错用例在「已调用 activate、尚未拿到交换锁」这一刻插入另一段持锁写入。activate
// 的阻塞部分跑在 spawn_blocking 线程上，线程本地注入不可达，所以按 home 登记在进程级表里
// （每个用例的 home 在各自 tempdir 下，并行用例互不串）。
#[cfg(test)]
type ActivateLockProbe = Box<dyn FnOnce() + Send>;

#[cfg(test)]
static ACTIVATE_LOCK_PROBES: std::sync::Mutex<Vec<(PathBuf, ActivateLockProbe)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn before_next_activate_lock(home: &Path, probe: impl FnOnce() + Send + 'static) {
    ACTIVATE_LOCK_PROBES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((home.to_path_buf(), Box::new(probe)));
}

#[cfg(test)]
fn run_activate_lock_probe(home: &Path) {
    let probe = {
        let mut probes = ACTIVATE_LOCK_PROBES
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let index = probes.iter().position(|(registered, _)| registered == home);
        index.map(|i| probes.remove(i).1)
    };
    if let Some(probe) = probe {
        probe();
    }
}

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

    /// 原子写 live 凭证：tmp + rename + 0o600 + 文件级 fsync（issue #153）。
    /// fsync 失败如实上抛：切换路径由快照回滚兜底，不静默成功。
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
        #[cfg(test)]
        if take_fail_next_live_sync() {
            return Err(Error::provider("injected live fsync failure"));
        }
        kimi_switch_core::swap::sync_file(path)?;
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
            // blob metadata 不含会员到期键；整表重建 extra 时把已有三键带回去，
            // 避免 GUI 导入 / CLI login 把绑定写入的到期日清掉（ADR-0004 修订）。
            for key in [
                EXTRA_CURRENT_START_TIME,
                EXTRA_CURRENT_END_TIME,
                EXTRA_NEXT_BILLING_TIME,
            ] {
                if extra.contains_key(key) {
                    continue;
                }
                if let Some(value) = ex.extra.get(key) {
                    extra.insert(key.to_string(), value.clone());
                }
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

    /// 当前 live 凭证对应的 registry id。启动一致性自检（issue #155）用它辨认
    /// live 身份并对照墓碑；身份口径 = `parse_metadata` → `official_identity`。
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
    /// 取账号 blob：active 账号优先读 live（并顺手回灌 store 副本），parked 读 store，最后 legacy。
    ///
    /// 「顺手回灌」是 ADR-0002 修订记录的第二条回灌路径，按回灌新旧比对写库存
    /// （[`Self::backfill_live_into_store_with`]，ADR-0019 决策 8）：重授权 live 写失败窗口内，
    /// 可比较时较旧的 live 不再撤销库存里的新凭证。读取本身仍返回 live，不主动改写它。
    ///
    /// 错误处理顺序与原始 Beta 实现一致：store 读取失败时不立即冒泡，而是先捕获错误、
    /// 继续尝试 `recover_legacy`；只有 legacy 也拿不到时才把原始 store 错误抛出去。这样即使
    /// keyring 后端本身故障，仍有机会从 legacy 布局恢复凭证。
    fn raw_blob_for_account(&self, account: &Account) -> Result<String> {
        Self::raw_blob_for_account_with(
            self.runtime.as_ref(),
            self.store.as_ref(),
            &self.home,
            account,
        )
    }

    /// [`Self::raw_blob_for_account`] 的借用参数版本，供 activate 在交换锁内（`spawn_blocking`
    /// 闭包里）读取目标凭证。
    fn raw_blob_for_account_with(
        runtime: &A,
        store: &dyn CredentialStore,
        home: &Path,
        account: &Account,
    ) -> Result<String> {
        if let Some(raw) = Self::read_live_if_matches_with(runtime, home, account) {
            // 回灌只是顺手保鲜：库写失败不影响读取。
            let _ = Self::backfill_live_into_store_with(runtime, store, &account.id, &raw);
            return Ok(raw);
        }
        let store_error =
            match store.get(runtime.id(), account.id.0.as_str(), runtime.store_field()) {
                Ok(Some(raw)) => return Ok(raw),
                Ok(None) => None,
                Err(e) => Some(e),
            };
        if let Some(raw) = runtime.recover_legacy(home, account) {
            let _ = store.set(
                runtime.id(),
                account.id.0.as_str(),
                runtime.store_field(),
                &raw,
            );
            return Ok(raw);
        }
        if let Some(e) = store_error {
            return Err(e);
        }
        Err(Error::Credential(format!(
            "no stored credentials for {}:{}; run `kimi-switch login {}` or re-import",
            runtime.id(),
            account.id,
            runtime.id()
        )))
    }

    fn read_live_if_matches(&self, account: &Account) -> Option<String> {
        Self::read_live_if_matches_with(self.runtime.as_ref(), &self.home, account)
    }

    fn read_live_if_matches_with(runtime: &A, home: &Path, account: &Account) -> Option<String> {
        let raw = fs::read_to_string(runtime.live_cred_path(home)).ok()?;
        let meta = runtime.parse_metadata(&raw);
        Self::metadata_matches(&meta, account, runtime.dedup_extra_key()).then_some(raw)
    }

    /// 写后回读身份校验（issue #154，spec #152 实现决策 6）：activate 把目标 blob
    /// 写进 live 并落盘后，立即重读 live、解析官方身份、与目标账号比对。不一致 →
    /// 返回错误，由 `swap_with_snapshot` 的既有 commit 失败路径回滚——此刻 registry
    /// 尚未提交，live 恢复切换前内容、registry 保持原状，对外语义「要么切对、
    /// 要么回到原状」，绝不停留在半切换中间态还报告成功。
    ///
    /// 比对口径与切换路径既有身份辨认同源（不另造第二套）：live 经
    /// `parse_metadata` 解析（对 Kimi 即 access_token JWT 的 `user_id` claim），
    /// 官方身份取 [`Self::official_identity`]（primary_id 回退 label），
    /// 命中判定复用 [`Self::metadata_matches`]（账号 id / label / 去重键任一命中即一致）。
    ///
    /// 身份无法辨认（live 读失败、blob 解析不出任何身份）≠ 不一致：只记 warn 放行。
    /// 写入内容来自账号库，解析不出身份是账号数据自身的问题而非切换写错；
    /// 校验只拦「写入结果身份与目标确证不符」，宁可漏报也不把可用切换误回滚
    /// （unknown ≠ mismatch，与凭证健康判定 ADR-0009 的「无法确证不算失效」同向）。
    /// 纯本地读 + 解析，零网络、成本可忽略。
    ///
    /// 凭证安全护栏（issue #154 reviewer P2）：回读到的身份恰为**切换前 live 身份**
    /// （`pre_swap_identity`）时，视为「官方客户端并发写回旧账号」特征——此刻 live
    /// 可能携带旧账号新轮换的一次性 refresh token 的唯一副本（refresh token 一次性
    /// 轮换，被备份覆盖即不可逆销毁）。回滚恢复备份**之前**，先经既有 capture-on-leave
    /// 通道把**当前 live 内容**抢救进其 owner 账号的 store，绝不让一次性 token 只存在
    /// 于即将被覆盖的 live 里；抢救失败如实并入错误并明示该账号可能需重新授权。
    /// 其他 mismatch（既非旧身份也非目标，如第三方身份）维持纯回滚语义：无证据表明
    /// live 携带任何账号的新凭证，按快照原样恢复即可。
    fn verify_live_identity_after_swap(
        runtime: &A,
        store: &dyn CredentialStore,
        registry: &AccountRegistry,
        home: &Path,
        account: &Account,
        pre_swap_identity: Option<&str>,
    ) -> Result<()> {
        let raw = match fs::read_to_string(runtime.live_cred_path(home)) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!(
                    account = %account.id,
                    err = %e,
                    "post-swap identity verify skipped: cannot re-read live credentials"
                );
                return Ok(());
            }
        };
        let meta = runtime.parse_metadata(&raw);
        let Some(observed) = Self::official_identity(&meta) else {
            tracing::warn!(
                account = %account.id,
                "post-swap identity verify skipped: live credentials carry no identity"
            );
            return Ok(());
        };
        if Self::metadata_matches(&meta, account, runtime.dedup_extra_key()) {
            return Ok(());
        }
        if pre_swap_identity == Some(observed) {
            // 官方并发写回旧账号：先把当前 live（可能含新一次性 refresh token 的
            // 唯一副本）经 capture-on-leave 通道抢救进 owner store，再返回 mismatch
            // 让骨架回滚恢复备份。
            if let Err(e) = Self::capture_live_into_store_with(runtime, store, registry, home) {
                // 抢救失败如实并入错误，不静默销毁线索；live 仍按备份恢复，
                // 维持「要么切对、要么回到原状」语义。
                tracing::error!(account = %observed, err = %e, "salvage capture failed");
                return Err(Error::provider(format!(
                    "切换后回读校验失败：登录文件身份 {observed} 与目标账号 {} 不一致（检测到官方客户端并发写回该账号）；已回滚到切换前状态，但抢救其新凭证进账号库失败（{e}），该账号可能需要重新授权",
                    account.id
                )));
            }
            tracing::warn!(
                account = %observed,
                target = %account.id,
                "post-swap verify: official client wrote the previous account back; salvaged live credentials into store before rollback"
            );
        }
        Err(Error::provider(format!(
            "切换后回读校验失败：登录文件身份 {observed} 与目标账号 {} 不一致，已回滚到切换前状态",
            account.id
        )))
    }

    /// 覆盖 live 前把 live 凭证回灌进其 owner 账号 store。
    /// 只接收借用参数，供 `activate` 的 `spawn_blocking` 闭包与同步 reconcile 共用一份实现。
    ///
    /// 按回灌新旧比对写入（[`Self::backfill_live_into_store_with`]，ADR-0019 决策 8）：
    /// live 缺 refresh 且 store 已有 refresh 时跳过（防静默写死账号），live 严格较旧时也跳过。
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
        Self::backfill_live_into_store_with(runtime, store, &owner.id, &live_raw)
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
    /// 窗口期（store=新/live=旧）：其后的查询或切换按回灌新旧比对回灌（ADR-0019 决策 8，
    /// 修订 ADR-0002）。可比较时较旧的 live 不再撤销新凭证，切走再切回即生效；任一方解析不出
    /// 过期时刻时仍以 live 为准、新凭证被撤销。补救是重试重新授权。
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
            // 窗口期对外行为见函数 doc：live 仍为旧凭证，查询照用它；错误文案必须把状态与补救讲清楚。
            Self::write_blob(&self.live_path(), &raw).map_err(Error::reauth_live_write_failed)?;
        }
        Ok(account)
    }
}

// ---------------------------------------------------------------------------
// 回灌新旧比对（ADR-0019 决策 8）：live → 账号库的回灌不让较旧的凭证覆盖较新的
// ---------------------------------------------------------------------------

/// 一份 live 凭证能否回灌进账号库当前副本的裁决。
#[derive(Clone, Copy, PartialEq, Eq)]
enum BackfillVerdict {
    /// 照写：库存没有这一项、两份 refresh token 相同、库存没有 refresh token 要保护，
    /// 或比不出 live 严格较旧（含任一方解析不出过期时刻）。
    Write,
    /// live 缺 refresh token 而库存有：跳过，官方墓碑化的登录文件不污染账号库。
    LiveMissingRefresh,
    /// 两份 refresh token 不同且 live 的过期时刻严格早于库存：跳过，较旧的凭证不覆盖较新的。
    LiveOlder,
}

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// live → 账号库的回灌。所有回灌点共用：capture-on-leave、读取当前账号凭证时的顺手回灌、
    /// 主动续期发现官方已抢先轮换时的采纳回灌。在凭证存储自身的锁下读库存、按
    /// [`Self::backfill_verdict`] 裁决、再写入（`set_if`）；跳过时记 warn。只防覆盖，从不改写 live。
    fn backfill_live_into_store_with(
        runtime: &A,
        store: &dyn CredentialStore,
        owner: &AccountId,
        live_raw: &str,
    ) -> Result<()> {
        // 先经 `get` 读一次：legacy 钥匙串里还没迁移的库存副本借此落进凭证文件，下面的 `set_if`
        // 才比对得到它（`set_if` 锁内只看凭证文件，ADR-0019 决策 2）。
        store.get(runtime.id(), owner.0.as_str(), runtime.store_field())?;
        let verdict = std::cell::Cell::new(BackfillVerdict::Write);
        store.set_if(
            runtime.id(),
            owner.0.as_str(),
            runtime.store_field(),
            live_raw,
            &|stored| {
                verdict.set(Self::backfill_verdict(runtime, stored, live_raw));
                verdict.get() == BackfillVerdict::Write
            },
        )?;
        match verdict.get() {
            BackfillVerdict::Write => {}
            BackfillVerdict::LiveMissingRefresh => tracing::warn!(
                account = %owner,
                "live credentials missing refresh_token; skipped backfill to keep the stored copy"
            ),
            BackfillVerdict::LiveOlder => tracing::warn!(
                account = %owner,
                "live credentials older than the stored copy; skipped backfill to keep the newer one"
            ),
        }
        Ok(())
    }

    /// 回灌新旧比对：两份 refresh token 相同照写；不同则比过期时刻
    /// （[`FileBlobRuntime::credential_expiry`]：JWT `exp` 优先，缺失回退 `expires_at`），
    /// live 严格较旧才不覆盖；live 更晚或相等、任一方解析不出都维持以 live 为准。
    /// 「live 缺 refresh token 而库存有 → 跳过」的守卫先于比对。
    fn backfill_verdict(runtime: &A, stored: Option<&str>, live: &str) -> BackfillVerdict {
        let Some(stored) = stored else {
            return BackfillVerdict::Write;
        };
        match (extract_refresh_token(live), extract_refresh_token(stored)) {
            (None, Some(_)) => BackfillVerdict::LiveMissingRefresh,
            (Some(live_refresh), Some(stored_refresh)) if live_refresh != stored_refresh => {
                match (
                    runtime.credential_expiry(live).map(|e| e.expires_at),
                    runtime.credential_expiry(stored).map(|e| e.expires_at),
                ) {
                    (Some(live_exp), Some(stored_exp)) if live_exp < stored_exp => {
                        BackfillVerdict::LiveOlder
                    }
                    _ => BackfillVerdict::Write,
                }
            }
            _ => BackfillVerdict::Write,
        }
    }
}

// ---------------------------------------------------------------------------
// Provider 实现：activate（含带前置条件的切换）+ query_quota + 凭证恢复信号
// ---------------------------------------------------------------------------

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// activate 与带前置条件切换的共用实现。`expected_current` 为 `Some` 时是期望当前账号
    /// （ADR-0020）：交换锁内先重读 registry 当前标记，不是它就返回
    /// [`ActivateOutcome::Superseded`]；`None` 时恒为 [`ActivateOutcome::Activated`] 或错误。
    async fn activate_expecting(
        &self,
        id: &AccountId,
        expected_current: Option<AccountId>,
    ) -> Result<ActivateOutcome> {
        // 阶段 1：只查 registry（无网络调用）。目标凭证在阶段 2 的交换锁内读取。
        let account = self.require_account(id)?;

        // 阶段 2：阻塞 IO 整体搬进 spawn_blocking。不用 `block_in_place`：调用方可能运行在
        // current-thread runtime（如测试），`block_in_place` 在那种 runtime 下会直接 panic；
        // `spawn_blocking` 没有这个限制。
        let home = self.home.clone();
        let live_path = self.live_path();
        let provider_id = self.runtime.id();
        let runtime = self.runtime.clone();
        let store = self.store.clone();
        let registry = self.registry.clone();
        let id_owned = id.clone();
        // registry.toml 纳入切换备份/回滚集（issue #153 续跑，reviewer P1-2）：
        // commit 阶段 set_active 的写入（含其 fsync 失败）与 live 同生命周期回滚。
        let registry_file = registry.path().to_path_buf();

        tokio::task::spawn_blocking(move || {
            let lock_dir = FileBlobProvider::<A>::swap_lock_dir(&registry, &home);
            #[cfg(test)]
            run_activate_lock_probe(&home);
            // 读取进锁（ADR-0019）：目标凭证读取、capture-on-leave、切换前身份记录，与写入、
            // 写后回读校验、registry 提交同在一段交换锁临界区。锁按线程可重入，下面的切换骨架
            // 与 registry 写入照常进锁。锁内纯本地 IO、零网络；轮换的网络请求在锁外，
            // 切换不等待任何在途轮换。
            with_swap_lock(&lock_dir, || {
                // 期望当前账号（ADR-0020）：进锁后先重读 registry 当前标记。registry 写入与切换
                // 同在这把锁下，判定到提交之间不会再有别的切换插进来。当前账号已不是期望账号
                // （决定之后有手动切换抢先）就放弃：不读写凭证、不建快照、不碰 registry。
                if let Some(expected) = &expected_current {
                    if !registry
                        .find(provider_id, expected)?
                        .is_some_and(|current| current.active)
                    {
                        tracing::info!(
                            account = %id_owned,
                            expected_current = %expected,
                            "activate superseded: current account changed before swap lock"
                        );
                        return Ok(ActivateOutcome::Superseded);
                    }
                }

                let target_raw = FileBlobProvider::<A>::raw_blob_for_account_with(
                    runtime.as_ref(),
                    store.as_ref(),
                    &home,
                    &account,
                )?;

                // capture-on-leave：覆盖 live 前，先把当前 live 凭证回灌进它所属账号的 store。
                // 否则切走的账号 store 副本会停在旧 refresh token，下次切回写回旧 token →
                // "already used"。失败只 warn、不阻断切换（ADR-0015 决策 11）。
                if let Err(e) = FileBlobProvider::<A>::capture_live_into_store_with(
                    runtime.as_ref(),
                    store.as_ref(),
                    registry.as_ref(),
                    &home,
                ) {
                    tracing::warn!(err = %e, "capture-on-leave failed; continuing swap");
                }

                // 切换前 live 身份快照（issue #154 reviewer P2）：此刻 writer 尚未运行，
                // live 仍是切换前内容；mismatch 回滚前用它识别「官方客户端并发写回
                // 旧账号」特征。读失败/解析不出 → None（无法确证，不做抢救判定）。
                let pre_swap_identity = fs::read_to_string(runtime.live_cred_path(&home))
                    .ok()
                    .and_then(|raw| {
                        let meta = runtime.parse_metadata(&raw);
                        Self::official_identity(&meta).map(str::to_string)
                    });

                let targets = vec![SwapTarget {
                    snapshot_name: "credentials",
                    live_path,
                    writer: Box::new(move |p: &Path| {
                        FileBlobProvider::<A>::write_blob(p, &target_raw)
                    }),
                }];
                swap_with_snapshot_protected(
                    provider_id,
                    &home,
                    &lock_dir,
                    targets,
                    vec![registry_file],
                    || {
                        // 写后回读身份校验（issue #154，spec #152 实现决策 6）：live 已落盘、
                        // registry 尚未提交。此刻校验失败走既有 commit 失败回滚——live 恢复
                        // 切换前内容、registry 从未变更，对外即「要么切对、要么回到原状」。
                        FileBlobProvider::<A>::verify_live_identity_after_swap(
                            runtime.as_ref(),
                            store.as_ref(),
                            registry.as_ref(),
                            &home,
                            &account,
                            pre_swap_identity.as_deref(),
                        )?;
                        registry.set_active(provider_id, &id_owned)
                    },
                )?;
                Ok(ActivateOutcome::Activated)
            })
        })
        .await
        .map_err(|e| Error::provider(format!("spawn_blocking join failed: {e}")))?
    }
}

#[async_trait]
impl<A: FileBlobRuntime> Provider for FileBlobProvider<A> {
    async fn activate(&self, id: &AccountId) -> Result<()> {
        self.activate_expecting(id, None).await.map(|_| ())
    }

    async fn activate_if_current(
        &self,
        id: &AccountId,
        expected_current: &AccountId,
    ) -> Result<ActivateOutcome> {
        self.activate_expecting(id, Some(expected_current.clone()))
            .await
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
        } else {
            // 停泊按需轮换（ADR-0019）：临期才先轮换，未临期遇 401 强制轮换一次、重试一次。
            return self.query_parked_quota(&account, &raw).await;
        }
        self.fetch_quota_with_blob(&account, &raw).await
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
    /// 轮换结果经轮换写回比对提交（[`Self::commit_rotation`]，ADR-0019）。写序仍遵守 ADR-0002：
    /// 账号库为唯一真本，先落账号库槽位，再原子写 live（tmp+rename+fsync，沿用切换写法）。
    /// 续期在途时用户切走了：当前登录文件已经换人，不写它、不撤销切换；库存比对通过，新凭证
    /// 照样落进该账号的库存。轮换被官方拒绝（invalid_grant）时记录指纹并停止重试，
    /// 本地凭证不删除不墓碑化，本次查询按「凭证过期型 401」（issue #41）呈现。
    ///
    /// live 原子写失败（本地 IO）时：账号库已是新凭证，live 保持被消耗的旧凭证，本次查询报错。
    /// 之后读取当前账号凭证（[`Self::raw_blob_for_account`]）与切走时的 capture-on-leave 都按
    /// 回灌新旧比对回灌（ADR-0019 决策 8）：可比较时旧 live 不再覆盖账号库，切走再切回即恢复；
    /// 任一方解析不出过期时刻时仍以 live 为准，旧 live 回灌库槽位、销毁新轮换 refresh token 的
    /// 唯一副本，此后轮换必被官方 invalid_grant 拒绝，收敛到凭证过期型 401 + 指纹停试，
    /// 需用户按引导重新授权（ADR-0003 已接受边界）。
    async fn renew_active_proactively(&self, account: &Account, raw: String) -> Result<String> {
        // 即用即读：轮换前现读 live 文件（官方即用即读，我们也即用即读），
        // 把与官方轮换的竞态窗口压到最小，不用查询开始时的旧副本。
        let Some(live) = self.read_live_if_matches(account) else {
            // live 读不出或已改属其他账号：不碰 live 与账号库，沿用原凭证查询。
            return Ok(raw);
        };
        if extract_access_token(&live).as_deref() != extract_access_token(&raw).as_deref() {
            // 官方已抢先轮换（与官方 100ms 重读采纳机制同款）：直接采纳最新 live，
            // 按回灌新旧比对顺手回灌账号库（ADR-0019 决策 8）；本次查询不再消耗 refresh token。
            // 回灌只是顺手保鲜：库写失败不应让手握有效 live 的查询整体失败，
            // 与 `raw_blob_for_account` 的回灌容错（`let _ =`）一致。
            let _ = Self::backfill_live_into_store_with(
                self.runtime.as_ref(),
                self.store.as_ref(),
                &account.id,
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
                // 轮换写回比对（ADR-0019）：`live` 是本次消耗掉的那份凭证。交换锁内先按比对写库存，
                // 再仅当该账号仍是当前账号、当前登录文件仍持被消耗的 token 时原子写 live；续期在途时
                // 被切走就不碰当前登录文件，不撤销切换。live 写失败时 live 保持旧内容不被破坏，
                // 库存里的新凭证之后靠回灌新旧比对保住（见函数 doc）。
                self.commit_rotation(account, &live, &new_blob).await?;
                // 指纹清除规则不变：只清掉这次被消耗的 blob 的拒止指纹。
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

/// 停泊按需轮换阈值 max(300 秒, `expires_in` × 0.5) 的下限一项（秒），与官方 OAuth 管理器同一
/// 口径（ADR-0019）。只写在 ADR 与 CONTEXT.md，不进配置。
const PARKED_ROTATION_MIN_REMAINING_SECS: i64 = 300;

/// 停泊账号此刻是否该先轮换再查询（停泊按需轮换，ADR-0019）：access token 剩余不足
/// max(300 秒, `expires_in` × 0.5) 即临期，已过期同样算；`expires_in` 缺失时阈值取 300 秒；
/// 过期时刻解析不出（`None`）→ 维持旧行为，先轮换。
fn parked_rotation_due(expiry: Option<CredentialExpiry>, now: i64) -> bool {
    let Some(expiry) = expiry else {
        return true;
    };
    let remaining = expiry.expires_at.saturating_sub(now);
    // 「剩余 < max(300, 寿命 × 0.5)」拆成两项各比一次；寿命一项两边乘 2，整数运算不丢半秒。
    remaining < PARKED_ROTATION_MIN_REMAINING_SECS
        || expiry
            .expires_in
            .is_some_and(|lifetime| remaining.saturating_mul(2) < lifetime)
}

/// 强制轮换要求的剩余时间余量：盖住发请求前的本地准备与调度抖动（ADR-0019）。
const FORCED_ROTATION_BUDGET_MARGIN: Duration = Duration::from_secs(2);

/// 本次额度查询剩下的时间够不够发强制轮换：要盖得住轮换请求的超时上限加余量，外层超时才
/// 不可能在轮换响应返回前取消查询（ADR-0019）。没有外层超时（`time_left` 为 `None`）时
/// 照发；有外层超时而 runtime 说不出上限时不发（fail-closed）。
///
/// 查询途中由 401 触发、被取消即丢失的轮换都用这一条规则：停泊账号的强制轮换（引擎），
/// 以及 adapter 持官方刷新锁的当前账号 401 协调恢复（Kimi）。`time_left` 取自
/// `quota_query::attempt_time_left`，`refresh_timeout` 取自 [`FileBlobRuntime::refresh_timeout`]。
pub fn forced_rotation_fits(
    time_left: Option<Duration>,
    refresh_timeout: Option<Duration>,
) -> bool {
    let Some(left) = time_left else {
        return true;
    };
    refresh_timeout.is_some_and(|limit| left >= limit.saturating_add(FORCED_ROTATION_BUDGET_MARGIN))
}

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// 停泊账号的额度查询（停泊按需轮换，ADR-0019）：access token 临期（见
    /// [`parked_rotation_due`]）、过期时刻解析不出或凭证里没有 access token 时先轮换再查询。
    /// 未临期直接用现有 access token 查询；遇 401（服务端提前作废了 access token）强制轮换
    /// 一次、重试一次。
    ///
    /// 两种轮换都走 [`Self::refresh_parked_if_needed`]，经轮换写回比对提交、守 DeadToken 指纹
    /// 纪律。强制轮换排在一次查询之后，发出前先核对本次查询的剩余时间（见
    /// [`forced_rotation_fits`]），不够就不发、返回首个 401，留给下次查询。强制轮换被拒或
    /// 不可用时同样返回首个 401，重试仍是 401 时原样返回：都按既有停泊 401 口径呈现（裸 401），
    /// 不归为凭证待续期。
    async fn query_parked_quota(&self, account: &Account, raw: &str) -> Result<Vec<Quota>> {
        // 凭证里没有 access token 就无从查询：与过期时刻解析不出同样处理，先轮换再查询。
        let expiry = extract_access_token(raw).and_then(|_| self.runtime.credential_expiry(raw));
        if parked_rotation_due(expiry, Utc::now().timestamp()) {
            let fresh = self.refresh_parked_if_needed(account, raw).await?;
            return self
                .fetch_quota_with_blob(account, fresh.as_deref().unwrap_or(raw))
                .await;
        }
        match self.fetch_quota_with_blob(account, raw).await {
            Err(unauthorized) if unauthorized.http_status() == Some(401) => {
                let time_left = kimi_switch_core::quota_query::attempt_time_left();
                if !forced_rotation_fits(time_left, self.runtime.refresh_timeout()) {
                    // 剩余时间盖不住轮换请求：不发，留给下次查询用完整预算重来。
                    tracing::warn!(
                        account = %account.id,
                        time_left_ms = time_left.map(|left| left.as_millis()),
                        "parked quota query got 401 too late in its attempt; leaving forced rotation to the next query"
                    );
                    return Err(unauthorized);
                }
                tracing::info!(
                    account = %account.id,
                    "parked quota query got 401 before access token expiry; forcing one rotation"
                );
                match self.refresh_parked_if_needed(account, raw).await? {
                    Some(fresh) => self.fetch_quota_with_blob(account, &fresh).await,
                    None => Err(unauthorized),
                }
            }
            other => other,
        }
    }

    /// 用 blob 里的 access token 查询额度。
    async fn fetch_quota_with_blob(&self, account: &Account, raw: &str) -> Result<Vec<Quota>> {
        let access = extract_access_token(raw).ok_or_else(|| {
            Error::quota_missing_credential(
                "no access_token in credentials; schema may have changed",
            )
        })?;
        self.runtime.fetch_quota(&access, account).await
    }

    /// parked 账号刷新一次，结果经轮换写回比对提交（[`Self::commit_rotation`]，ADR-0019）：
    /// 刷新请求在锁外，提交在交换锁内，轮换期间被切成当前账号时同步写当前登录文件。
    /// 由 [`Self::query_parked_quota`] 在临期时、或未临期遇 401 时调用。
    /// 返回 `Some(新 blob)` 表示已轮换；本次查询照用新凭证，不论库存是否因比对不符放弃写入。
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
                self.commit_rotation(account, raw, &new_blob).await?;
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
// 轮换写回比对（ADR-0019）：网络请求在锁外完成，提交在交换锁内进行
// ---------------------------------------------------------------------------

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// 交换锁目录：注册表同目录，与注册表写操作、切换同一临界区。
    fn swap_lock_dir(registry: &AccountRegistry, home: &Path) -> PathBuf {
        registry
            .path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.to_path_buf())
    }

    /// 轮换写回比对的异步入口：交换锁最多等 3 秒，阻塞部分放进 `spawn_blocking`。
    /// 规则见 [`Self::commit_rotation_with`]。
    async fn commit_rotation(
        &self,
        account: &Account,
        consumed: &str,
        rotated: &str,
    ) -> Result<()> {
        let runtime = self.runtime.clone();
        let store = self.store.clone();
        let registry = self.registry.clone();
        let home = self.home.clone();
        let account = account.clone();
        let consumed = consumed.to_string();
        let rotated = rotated.to_string();
        tokio::task::spawn_blocking(move || {
            Self::commit_rotation_with(
                runtime.as_ref(),
                store.as_ref(),
                registry.as_ref(),
                &home,
                &account,
                &consumed,
                &rotated,
            )
        })
        .await
        .map_err(|e| Error::provider(format!("spawn_blocking join failed: {e}")))?
    }

    /// 轮换写回比对：`consumed` 是本次轮换消耗掉的那份凭证，`rotated` 是轮换结果。
    ///
    /// 在交换锁内依次提交（写序先库存后 live，ADR-0002；锁内不做网络请求）：
    /// 1. 库存凭证仍持被消耗的 refresh token 才替换，否则放弃并记 warn
    ///    （[`Self::store_rotation_if_unchanged`]）。
    /// 2. 该账号此刻是 registry 当前账号、当前登录文件身份是它、且仍持被消耗的 refresh token
    ///    时，把新凭证原子写进当前登录文件；当前登录文件已经换人或已被别处更新时绝不覆盖。
    ///
    /// 第 1 步出错（凭证存储 IO 故障）照样执行第 2 步：此刻当前登录文件可能是新 refresh token
    /// 唯一能落的地方。第 2 步写进了当前登录文件，新凭证已有归宿，库存错误只记 warn、返回成功，
    /// 之后的回灌新旧比对（ADR-0019 决策 8）把它补回库存；第 2 步没写（不是当前账号、已换人
    /// 或已被更新）或也出错，如实返回库存错误。
    ///
    /// 交换锁没进去（3 秒等待超时，或锁文件建不了、打不开、加锁出错）时临界区一步都没执行：
    /// 放弃写当前登录文件，库存仍在凭证存储自身的锁下按比对写入并记 warn，新凭证绝不因锁
    /// 不可用而丢弃。比对只看 refresh token，不比整串。
    ///
    /// 借用参数、同步执行，供各写回点在 `spawn_blocking` 里共用：引擎内的停泊账号查询前轮换与
    /// 当前账号主动续期（经 [`Self::commit_rotation`]），以及 adapter 自己持官方刷新锁完成刷新的
    /// 写回点（Kimi 401 协调恢复；锁顺序官方刷新锁 → 交换锁）。`store`、`registry`、`home`
    /// 必须是该 provider 装配时的同一份。
    pub fn commit_rotation_with(
        runtime: &A,
        store: &dyn CredentialStore,
        registry: &AccountRegistry,
        home: &Path,
        account: &Account,
        consumed: &str,
        rotated: &str,
    ) -> Result<()> {
        let consumed_refresh = extract_refresh_token(consumed);
        let consumed_refresh = consumed_refresh.as_deref();
        let entered = std::cell::Cell::new(false);
        let committed = with_swap_lock(&Self::swap_lock_dir(registry, home), || {
            entered.set(true);
            let stored = Self::store_rotation_if_unchanged(
                runtime,
                store,
                account,
                consumed_refresh,
                rotated,
            );
            let live =
                Self::live_still_holds_consumed(runtime, registry, home, account, consumed_refresh)
                    .and_then(|holds| {
                        if holds {
                            Self::write_blob(&runtime.live_cred_path(home), rotated)?;
                        }
                        Ok(holds)
                    });
            match (stored, live) {
                (Ok(()), live) => live.map(|_| ()),
                (Err(store_err), Ok(true)) => {
                    tracing::warn!(
                        account = %account.id,
                        err = %store_err,
                        "rotation writeback to stored credentials failed; new credentials kept in live credentials"
                    );
                    Ok(())
                }
                (Err(store_err), Ok(false)) => Err(store_err),
                (Err(store_err), Err(live_err)) => {
                    tracing::warn!(
                        account = %account.id,
                        err = %live_err,
                        "rotation writeback to live credentials failed as well"
                    );
                    Err(store_err)
                }
            }
        });
        let lock_err = match committed {
            Err(lock_err) if !entered.get() => lock_err,
            other => return other,
        };
        tracing::warn!(
            account = %account.id,
            err = %lock_err,
            "swap lock not entered; rotation written back to stored credentials only, live credentials untouched"
        );
        Self::store_rotation_if_unchanged(runtime, store, account, consumed_refresh, rotated)
    }

    /// 库存侧比对写：在凭证存储自身的锁下读当前库存，refresh token 仍等于被消耗的那个才写入
    /// 轮换结果；库存已被别处更新（重新授权、另一进程的轮换、回灌）就放弃并记 warn。
    fn store_rotation_if_unchanged(
        runtime: &A,
        store: &dyn CredentialStore,
        account: &Account,
        consumed_refresh: Option<&str>,
        rotated: &str,
    ) -> Result<()> {
        let written = store.set_if(
            runtime.id(),
            account.id.0.as_str(),
            runtime.store_field(),
            rotated,
            &|current| {
                current.is_some_and(|raw| extract_refresh_token(raw).as_deref() == consumed_refresh)
            },
        )?;
        if !written {
            tracing::warn!(
                account = %account.id,
                "rotation writeback skipped: stored credentials no longer hold the consumed refresh token"
            );
        }
        Ok(())
    }

    /// 交换锁内重读：该账号是否仍是 registry 当前账号、当前登录文件身份是否是它、
    /// 当前登录文件是否仍持 `consumed_refresh`。任一项不符（含 live 读不出）即 false。
    fn live_still_holds_consumed(
        runtime: &A,
        registry: &AccountRegistry,
        home: &Path,
        account: &Account,
        consumed_refresh: Option<&str>,
    ) -> Result<bool> {
        let Some(current) = registry.find(runtime.id(), &account.id)? else {
            return Ok(false);
        };
        if !current.active {
            return Ok(false);
        }
        let Ok(live) = fs::read_to_string(runtime.live_cred_path(home)) else {
            return Ok(false);
        };
        let meta = runtime.parse_metadata(&live);
        Ok(
            Self::metadata_matches(&meta, &current, runtime.dedup_extra_key())
                && extract_refresh_token(&live).as_deref() == consumed_refresh,
        )
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
        /// 额度查询次数（停泊按需轮换的「重试一次」靠它断言）。
        fetch_calls: AtomicUsize,
        last_access_token: Mutex<Option<String>>,
        /// 用这些 access token 查询额度一律返回 401，模拟服务端提前作废了 access token。
        unauthorized_access_tokens: HashSet<String>,
        /// 第一次额度查询先等这么久再回应（模拟慢 401）；默认不等。
        first_fetch_delay: Duration,
        /// 轮换请求发出后等这么久才回应（服务端此刻已消耗 refresh token）；默认不等。
        refresh_delay: Duration,
        legacy_blob: Option<String>,
        /// 默认 "dedup_key"；部分测试覆盖成非默认名，模拟 Beta 把去重键落在
        /// 一个迁移前就存在、不叫 "dedup_key" 的字段名下。
        dedup_extra_key: &'static str,
        /// 主动续期门（issue #44）：默认 false = 开关关闭/有锁平台，当前账号不轮换。
        proactive_wanted: bool,
        /// 模拟官方在判定期间抢先改写 live：wanted 判定时把 live 改成这份 blob。
        official_live_during_wanted: Option<String>,
        /// 官方并发写回模拟（issue #154 reviewer P2）：live 内容恰好等于 `after`
        /// （即 writer 刚写入的目标 blob）时，把它覆写为 `with`（官方为旧账号
        /// 轮换出的新 token 版本）。条件按内容匹配、覆写后自失效，天然只触发
        /// 一次，不依赖对 `live_cred_path` 的调用计数。
        official_overwrite_when_live_is: Option<(String, String)>,
        /// 这些 uid 的 refresh 一律 DeadToken，便于同一 runtime 里混测「当前号轮换 / 停泊死号」。
        dead_uids: HashSet<String>,
        /// 可暂停轮换闸门（ADR-0019）：设置后 refresh 在请求已发出时停住，等测试放行。
        rotation_gate: Option<Arc<RotationGate>>,
        /// `Rotate` 行为返回的轮换结果；缺省按旧规则生成 `{"uid":…,"access_token":"ROTATED"}`。
        rotated_blob: Option<String>,
    }

    /// 可暂停轮换闸门：轮换请求发出后停住（此刻服务端已消耗本次 refresh token），
    /// 由测试放行。竞态用例靠它构造「轮换在途 × 切换」交错，不靠 sleep 赌时序。
    #[derive(Default)]
    struct RotationGate {
        sent: tokio::sync::Notify,
        released: tokio::sync::Notify,
    }

    impl RotationGate {
        /// 等到轮换请求已发出、停在闸门上。
        async fn wait_sent(&self) {
            self.sent.notified().await;
        }

        /// 放行停在闸门上的轮换。
        fn release(&self) {
            self.released.notify_one();
        }
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
                fetch_calls: AtomicUsize::new(0),
                last_access_token: Mutex::new(None),
                unauthorized_access_tokens: HashSet::new(),
                first_fetch_delay: Duration::ZERO,
                refresh_delay: Duration::ZERO,
                legacy_blob: None,
                dedup_extra_key: "dedup_key",
                proactive_wanted: false,
                official_live_during_wanted: None,
                official_overwrite_when_live_is: None,
                dead_uids: HashSet::new(),
                rotation_gate: None,
                rotated_blob: None,
            }
        }

        /// 轮换成功并返回 `rotated`，但请求发出后先停在 `gate` 上等测试放行。
        fn with_rotation_gate(home: PathBuf, gate: Arc<RotationGate>, rotated: &str) -> Self {
            Self {
                rotation_gate: Some(gate),
                rotated_blob: Some(rotated.to_string()),
                ..Self::with_behavior(home, RefreshBehavior::Rotate)
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
            // 测试 seam（issue #154 reviewer P2）：模拟官方客户端在采纳窗口内把
            // 旧账号新轮换的一次性 token 写回 live——恰在读路径解析前、live 已是
            // writer 写入的目标 blob 时覆写。真实时序注入：写入后、回读前。
            if let Some((after, with)) = &self.official_overwrite_when_live_is {
                let live = home.join("live.json");
                if fs::read_to_string(&live).ok().as_deref() == Some(after.as_str()) {
                    fs::write(&live, with).unwrap();
                }
            }
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
            if let Some(gate) = &self.rotation_gate {
                gate.sent.notify_one();
                gate.released.notified().await;
            }
            if !self.refresh_delay.is_zero() {
                tokio::time::sleep(self.refresh_delay).await;
            }
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
                    RefreshOutcome::Rotated(self.rotated_blob.clone().unwrap_or_else(|| {
                        let v: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
                        let uid = v.get("uid").and_then(|x| x.as_str()).unwrap_or_default();
                        format!(r#"{{"uid":"{uid}","access_token":"ROTATED"}}"#)
                    }))
                }
            })
        }
        async fn fetch_quota(&self, access_token: &str, _account: &Account) -> Result<Vec<Quota>> {
            let call = self.fetch_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 && !self.first_fetch_delay.is_zero() {
                tokio::time::sleep(self.first_fetch_delay).await;
            }
            *self.last_access_token.lock().unwrap() = Some(access_token.to_string());
            if self.unauthorized_access_tokens.contains(access_token) {
                return Err(Error::quota_http(401, "fake usages HTTP 401 Unauthorized"));
            }
            Ok(vec![])
        }
        fn recover_legacy(&self, _home: &Path, _account: &Account) -> Option<String> {
            self.legacy_blob.clone()
        }
        // 测试 blob 不是 JWT：过期信息只认顶层整数 `expires_at` / `expires_in`
        // （JWT `exp` 优先的口径由 Kimi 适配器负责）。
        fn credential_expiry(&self, blob: &str) -> Option<CredentialExpiry> {
            let v: serde_json::Value = serde_json::from_str(blob).ok()?;
            Some(CredentialExpiry {
                expires_at: v.get("expires_at")?.as_i64()?,
                expires_in: v.get("expires_in").and_then(|x| x.as_i64()),
            })
        }
        fn refresh_timeout(&self) -> Option<Duration> {
            Some(FAKE_REFRESH_TIMEOUT)
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

    /// 切换前快照根目录由 `AppPaths::resolve()` 读 `KIMI_SWITCH_HOME` 决定，未设置时落到
    /// 真实用户数据目录。provider 夹具持有本守卫，把它收口到用例 tempdir；进程级 env
    /// 在并行用例间共享一把锁，Drop 先恢复 env 再放锁（模式与 core `swap.rs` 夹具一致）。
    struct IsolatedSwitchHome {
        previous: Option<std::ffi::OsString>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for IsolatedSwitchHome {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("KIMI_SWITCH_HOME", value),
                None => std::env::remove_var("KIMI_SWITCH_HOME"),
            }
        }
    }

    fn isolate_switch_home(tmp: &Path) -> IsolatedSwitchHome {
        static SWITCH_HOME_LOCK: Mutex<()> = Mutex::new(());
        let guard = SWITCH_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("KIMI_SWITCH_HOME");
        std::env::set_var("KIMI_SWITCH_HOME", tmp.join("switch-home"));
        IsolatedSwitchHome {
            previous,
            _guard: guard,
        }
    }

    /// 测试用 provider：带快照隔离守卫，随用例结束一起释放。守卫持锁到用例结束，
    /// 所以每个用例只建一个 provider。
    struct TestProvider {
        inner: FileBlobProvider<FakeRuntime>,
        _switch_home: IsolatedSwitchHome,
    }

    impl std::ops::Deref for TestProvider {
        type Target = FileBlobProvider<FakeRuntime>;

        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }

    fn provider(tmp: &Path) -> TestProvider {
        provider_with(tmp, FakeRuntime::new(tmp.join("home")))
    }

    fn provider_with(tmp: &Path, runtime: FakeRuntime) -> TestProvider {
        let switch_home = isolate_switch_home(tmp);
        let store = Arc::new(FileStore::new(tmp.join("creds.json")));
        let registry = Arc::new(AccountRegistry::new(tmp.join("registry.toml")));
        TestProvider {
            inner: FileBlobProvider::new(runtime, store, registry),
            _switch_home: switch_home,
        }
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

    // --- 回灌新旧比对（ADR-0019 决策 8）：经 capture-on-leave 通道断言 ---

    /// 把 u1 的库存设成 `stored`、当前登录文件设成 `live`，回灌一次，返回 u1 的库存。
    fn backfill_once(p: &FileBlobProvider<FakeRuntime>, stored: &str, live: &str) -> String {
        p.import_raw(stored.into(), None, Some(false)).unwrap();
        fs::write(p.test_live_path(), live).unwrap();
        p.reconcile_active_from_live().unwrap();
        p.test_store_get("u1").unwrap()
    }

    /// 两份 refresh token 不同、live 的过期时刻严格较早：不覆盖库存里较新的凭证。
    #[test]
    fn backfill_skips_live_older_than_stored_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();
        let stored = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2","expires_at":2000}"#;
        let older = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1","expires_at":1000}"#;

        assert_eq!(
            backfill_once(&p, stored, older),
            stored,
            "较旧的 live 不得覆盖库存里较新的凭证"
        );
    }

    /// live 过期得更晚或同刻：照常覆盖。
    #[test]
    fn backfill_overwrites_when_live_is_newer_or_equally_new() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();
        let stored = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1","expires_at":1000}"#;
        let newer = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2","expires_at":2000}"#;
        let same_time =
            r#"{"uid":"u1","refresh_token":"R3","access_token":"A3","expires_at":1000}"#;

        assert_eq!(backfill_once(&p, stored, newer), newer, "live 较新时覆盖");
        assert_eq!(
            backfill_once(&p, stored, same_time),
            same_time,
            "同刻过期不算严格较旧，照常覆盖"
        );
    }

    /// 任一方解析不出过期时刻：不可比较，维持以 live 为准。
    #[test]
    fn backfill_lets_live_win_when_either_side_has_no_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();
        let stored_with_expiry =
            r#"{"uid":"u1","refresh_token":"R2","access_token":"A2","expires_at":2000}"#;
        let live_without_expiry = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let stored_without_expiry = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2"}"#;
        let live_with_expiry =
            r#"{"uid":"u1","refresh_token":"R1","access_token":"A1","expires_at":1000}"#;

        assert_eq!(
            backfill_once(&p, stored_with_expiry, live_without_expiry),
            live_without_expiry,
            "live 无过期时刻：维持现状，live 覆盖"
        );
        assert_eq!(
            backfill_once(&p, stored_without_expiry, live_with_expiry),
            live_with_expiry,
            "库存无过期时刻：维持现状，live 覆盖"
        );
    }

    /// 仿 `FileStore` 的 legacy 钥匙串回退：`get` 在凭证文件未命中时取 legacy 副本并迁移进文件，
    /// `set_if` 只比对凭证文件（锁内不碰钥匙串）。生产装配就是带 legacy 回退的 `FileStore`。
    struct UnmigratedLegacyStore {
        inner: FileStore,
        legacy: Mutex<std::collections::BTreeMap<(String, String, String), String>>,
    }

    impl kimi_switch_core::CredentialStore for UnmigratedLegacyStore {
        fn set(&self, provider: &str, account: &str, field: &str, value: &str) -> Result<()> {
            self.inner.set(provider, account, field, value)
        }
        fn get(&self, provider: &str, account: &str, field: &str) -> Result<Option<String>> {
            if let Some(value) = self.inner.get(provider, account, field)? {
                return Ok(Some(value));
            }
            let key = (provider.to_string(), account.to_string(), field.to_string());
            let Some(value) = self.legacy.lock().unwrap().remove(&key) else {
                return Ok(None);
            };
            self.inner.set(provider, account, field, &value)?;
            Ok(Some(value))
        }
        fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()> {
            self.inner.delete(provider, account, field)
        }
        fn set_if(
            &self,
            provider: &str,
            account: &str,
            field: &str,
            value: &str,
            should_write: &dyn Fn(Option<&str>) -> bool,
        ) -> Result<bool> {
            self.inner
                .set_if(provider, account, field, value, should_write)
        }
    }

    /// 回灌比对的是账号库里的副本，含 legacy 钥匙串里还没迁移进凭证文件的那份：
    /// live 缺 refresh token 而该副本有时，守卫照样跳过，不把它盖掉。
    #[test]
    fn backfill_guard_sees_stored_copy_not_yet_migrated_from_legacy_keyring() {
        let tmp = tempfile::tempdir().unwrap();
        let _switch_home = isolate_switch_home(tmp.path());
        let legacy_copy = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let store = UnmigratedLegacyStore {
            inner: FileStore::new(tmp.path().join("creds.json")),
            legacy: Mutex::new(
                [(
                    ("fake".to_string(), "u1".to_string(), "blob".to_string()),
                    legacy_copy.to_string(),
                )]
                .into(),
            ),
        };
        let p = FileBlobProvider::new(
            FakeRuntime::new(tmp.path().join("home")),
            Arc::new(store),
            Arc::new(AccountRegistry::new(tmp.path().join("registry.toml"))),
        );
        p.test_registry_upsert(fake_account("u1")).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), r#"{"uid":"u1","access_token":"A2"}"#).unwrap();

        p.reconcile_active_from_live().unwrap();

        assert_eq!(
            p.test_store_get("u1").unwrap(),
            legacy_copy,
            "缺 refresh token 的 live 不得盖掉 legacy 里那份带 refresh token 的库存副本"
        );
    }

    /// 库存里没有另一个 refresh token 要保护时照写，即使 live 过期得更早：
    /// refresh token 相同（同一凭证的其他字段跟着同步），或库存根本没有 refresh token。
    #[test]
    fn backfill_writes_earlier_expiring_live_when_stored_refresh_token_is_same_or_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();
        let stored =
            r#"{"uid":"u1","refresh_token":"R1","access_token":"A-LATE","expires_at":2000}"#;
        let same_refresh =
            r#"{"uid":"u1","refresh_token":"R1","access_token":"A-EARLY","expires_at":1000}"#;
        let stored_without_refresh = r#"{"uid":"u1","access_token":"A-LATE","expires_at":2000}"#;
        let live = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1","expires_at":1000}"#;

        assert_eq!(
            backfill_once(&p, stored, same_refresh),
            same_refresh,
            "refresh token 相同：照写"
        );
        assert_eq!(
            backfill_once(&p, stored_without_refresh, live),
            live,
            "库存没有 refresh token：没有一次性 token 要保护，照写"
        );
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

    /// 导入/登录从 blob metadata 整表重建 extra 时，必须把已有会员到期三键带回去。
    /// 真机：当前账号走 GUI 导入 / CLI login 后轨道消失，停泊账号没走过导入所以还在。
    #[test]
    fn import_active_keeps_membership_expiry_extra_keys() {
        use kimi_switch_core::membership_expiry::{
            EXTRA_CURRENT_END_TIME, EXTRA_CURRENT_START_TIME, EXTRA_NEXT_BILLING_TIME,
        };

        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), r#"{"uid":"u1","access_token":"A1"}"#).unwrap();
        p.import_active(None).unwrap();

        let mut bound = p
            .test_list_accounts()
            .unwrap()
            .into_iter()
            .find(|account| account.id.0 == "u1")
            .expect("imported u1");
        bound.extra.insert(
            EXTRA_CURRENT_END_TIME.into(),
            serde_json::Value::String("2027-08-28T00:00:00Z".into()),
        );
        bound.extra.insert(
            EXTRA_NEXT_BILLING_TIME.into(),
            serde_json::Value::String("2027-08-27T01:25:37.930341Z".into()),
        );
        bound.extra.insert(
            EXTRA_CURRENT_START_TIME.into(),
            serde_json::Value::String("2026-08-28T00:00:00Z".into()),
        );
        p.test_registry_upsert(bound).unwrap();

        fs::write(p.test_live_path(), r#"{"uid":"u1","access_token":"A2"}"#).unwrap();
        p.import_active(None).unwrap();

        let extra = p
            .test_list_accounts()
            .unwrap()
            .into_iter()
            .find(|account| account.id.0 == "u1")
            .expect("re-imported u1")
            .extra;
        assert_eq!(
            extra.get(EXTRA_CURRENT_END_TIME).and_then(|v| v.as_str()),
            Some("2027-08-28T00:00:00Z")
        );
        assert_eq!(
            extra.get(EXTRA_NEXT_BILLING_TIME).and_then(|v| v.as_str()),
            Some("2027-08-27T01:25:37.930341Z")
        );
        assert_eq!(
            extra.get(EXTRA_CURRENT_START_TIME).and_then(|v| v.as_str()),
            Some("2026-08-28T00:00:00Z")
        );
        assert!(
            p.test_store_get("u1").unwrap().contains("A2"),
            "Code blob 仍要换成这次导入的新凭证"
        );
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

    /// 回灌新旧比对（ADR-0019 决策 8，修订 ADR-0002）：重授权 live 写失败留下
    /// store=新/live=旧 窗口后，其后一次切换的 capture-on-leave 不再用旧 live 撤销新凭证；
    /// 再切回该账号时当前登录文件拿到的就是重新授权的结果，重新授权不白做。
    #[tokio::test]
    async fn reauth_live_write_failure_window_then_activate_keeps_new_credentials_in_store() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let old_u1 = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1","expires_at":1000}"#;
        let new_u1 = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2","expires_at":2000}"#;
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
            new_u1,
            "窗口期内切走时，capture-on-leave 不得用旧 live 撤销库存里的新凭证"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            u2_blob,
            "切换后 live 应为 u2 的 blob"
        );

        p.activate(&AccountId("u1".into())).await.unwrap();
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            new_u1,
            "切回 u1 时当前登录文件应拿到重新授权的新凭证"
        );
    }

    /// 回灌新旧比对（ADR-0019 决策 8，修订 ADR-0002）：重授权 live 写失败留下
    /// store=新/live=旧 窗口后，对当前账号一次额度查询不再用旧 live 撤销新凭证。
    /// 两份凭证都带过期时刻、live 严格较旧，读取时的顺手回灌放弃覆盖；查询仍用当前登录文件，
    /// live 也不被主动改写。
    #[tokio::test]
    async fn reauth_live_write_failure_window_then_query_quota_keeps_new_credentials_in_store() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let old_u1 = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1","expires_at":1000}"#;
        let new_u1 = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2","expires_at":2000}"#;

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

        // 解除注入后对当前账号查一次额度（FakeRuntime 无网络）。
        fs::remove_dir_all(&tmp_target).unwrap();
        p.query_quota(&AccountId("u1".into())).await.unwrap();

        assert_eq!(
            p.test_store_get("u1").unwrap(),
            new_u1,
            "窗口期内的查询不得用旧 live 撤销库存里的新凭证"
        );
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("A1"),
            "读取仍以当前登录文件为准"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            old_u1,
            "只防覆盖，不主动改写当前登录文件"
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

    /// 凭证不带过期信息（过期时刻解析不出）→ 停泊按需轮换维持旧行为，先轮换再查询（ADR-0019）。
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

    // --- query_quota：停泊按需轮换（ADR-0019，#190）---

    /// 停泊按需轮换用例的凭证：access token 在此刻之后 `remaining_secs` 秒过期，`expires_in` 可缺。
    fn blob_expiring_in(uid: &str, remaining_secs: i64, expires_in: Option<i64>) -> String {
        let mut blob = serde_json::json!({
            "uid": uid,
            "refresh_token": format!("R-{uid}"),
            "access_token": format!("A-{uid}"),
            "expires_at": Utc::now().timestamp() + remaining_secs,
        });
        if let Some(secs) = expires_in {
            blob["expires_in"] = secs.into();
        }
        blob.to_string()
    }

    /// 未临期（15 分钟寿命还剩 14 分钟，阈值 max(300, 450) = 450 秒）→ 直接用现有 access token
    /// 查询，不轮换，库存凭证不动。
    #[tokio::test]
    async fn parked_query_before_expiry_does_not_rotate() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let blob = blob_expiring_in("u1", 840, Some(900));
        let account = p.import_raw(blob.clone(), None, Some(false)).unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            0,
            "未临期不得轮换"
        );
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("A-u1")
        );
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            blob,
            "未轮换时库存凭证不动"
        );
    }

    /// 临期（剩余不足阈值）或已过期 → 先轮换再查询，查询用轮换后的 access token。
    #[tokio::test]
    async fn parked_query_near_or_past_expiry_rotates_first() {
        for (remaining, expires_in, case) in [
            (400, Some(900), "剩 400 秒，不足半寿命 450 秒"),
            (200, None, "expires_in 缺失，剩 200 秒不足 300 秒"),
            (-60, Some(900), "已过期"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let runtime =
                FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
            let p = provider_with(tmp.path(), runtime);
            let blob = blob_expiring_in("u1", remaining, expires_in);
            let account = p.import_raw(blob, None, Some(false)).unwrap();

            p.query_quota(&account.id).await.unwrap();

            assert_eq!(
                p.test_runtime().refresh_calls.load(Ordering::SeqCst),
                1,
                "{case}：应先轮换"
            );
            assert_eq!(
                p.test_runtime()
                    .last_access_token
                    .lock()
                    .unwrap()
                    .as_deref(),
                Some("ROTATED"),
                "{case}：查询应用轮换后的 access token"
            );
        }
    }

    /// 凭证里没有 access token（即便 `expires_at` 未临期）→ 无从查询，先轮换再查询。
    #[tokio::test]
    async fn parked_query_without_access_token_rotates_first() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let mut blob: serde_json::Value =
            serde_json::from_str(&blob_expiring_in("u1", 840, Some(900))).unwrap();
        blob.as_object_mut().unwrap().remove("access_token");
        let account = p.import_raw(blob.to_string(), None, Some(false)).unwrap();

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
    }

    /// 阈值 max(300 秒, expires_in × 0.5) 的边界：剩余「不足」才临期，恰好等于阈值不算；
    /// expires_in 缺失取 300 秒；已过期算临期；过期时刻解析不出照旧先轮换。
    #[test]
    fn parked_rotation_threshold_is_max_of_300s_and_half_lifetime() {
        let now = 2_000_000_000_i64;
        let due = |remaining: i64, expires_in: Option<i64>| {
            parked_rotation_due(
                Some(CredentialExpiry {
                    expires_at: now + remaining,
                    expires_in,
                }),
                now,
            )
        };
        // 15 分钟寿命：阈值取半寿命 450 秒。
        assert!(!due(450, Some(900)));
        assert!(due(449, Some(900)));
        // 寿命 400 秒：半寿命 200 秒低于下限，阈值取 300 秒。
        assert!(!due(300, Some(400)));
        assert!(due(299, Some(400)));
        // expires_in 缺失：阈值取 300 秒。
        assert!(!due(300, None));
        assert!(due(299, None));
        // 奇数寿命不丢半秒：901 秒的阈值是 450.5 秒，剩 450 秒已不足。
        assert!(due(450, Some(901)));
        assert!(!due(451, Some(901)));
        // 已过期（含恰在过期时刻）算临期。
        assert!(due(0, Some(900)));
        assert!(due(-60, Some(900)));
        // 过期时刻解析不出：维持旧行为，先轮换。
        assert!(parked_rotation_due(None, now));
    }

    /// 强制轮换的预算门：剩余时间至少是轮换请求超时上限加 2 秒余量才发；没有外层超时照发；
    /// 有外层超时而上限不明时不发。
    #[test]
    fn forced_rotation_fits_only_when_time_left_covers_refresh_timeout_plus_margin() {
        let limit = Some(Duration::from_secs(10));
        assert!(forced_rotation_fits(None, limit));
        assert!(forced_rotation_fits(None, None));
        assert!(forced_rotation_fits(Some(Duration::from_secs(12)), limit));
        assert!(!forced_rotation_fits(
            Some(Duration::from_millis(11_999)),
            limit
        ));
        assert!(!forced_rotation_fits(Some(Duration::from_secs(60)), None));
    }

    /// 未临期却遇 401（服务端提前作废了 access token）→ 强制轮换一次，用新 access token 重试一次；
    /// 强制轮换的结果照常落库。
    #[tokio::test]
    async fn parked_401_before_expiry_forces_one_rotation_and_retries_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime =
            FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        runtime.unauthorized_access_tokens.insert("A-u1".into());
        let p = provider_with(tmp.path(), runtime);
        let blob = blob_expiring_in("u1", 840, Some(900));
        let account = p.import_raw(blob, None, Some(false)).unwrap();

        p.query_quota(&account.id)
            .await
            .expect("强制轮换后重试应成功");

        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            1,
            "只强制轮换一次"
        );
        assert_eq!(
            p.test_runtime().fetch_calls.load(Ordering::SeqCst),
            2,
            "首次 401 后只重试一次"
        );
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("ROTATED")
        );
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            r#"{"uid":"u1","access_token":"ROTATED"}"#,
            "强制轮换的结果必须写回库存"
        );
    }

    /// 强制轮换被官方拒绝 → 按 DeadToken 指纹纪律记录在案，本次不重试查询，返回首个 401（停泊
    /// 401 口径）；之后再遇 401 也不再轮换。
    #[tokio::test]
    async fn parked_forced_rotation_rejected_records_fingerprint_and_stops_retrying() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime =
            FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Dead);
        runtime.unauthorized_access_tokens.insert("A-u1".into());
        let p = provider_with(tmp.path(), runtime);
        let blob = blob_expiring_in("u1", 840, Some(900));
        let account = p.import_raw(blob.clone(), None, Some(false)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();

        let err = p.query_quota(&account.id).await.unwrap_err();

        assert_eq!(err.http_status(), Some(401));
        assert_eq!(
            err.quota_failure().map(|failure| failure.kind),
            Some(QuotaFailureKind::Http),
            "按停泊 401 口径呈现，不归为凭证待续期"
        );
        assert_eq!(
            fs::read_to_string(p.test_home().join("fake-dead-refresh")).unwrap(),
            "R-u1",
            "被拒 refresh token 的指纹必须记录在案"
        );
        assert_eq!(
            p.test_runtime().fetch_calls.load(Ordering::SeqCst),
            1,
            "强制轮换被拒后不得重试查询"
        );
        assert_eq!(p.test_store_get("u1").unwrap(), blob, "账号库凭证保持不动");

        p.query_quota(&account.id).await.unwrap_err();
        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            1,
            "指纹在案：再遇 401 也不再轮换"
        );
    }

    /// 强制轮换成功但重试仍是 401 → 原样返回，按停泊 401 口径呈现（裸 401），不归为凭证待续期。
    #[tokio::test]
    async fn parked_retry_still_401_keeps_parked_401_presentation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime =
            FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        runtime.unauthorized_access_tokens.insert("A-u1".into());
        runtime.unauthorized_access_tokens.insert("ROTATED".into());
        let p = provider_with(tmp.path(), runtime);
        let blob = blob_expiring_in("u1", 840, Some(900));
        let account = p.import_raw(blob, None, Some(false)).unwrap();

        let err = p.query_quota(&account.id).await.unwrap_err();

        assert_eq!(err.http_status(), Some(401));
        assert_eq!(
            err.quota_failure().map(|failure| failure.kind),
            Some(QuotaFailureKind::Http),
            "按停泊 401 口径呈现，不归为凭证待续期"
        );
        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.test_runtime().fetch_calls.load(Ordering::SeqCst),
            2,
            "只重试一次"
        );
    }

    /// 假 runtime 声明的轮换请求超时上限，与 Kimi 的 10 秒同值。
    const FAKE_REFRESH_TIMEOUT: Duration = Duration::from_secs(10);

    /// 慢 401：未临期的停泊查询在单次查询预算（默认 20 秒）里用掉 15 秒才收到 401，剩下的 5 秒
    /// 盖不住轮换请求的超时上限（10 秒）加余量。此时照发强制轮换，外层超时会在轮换响应返回前
    /// 取消查询，服务端已消耗的一次性 refresh token 随之丢失。所以本次不发，按停泊 401 口径返回，
    /// 库存凭证原样不动；下一次查询拿到完整预算，强制轮换照常发出（耗满 10 秒也来得及）、落库、重试成功。
    #[tokio::test(start_paused = true)]
    async fn parked_slow_401_leaves_forced_rotation_to_next_query_with_full_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let budget = Duration::from_millis(kimi_switch_core::defaults::QUOTA_FETCH_TIMEOUT_MS);
        let mut runtime =
            FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        runtime.unauthorized_access_tokens.insert("A-u1".into());
        runtime.first_fetch_delay = budget - Duration::from_secs(5);
        runtime.refresh_delay = FAKE_REFRESH_TIMEOUT;
        let p = provider_with(tmp.path(), runtime);
        let blob = blob_expiring_in("u1", 840, Some(900));
        let account = p.import_raw(blob.clone(), None, Some(false)).unwrap();

        // 走生产的单次超时与重试包装：剩余预算只有它知道。
        let first = kimi_switch_core::query_quota_with_retry(&*p, &account.id).await;

        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            0,
            "剩余预算盖不住轮换请求的超时上限：强制轮换不得发出"
        );
        let err = first.expect_err("慢 401 按停泊 401 口径返回");
        assert_eq!(err.http_status(), Some(401));
        assert_eq!(
            err.quota_failure().map(|failure| failure.kind),
            Some(QuotaFailureKind::Http),
            "按停泊 401 口径呈现，不归为凭证待续期"
        );
        assert_eq!(p.test_store_get("u1").unwrap(), blob, "库存凭证原样不动");

        kimi_switch_core::query_quota_with_retry(&*p, &account.id)
            .await
            .expect("下一次查询拿到完整预算，强制轮换后重试应成功");

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("ROTATED")
        );
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            r#"{"uid":"u1","access_token":"ROTATED"}"#,
            "强制轮换的结果必须写回库存"
        );
    }

    /// 强制轮换同样走轮换写回比对：未临期的 u2 查询遇 401，强制轮换请求已发出时用户切到 u2。
    /// 放行后当前登录文件与库存凭证都持新凭证，重试用新 access token。
    #[tokio::test]
    async fn parked_forced_rotation_in_flight_then_switch_to_it_lands_new_token_in_live_and_store()
    {
        let tmp = tempfile::tempdir().unwrap();
        let gate = Arc::new(RotationGate::default());
        let mut runtime =
            FakeRuntime::with_rotation_gate(tmp.path().join("home"), gate.clone(), U2_ROTATED);
        runtime.unauthorized_access_tokens.insert("A-u2".into());
        let p = provider_with(tmp.path(), runtime);
        fs::create_dir_all(p.test_home()).unwrap();
        p.import_raw(U1_BLOB.into(), None, Some(true)).unwrap();
        let u2_blob = blob_expiring_in("u2", 840, Some(900));
        p.import_raw(u2_blob, None, Some(false)).unwrap();
        fs::write(p.test_live_path(), U1_BLOB).unwrap();
        let u2 = AccountId("u2".into());

        let query = p.query_quota(&u2);
        let switch = async {
            gate.wait_sent().await;
            p.activate(&u2).await.unwrap();
            gate.release();
        };
        let (quota, ()) = tokio::join!(query, switch);
        quota.expect("强制轮换后重试应成功");

        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            U2_ROTATED,
            "轮换在途时切到 u2，强制轮换的结果必须同步进当前登录文件"
        );
        assert_eq!(p.test_store_get("u2").unwrap(), U2_ROTATED);
        assert_eq!(
            p.test_runtime().fetch_calls.load(Ordering::SeqCst),
            2,
            "先用未临期的旧 access token 查询遇 401，再用新 access token 重试"
        );
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("A2-NEW")
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
    /// 本测试只钉失败瞬间的落库/落盘状态。之后能否保住账号库里的新凭证取决于回灌新旧比对
    /// （ADR-0019 决策 8）：可比较时旧 live 不再覆盖它；任一方解析不出过期时刻时仍以 live
    /// 为准，新轮换 refresh token 的唯一副本被销毁 → 轮换必被官方 invalid_grant 拒绝 →
    /// 记指纹停试 → 收敛到凭证过期型 401，需用户按引导重新授权。
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

    /// 采纳回灌同样按回灌新旧比对（ADR-0019 决策 8）：官方抢先轮换、本次采纳当前登录文件时，
    /// 账号库里的凭证比它还新，就不被覆盖；本次查询照用当前登录文件。
    #[tokio::test]
    async fn proactive_renewal_adoption_keeps_newer_stored_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime =
            FakeRuntime::with_proactive(tmp.path().join("home"), RefreshBehavior::Rotate);
        let official =
            r#"{"uid":"u1","refresh_token":"R9","access_token":"OFFICIAL","expires_at":2000}"#;
        runtime.official_live_during_wanted = Some(official.into());
        let p = provider_with(tmp.path(), runtime);
        let stored =
            r#"{"uid":"u1","refresh_token":"R-STORED","access_token":"STORED","expires_at":3000}"#;
        let old_live =
            r#"{"uid":"u1","refresh_token":"R1","access_token":"OLD","expires_at":1000}"#;
        let account = p.import_raw(stored.into(), None, Some(true)).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), old_live).unwrap();

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
            "查询照用官方刚写进当前登录文件的凭证"
        );
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            stored,
            "采纳回灌不得用较旧的 live 覆盖账号库里较新的凭证"
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

    /// 复现（spec #185 问题 1，#187）：当前账号 u1 的主动续期请求已发出时，用户切到 u2。
    /// 放行后续期结果不得写回当前登录文件、撤销切换：当前登录文件仍是 u2；续期得到的
    /// 新凭证仍落进 u1 的库存凭证，一次性 refresh token 的唯一有效副本不丢。
    #[tokio::test]
    async fn proactive_renewal_in_flight_then_switch_away_keeps_switch_and_stores_new_token() {
        const U1_RENEWED: &str = r#"{"uid":"u1","refresh_token":"R1-NEW","access_token":"A1-NEW"}"#;
        let tmp = tempfile::tempdir().unwrap();
        let gate = Arc::new(RotationGate::default());
        let runtime = FakeRuntime {
            proactive_wanted: true,
            ..FakeRuntime::with_rotation_gate(tmp.path().join("home"), gate.clone(), U1_RENEWED)
        };
        let p = provider_with(tmp.path(), runtime);
        fs::create_dir_all(p.test_home()).unwrap();
        let u1 = p.import_raw(U1_BLOB.into(), None, Some(true)).unwrap();
        let u2 = p.import_raw(U2_BLOB.into(), None, Some(false)).unwrap();
        fs::write(p.test_live_path(), U1_BLOB).unwrap();

        let query = p.query_quota(&u1.id);
        let switch_away = async {
            gate.wait_sent().await;
            p.activate(&u2.id).await.unwrap();
            gate.release();
        };
        let (quota, ()) = tokio::join!(query, switch_away);
        quota.unwrap();

        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            U2_BLOB,
            "续期在途时切走：续期结果不得写回当前登录文件、撤销切换"
        );
        assert_eq!(active_ids(&p), vec!["u2".to_string()]);
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            U1_RENEWED,
            "续期得到的新凭证必须落进原账号的库存凭证"
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

    /// issue #153：live 凭证原子写之后的文件级 fsync 失败必须进入既有回滚路径——
    /// 切换报错且 live 恢复为切换前内容，不静默成功。
    ///
    /// 注入旗标是线程本地：`activate` 把写入搬进 `spawn_blocking` 线程，测试线程
    /// 注入不可达，因此这里在测试线程直接以真实 `write_blob` 作为 SwapTarget 的
    /// writer 驱动 `swap_with_snapshot`（与 activate 内部同一结构），线程本地注入
    /// 可见。快照根目录由 provider 夹具经 `KIMI_SWITCH_HOME` 收口到用例 tempdir，
    /// 不触碰真实数据目录。
    #[test]
    fn live_write_fsync_failure_rolls_back_swap() {
        use kimi_switch_core::swap::{swap_with_snapshot, SwapTarget};

        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();
        let live = p.test_live_path();
        let u1_live = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        fs::write(&live, u1_live).unwrap();

        // 一次性注入：rename 成功后、fsync 之前失败。
        fail_next_live_sync();
        let blob = r#"{"uid":"u2","refresh_token":"R2","access_token":"A2"}"#.to_string();
        let err = swap_with_snapshot(
            "fake",
            &p.test_home(),
            tmp.path(),
            vec![SwapTarget {
                snapshot_name: "credentials",
                live_path: live.clone(),
                writer: Box::new(move |path: &Path| {
                    FileBlobProvider::<FakeRuntime>::write_blob(path, &blob)
                }),
            }],
            || Ok(()),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("injected live fsync failure"),
            "live fsync 失败必须上抛: {err}"
        );
        assert_eq!(
            fs::read_to_string(&live).unwrap(),
            u1_live,
            "fsync 失败应触发快照回滚，live 恢复为切换前内容"
        );

        // 注入解除后同一目标可正常切换（fsync 实际执行）。
        let blob2 = r#"{"uid":"u2","refresh_token":"R2","access_token":"A2"}"#.to_string();
        swap_with_snapshot(
            "fake",
            &p.test_home(),
            tmp.path(),
            vec![SwapTarget {
                snapshot_name: "credentials",
                live_path: live.clone(),
                writer: Box::new(move |path: &Path| {
                    FileBlobProvider::<FakeRuntime>::write_blob(path, &blob2)
                }),
            }],
            || Ok(()),
        )
        .unwrap();
        assert!(fs::read_to_string(&live).unwrap().contains("u2"));
    }

    // --- issue #154：切换写后回读身份校验与自动回滚 ---

    /// 零网络证明（沿用引擎零 HTTP 红线模式）：FakeRuntime 仅有的两个网络入口是
    /// `refresh` 与 `fetch_quota`，分别由 `refresh_calls` / `last_access_token` 观测。
    /// activate（含写后回读校验）全程零调用即证明校验纯本地、切换零网络。
    fn assert_activate_made_no_network_calls(p: &FileBlobProvider<FakeRuntime>) {
        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            0,
            "activate 全程不得调用 refresh（网络入口之一）"
        );
        assert!(
            p.test_runtime().last_access_token.lock().unwrap().is_none(),
            "activate 全程不得调用 fetch_quota（网络入口之一）"
        );
    }

    /// 正常切换：回读校验通过，对外行为无变化——live 换成目标 blob、active 标记
    /// 移到目标账号；且全程零网络调用。
    #[tokio::test]
    async fn activate_passes_post_swap_identity_verify_with_unchanged_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let u1_live = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let u2_blob = r#"{"uid":"u2","refresh_token":"R2","access_token":"A2"}"#;
        let u1 = p.import_raw(u1_live.into(), None, Some(true)).unwrap();
        let u2 = p.import_raw(u2_blob.into(), None, Some(false)).unwrap();
        fs::write(p.test_live_path(), u1_live).unwrap();

        p.activate(&u2.id).await.unwrap();

        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            u2_blob,
            "校验通过时 live 应为目标账号 blob，行为与既有切换一致"
        );
        let accounts = p.test_list_accounts().unwrap();
        let u1_after = accounts.iter().find(|a| a.id == u1.id).unwrap();
        let u2_after = accounts.iter().find(|a| a.id == u2.id).unwrap();
        assert!(!u1_after.active, "切走的账号应取消 active: {accounts:?}");
        assert!(u2_after.active, "目标账号应标 active: {accounts:?}");
        assert_activate_made_no_network_calls(&p);
    }

    /// 构造写入结果与目标身份不符：账号库 u2 的库存 blob 实际属于 u9（数据错乱
    /// 场景）。切换必须返回错误，且 live 文件与注册表均恢复/保持切换前状态。
    /// observed 是第三方身份（≠ 切换前 live 身份 u1）→ 不触发抢救（reviewer P2）：
    /// store 保持 capture-on-leave 回灌的切换前副本，错误按普通 mismatch 报告。
    #[tokio::test]
    async fn activate_rolls_back_live_and_registry_on_identity_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let u1_live = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let stranger_blob = r#"{"uid":"u9","refresh_token":"R9","access_token":"A9"}"#;
        let u1 = p.import_raw(u1_live.into(), None, Some(true)).unwrap();
        // 直接布置「id 为 u2 的账号卡 + 身份实为 u9 的库存 blob」：写入后回读
        // 解析出的官方身份 u9 与目标账号 u2 确证不符。
        p.test_registry_upsert(fake_account("u2")).unwrap();
        FileStore::new(tmp.path().join("creds.json"))
            .set("fake", "u2", "blob", stranger_blob)
            .unwrap();
        fs::write(p.test_live_path(), u1_live).unwrap();

        let err = p.activate(&AccountId("u2".into())).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("不一致"), "错误应说明身份不一致: {msg}");
        assert!(msg.contains("回滚"), "错误应说明已回滚到切换前状态: {msg}");
        assert!(
            !msg.contains("并发写回"),
            "第三方身份 mismatch 不应声称检测到官方并发写回: {msg}"
        );
        assert!(
            !msg.contains("rollback failed"),
            "回滚成功时错误不应声称回滚失败: {msg}"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            u1_live,
            "live 必须恢复为切换前内容"
        );
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            u1_live,
            "第三方身份 mismatch 不触发抢救覆写，store 保持 capture-on-leave 回灌的切换前副本"
        );
        let accounts = p.test_list_accounts().unwrap();
        let u1_after = accounts.iter().find(|a| a.id == u1.id).unwrap();
        let u2_after = accounts.iter().find(|a| a.id.0 == "u2").unwrap();
        assert!(
            u1_after.active,
            "registry 必须保持切换前 active 标记（仍在 u1）: {accounts:?}"
        );
        assert!(!u2_after.active, "目标账号不得被标 active: {accounts:?}");
        assert_activate_made_no_network_calls(&p);
    }

    /// 身份无法辨认（写入的 blob 解析不出任何身份）≠ 不一致：校验放行、切换照常
    /// 成功——写入内容来自账号库，解析不出身份是账号数据自身的问题，不是切换写错。
    #[tokio::test]
    async fn activate_skips_verify_when_written_identity_is_unidentifiable() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.test_home()).unwrap();

        let u1_live = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let opaque_blob = r#"{"opaque":"no-identity-here"}"#;
        p.import_raw(u1_live.into(), None, Some(true)).unwrap();
        p.test_registry_upsert(fake_account("u2")).unwrap();
        FileStore::new(tmp.path().join("creds.json"))
            .set("fake", "u2", "blob", opaque_blob)
            .unwrap();
        fs::write(p.test_live_path(), u1_live).unwrap();

        p.activate(&AccountId("u2".into())).await.unwrap();

        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            opaque_blob,
            "无法辨认身份时不得误回滚"
        );
        let accounts = p.test_list_accounts().unwrap();
        let u2_after = accounts.iter().find(|a| a.id.0 == "u2").unwrap();
        assert!(u2_after.active, "无法辨认身份时切换照常成功: {accounts:?}");
        assert_activate_made_no_network_calls(&p);
    }

    /// reviewer P2：官方客户端在写入与回读之间把旧账号（u1）新轮换的一次性
    /// refresh token 写回 live（注入 seam：live 恰为 writer 刚写入的目标 blob 时
    /// 覆写为旧账号新 token 版本）。mismatch 回滚前必须先把**当前 live 内容**
    /// 抢救进 u1 的 store（保留新 token，而非 capture-on-leave 早先灌入的切换前
    /// 副本），再恢复备份；错误仍按 mismatch 报告，live 与注册表恢复切换前状态。
    #[tokio::test]
    async fn activate_mismatch_salvages_official_rotated_live_of_previous_account_before_rollback()
    {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime = FakeRuntime::new(tmp.path().join("home"));
        let u1_live = r#"{"uid":"u1","refresh_token":"T1","access_token":"A1"}"#;
        let u2_blob = r#"{"uid":"u2","refresh_token":"RT-TARGET","access_token":"A2"}"#;
        // 官方为 u1 轮换出的新一次性 refresh token 版本（合成占位值）。
        let u1_rotated = r#"{"uid":"u1","refresh_token":"RT-ROTATED","access_token":"A1-NEW"}"#;
        runtime.official_overwrite_when_live_is =
            Some((u2_blob.to_string(), u1_rotated.to_string()));
        let p = provider_with(tmp.path(), runtime);
        fs::create_dir_all(p.test_home()).unwrap();

        p.import_raw(u1_live.into(), None, Some(true)).unwrap();
        p.test_registry_upsert(fake_account("u2")).unwrap();
        FileStore::new(tmp.path().join("creds.json"))
            .set("fake", "u2", "blob", u2_blob)
            .unwrap();
        fs::write(p.test_live_path(), u1_live).unwrap();

        let err = p.activate(&AccountId("u2".into())).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("不一致"), "错误仍应按身份不一致报告: {msg}");
        assert!(msg.contains("回滚"), "错误应说明已回滚: {msg}");
        assert!(
            !msg.contains("可能需要重新授权"),
            "抢救成功时不得声称需要重新授权: {msg}"
        );
        assert!(
            !msg.contains("rollback failed"),
            "回滚成功时错误不应声称回滚失败: {msg}"
        );

        assert_eq!(
            p.test_store_get("u1").unwrap(),
            u1_rotated,
            "回滚前必须把官方并发写回的当前 live（含新一次性 refresh token 唯一副本）\
             抢救进旧账号 store，而不是停留在 capture-on-leave 灌入的切换前副本"
        );
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            u1_live,
            "抢救后 live 仍须恢复为切换前内容（备份字节）"
        );
        let accounts = p.test_list_accounts().unwrap();
        let u1_after = accounts.iter().find(|a| a.id.0 == "u1").unwrap();
        let u2_after = accounts.iter().find(|a| a.id.0 == "u2").unwrap();
        assert!(u1_after.active, "registry 保持切换前标记: {accounts:?}");
        assert!(!u2_after.active, "目标账号不得被标 active: {accounts:?}");
        assert_activate_made_no_network_calls(&p);
    }

    // --- 夹具隔离：切换前快照不进真实用户数据目录 ---

    /// 切换前快照根目录由 `AppPaths::resolve()` 读 `KIMI_SWITCH_HOME` 决定，未设置时落到
    /// 真实用户数据目录（曾在真实目录留下 `fake-*` 快照）。经 provider 夹具切换时，快照
    /// 必须落在用例 tempdir。先断言快照根、再切换：隔离失效时在写盘前就失败。
    #[tokio::test]
    async fn activate_snapshot_stays_in_test_tempdir() {
        use kimi_switch_core::paths::AppPaths;

        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        let snapshots = AppPaths::resolve().unwrap().snapshots_dir();
        assert!(
            snapshots.starts_with(tmp.path()),
            "快照根目录必须收口到用例 tempdir，不得落到真实用户数据目录: {}",
            snapshots.display()
        );

        fs::create_dir_all(p.test_home()).unwrap();
        let u1_live = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
        let u2_blob = r#"{"uid":"u2","refresh_token":"R2","access_token":"A2"}"#;
        p.import_raw(u1_live.into(), None, Some(true)).unwrap();
        let u2 = p.import_raw(u2_blob.into(), None, Some(false)).unwrap();
        fs::write(p.test_live_path(), u1_live).unwrap();

        p.activate(&u2.id).await.unwrap();

        let names: Vec<String> = fs::read_dir(&snapshots)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.len(),
            1,
            "本次切换应在用例 tempdir 留下一份快照: {names:?}"
        );
        assert!(
            names[0].starts_with("fake-"),
            "快照目录按 provider 命名: {names:?}"
        );
    }

    // --- ADR-0019：轮换写回比对与 activate 读取进锁 ---

    const U1_BLOB: &str = r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#;
    const U2_BLOB: &str = r#"{"uid":"u2","refresh_token":"R2","access_token":"A2"}"#;
    const U2_ROTATED: &str = r#"{"uid":"u2","refresh_token":"R2-NEW","access_token":"A2-NEW"}"#;
    const U3_BLOB: &str = r#"{"uid":"u3","refresh_token":"R3","access_token":"A3"}"#;

    /// u1 为当前账号（live = u1），u2、u3 停泊；u2 的轮换会停在闸门上并返回 `U2_ROTATED`。
    fn gated_provider(tmp: &Path, gate: &Arc<RotationGate>) -> TestProvider {
        let runtime = FakeRuntime::with_rotation_gate(tmp.join("home"), gate.clone(), U2_ROTATED);
        let p = provider_with(tmp, runtime);
        fs::create_dir_all(p.test_home()).unwrap();
        p.import_raw(U1_BLOB.into(), None, Some(true)).unwrap();
        p.import_raw(U2_BLOB.into(), None, Some(false)).unwrap();
        p.import_raw(U3_BLOB.into(), None, Some(false)).unwrap();
        fs::write(p.test_live_path(), U1_BLOB).unwrap();
        p
    }

    fn active_ids(p: &FileBlobProvider<FakeRuntime>) -> Vec<String> {
        p.test_list_accounts()
            .unwrap()
            .into_iter()
            .filter(|a| a.active)
            .map(|a| a.id.0)
            .collect()
    }

    /// 复现（spec #185 问题 1）：后台额度查询为停泊账号 u2 轮换，请求已发出时用户切到 u2。
    /// 放行后当前登录文件与库存凭证都必须持新 refresh token；随后读取当前账号凭证
    /// （当前账号额度查询会顺手回灌库存）不得把库存降级回已被消耗的旧 token。
    #[tokio::test]
    async fn parked_rotation_in_flight_then_switch_to_it_lands_new_token_in_live_and_store() {
        let tmp = tempfile::tempdir().unwrap();
        let gate = Arc::new(RotationGate::default());
        let p = gated_provider(tmp.path(), &gate);
        let u2 = AccountId("u2".into());

        let query = p.query_quota(&u2);
        let switch = async {
            gate.wait_sent().await;
            p.activate(&u2).await.unwrap();
            gate.release();
        };
        let (quota, ()) = tokio::join!(query, switch);
        quota.unwrap();

        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            U2_ROTATED,
            "切到 u2 后，轮换结果必须同步进当前登录文件，否则官方客户端拿已作废的 R2 续期被拒"
        );
        assert_eq!(p.test_store_get("u2").unwrap(), U2_ROTATED);
        assert_eq!(active_ids(&p), vec!["u2".to_string()]);

        p.query_quota(&u2).await.unwrap();
        assert_eq!(
            p.test_store_get("u2").unwrap(),
            U2_ROTATED,
            "随后读取当前账号凭证不得把库存降级回被消耗的 R2"
        );
    }

    /// 停泊轮换在途时切到别的账号 u3：放行后 u2 的库存持新 token，当前登录文件仍是 u3。
    #[tokio::test]
    async fn parked_rotation_in_flight_then_switch_elsewhere_keeps_live_and_stores_new_token() {
        let tmp = tempfile::tempdir().unwrap();
        let gate = Arc::new(RotationGate::default());
        let p = gated_provider(tmp.path(), &gate);
        let u2 = AccountId("u2".into());

        let query = p.query_quota(&u2);
        let switch = async {
            gate.wait_sent().await;
            p.activate(&AccountId("u3".into())).await.unwrap();
            gate.release();
        };
        let (quota, ()) = tokio::join!(query, switch);
        quota.unwrap();

        assert_eq!(p.test_store_get("u2").unwrap(), U2_ROTATED);
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            U3_BLOB,
            "当前登录文件已经换人时，轮换结果绝不覆盖它"
        );
        assert_eq!(active_ids(&p), vec!["u3".to_string()]);
    }

    /// 轮换期间库存凭证被别处更新（这里是用户重新授权了 u2）：库存已不再持被消耗的 R2，
    /// 写回放弃覆盖，重新授权的结果保留。
    #[tokio::test]
    async fn parked_rotation_writeback_yields_when_store_was_updated_meanwhile() {
        let tmp = tempfile::tempdir().unwrap();
        let gate = Arc::new(RotationGate::default());
        let p = gated_provider(tmp.path(), &gate);
        let u2 = AccountId("u2".into());
        let reauthorized = r#"{"uid":"u2","refresh_token":"R2-REAUTH","access_token":"A2-REAUTH"}"#;

        let query = p.query_quota(&u2);
        let reauth = async {
            gate.wait_sent().await;
            p.replace_account_blob(&u2, reauthorized.into()).unwrap();
            gate.release();
        };
        let (quota, ()) = tokio::join!(query, reauth);
        quota.unwrap();

        assert_eq!(
            p.test_store_get("u2").unwrap(),
            reauthorized,
            "库存已被别处更新：轮换写回必须放弃覆盖"
        );
        assert_eq!(fs::read_to_string(p.test_live_path()).unwrap(), U1_BLOB);
    }

    /// 交换锁等不到（3 秒上限）：轮换期间 u2 已被切成当前账号，提交时交换锁一直被另一处占着。
    /// 不写当前登录文件；库存仍按比对写入新凭证，查询照常完成——新凭证不丢。
    #[tokio::test]
    async fn parked_rotation_commit_without_swap_lock_still_stores_new_token() {
        use std::sync::mpsc;
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let gate = Arc::new(RotationGate::default());
        let p = gated_provider(tmp.path(), &gate);
        let u2 = AccountId("u2".into());
        // 交换锁在注册表同目录。
        let lock_dir = tmp.path().to_path_buf();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let query = p.query_quota(&u2);
        let switch_then_hold_lock = async {
            gate.wait_sent().await;
            p.activate(&u2).await.unwrap();
            let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
            let holder = std::thread::spawn(move || {
                with_swap_lock(&lock_dir, || {
                    held_tx.send(()).ok();
                    release_rx.recv_timeout(Duration::from_secs(30)).ok();
                    Ok(())
                })
            });
            held_rx.await.expect("持锁线程应先进入交换锁");
            gate.release();
            holder
        };
        let (quota, holder) = tokio::join!(query, switch_then_hold_lock);
        release_tx.send(()).ok();
        holder.join().unwrap().unwrap();

        quota.expect("交换锁等不到只影响当前登录文件，查询照常用新凭证完成");
        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            U2_BLOB,
            "交换锁等不到时不得写当前登录文件"
        );
        assert_eq!(
            p.test_store_get("u2").unwrap(),
            U2_ROTATED,
            "交换锁等不到时库存仍按比对写入，新凭证不丢"
        );
    }

    /// 交换锁进不去（锁文件路径被一个目录占住，打开锁文件即失败；现实中如 sudo 跑过留下的
    /// root 属主锁文件）：临界区一步没执行，回退为只按比对写库存，新凭证不丢。
    #[tokio::test]
    async fn parked_rotation_commit_when_swap_lock_cannot_be_entered_still_stores_new_token() {
        let tmp = tempfile::tempdir().unwrap();
        let gate = Arc::new(RotationGate::default());
        let p = gated_provider(tmp.path(), &gate);
        let u2 = AccountId("u2".into());
        // 交换锁文件在注册表同目录。
        let lock_file = tmp.path().join(".kimi-switch.lock");

        let query = p.query_quota(&u2);
        let break_lock = async {
            gate.wait_sent().await;
            match fs::remove_file(&lock_file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("remove swap lock file: {e}"),
            }
            fs::create_dir(&lock_file).unwrap();
            gate.release();
        };
        let (quota, ()) = tokio::join!(query, break_lock);

        assert_eq!(
            p.test_store_get("u2").unwrap(),
            U2_ROTATED,
            "交换锁进不去时库存仍按比对写入，新凭证不丢"
        );
        assert_eq!(fs::read_to_string(p.test_live_path()).unwrap(), U1_BLOB);
        quota.expect("库存已保住新凭证：查询照用新凭证完成");
    }

    /// 轮换期间 u2 被切成当前账号，提交时库存写入出错：当前登录文件仍须拿到新凭证（此刻它是
    /// 新 refresh token 唯一能落的地方），库存错误只记 warn，查询照用新凭证完成。
    #[tokio::test]
    async fn parked_rotation_store_write_error_still_lands_new_token_in_live() {
        let tmp = tempfile::tempdir().unwrap();
        let _switch_home = isolate_switch_home(tmp.path());
        let gate = Arc::new(RotationGate::default());
        let seed = FileStore::new(tmp.path().join("creds.json"));
        seed.set("fake", "u1", "blob", U1_BLOB).unwrap();
        seed.set("fake", "u2", "blob", U2_BLOB).unwrap();
        let p = FileBlobProvider::new(
            FakeRuntime::with_rotation_gate(tmp.path().join("home"), gate.clone(), U2_ROTATED),
            Arc::new(FailingSetStore { inner: seed }),
            Arc::new(AccountRegistry::new(tmp.path().join("registry.toml"))),
        );
        let mut u1 = fake_account("u1");
        u1.active = true;
        p.test_registry_upsert(u1).unwrap();
        p.test_registry_upsert(fake_account("u2")).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        fs::write(p.test_live_path(), U1_BLOB).unwrap();
        let u2 = AccountId("u2".into());

        let query = p.query_quota(&u2);
        let switch = async {
            gate.wait_sent().await;
            p.activate(&u2).await.unwrap();
            gate.release();
        };
        let (quota, ()) = tokio::join!(query, switch);

        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            U2_ROTATED,
            "库存写不进时新凭证必须落进当前登录文件，否则两边都只剩已被消耗的 R2"
        );
        quota.expect("新凭证已落进当前登录文件：库存写入错误只记 warn，查询照用新凭证完成");
    }

    /// u1 为当前账号（live = u1），u2、u3 停泊；runtime 不轮换。
    fn three_account_provider(tmp: &Path) -> TestProvider {
        let p = provider(tmp);
        fs::create_dir_all(p.test_home()).unwrap();
        p.import_raw(U1_BLOB.into(), None, Some(true)).unwrap();
        p.import_raw(U2_BLOB.into(), None, Some(false)).unwrap();
        p.import_raw(U3_BLOB.into(), None, Some(false)).unwrap();
        fs::write(p.test_live_path(), U1_BLOB).unwrap();
        p
    }

    /// activate 的目标凭证读取在交换锁内：在「已调用 activate(u2)、尚未拿到交换锁」这一刻，
    /// 一次轮换提交（持交换锁写库存）把 u2 的库存换成了新凭证。activate 必须写入这份新凭证，
    /// 而不是调用之初读到的旧库存——否则当前登录文件拿到的是已被消耗的 R2。
    #[tokio::test]
    async fn activate_reads_target_credentials_inside_swap_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let p = three_account_provider(tmp.path());
        let lock_dir = tmp.path().to_path_buf();
        let creds = tmp.path().join("creds.json");
        before_next_activate_lock(&p.test_home(), move || {
            with_swap_lock(&lock_dir, || {
                FileStore::new(creds).set("fake", "u2", "blob", U2_ROTATED)
            })
            .unwrap();
        });

        p.activate(&AccountId("u2".into())).await.unwrap();

        assert_eq!(
            fs::read_to_string(p.test_live_path()).unwrap(),
            U2_ROTATED,
            "activate 必须写入进锁之后读到的目标凭证"
        );
        assert_eq!(active_ids(&p), vec!["u2".to_string()]);
    }

    /// 两个切换交错：在「已调用 activate(u3)、尚未拿到交换锁」这一刻，另一个切换把 u2 切成了
    /// 当前账号，官方客户端随即为 u2 轮换出新 token 写进当前登录文件。activate(u3) 的
    /// capture-on-leave 必须回灌这份 u2 的当前凭证，而不是调用之初看到的 u1。
    #[tokio::test]
    async fn interleaved_switches_capture_on_leave_keeps_intermediate_account_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let p = three_account_provider(tmp.path());
        let u2_official =
            r#"{"uid":"u2","refresh_token":"R2-OFFICIAL","access_token":"A2-OFFICIAL"}"#;
        let lock_dir = tmp.path().to_path_buf();
        let live = p.test_live_path();
        let registry_file = tmp.path().join("registry.toml");
        before_next_activate_lock(&p.test_home(), move || {
            with_swap_lock(&lock_dir, || {
                fs::write(&live, u2_official)?;
                AccountRegistry::new(registry_file).set_active("fake", &AccountId("u2".into()))
            })
            .unwrap();
        });

        p.activate(&AccountId("u3".into())).await.unwrap();

        assert_eq!(
            p.test_store_get("u2").unwrap(),
            u2_official,
            "capture-on-leave 漏掉了中间账号 u2 的当前凭证"
        );
        assert_eq!(fs::read_to_string(p.test_live_path()).unwrap(), U3_BLOB);
        assert_eq!(active_ids(&p), vec!["u3".to_string()]);
    }

    /// 包一层 `CredentialStore`，`set` 恒定失败（`set_if` 走默认实现，同样落到 `set`），
    /// 模拟回灌写不进账号库；`get`/`delete` 照常转发到内层 `FileStore`。
    struct FailingSetStore {
        inner: FileStore,
    }

    impl kimi_switch_core::CredentialStore for FailingSetStore {
        fn set(&self, _provider: &str, _account: &str, _field: &str, _value: &str) -> Result<()> {
            Err(Error::Credential("simulated store write failure".into()))
        }
        fn get(&self, provider: &str, account: &str, field: &str) -> Result<Option<String>> {
            self.inner.get(provider, account, field)
        }
        fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()> {
            self.inner.delete(provider, account, field)
        }
    }

    /// capture-on-leave 进锁之后失败仍只 warn、不阻断切换（ADR-0015 决策 11）。
    #[tokio::test]
    async fn activate_still_switches_when_capture_on_leave_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let _switch_home = isolate_switch_home(tmp.path());
        let seed = FileStore::new(tmp.path().join("creds.json"));
        seed.set("fake", "u1", "blob", U1_BLOB).unwrap();
        seed.set("fake", "u2", "blob", U2_BLOB).unwrap();
        let p = FileBlobProvider::new(
            FakeRuntime::new(tmp.path().join("home")),
            Arc::new(FailingSetStore { inner: seed }),
            Arc::new(AccountRegistry::new(tmp.path().join("registry.toml"))),
        );
        let mut u1 = fake_account("u1");
        u1.active = true;
        p.test_registry_upsert(u1).unwrap();
        p.test_registry_upsert(fake_account("u2")).unwrap();
        fs::create_dir_all(p.test_home()).unwrap();
        // live 里的 u1 已被官方轮换过，capture-on-leave 要回灌它，但账号库写不进去。
        let u1_live = r#"{"uid":"u1","refresh_token":"R1-OFFICIAL","access_token":"A1-OFFICIAL"}"#;
        fs::write(p.test_live_path(), u1_live).unwrap();

        p.activate(&AccountId("u2".into()))
            .await
            .expect("capture-on-leave 失败只 warn，不阻断切换");

        assert_eq!(fs::read_to_string(p.test_live_path()).unwrap(), U2_BLOB);
        assert_eq!(active_ids(&p), vec!["u2".to_string()]);
    }

    // --- ADR-0020：期望当前账号（带前置条件的切换）---

    /// 前置条件一致：期望当前账号 u1 仍是 registry 当前账号，切换与无前置条件的 activate
    /// 相同——当前登录文件换成 u2、当前标记移到 u2、留一份切换前快照，全程零网络。
    #[tokio::test]
    async fn activate_if_current_switches_when_expected_account_is_still_current() {
        use kimi_switch_core::paths::AppPaths;

        let tmp = tempfile::tempdir().unwrap();
        let p = three_account_provider(tmp.path());

        let outcome = p
            .activate_if_current(&AccountId("u2".into()), &AccountId("u1".into()))
            .await
            .unwrap();

        assert_eq!(outcome, ActivateOutcome::Activated);
        assert_eq!(fs::read_to_string(p.test_live_path()).unwrap(), U2_BLOB);
        assert_eq!(active_ids(&p), vec!["u2".to_string()]);
        let snapshots = fs::read_dir(AppPaths::resolve().unwrap().snapshots_dir())
            .unwrap()
            .count();
        assert_eq!(snapshots, 1, "前置条件一致时照常留一份切换前快照");
        assert_activate_made_no_network_calls(&p);
    }

    /// 复现（spec #185 用户故事 42）：自动切换决定从 u1 切到 u2 之后、activate 拿到交换锁
    /// 之前，用户手动切到了 u3（持交换锁写当前登录文件与 registry）。带前置条件的切换必须在
    /// 锁内看到这次手动切换并返回「已被取代」：当前登录文件、registry 与快照目录都保持手动
    /// 切换之后的样子，自动切换不覆盖用户的选择。
    #[tokio::test]
    async fn activate_if_current_is_superseded_by_manual_switch_before_swap_lock() {
        use kimi_switch_core::paths::AppPaths;

        let tmp = tempfile::tempdir().unwrap();
        let p = three_account_provider(tmp.path());
        let lock_dir = tmp.path().to_path_buf();
        let live = p.test_live_path();
        let registry_file = tmp.path().join("registry.toml");
        let after_manual = Arc::new(Mutex::new(None));
        let after_manual_probe = after_manual.clone();
        before_next_activate_lock(&p.test_home(), move || {
            with_swap_lock(&lock_dir, || {
                fs::write(&live, U3_BLOB)?;
                AccountRegistry::new(registry_file.clone())
                    .set_active("fake", &AccountId("u3".into()))
            })
            .unwrap();
            *after_manual_probe.lock().unwrap() =
                Some((fs::read(&live).unwrap(), fs::read(&registry_file).unwrap()));
        });

        let outcome = p
            .activate_if_current(&AccountId("u2".into()), &AccountId("u1".into()))
            .await
            .unwrap();

        assert_eq!(outcome, ActivateOutcome::Superseded);
        let (live_after_manual, registry_after_manual) = after_manual
            .lock()
            .unwrap()
            .take()
            .expect("手动切换探针应已运行");
        assert_eq!(
            fs::read(p.test_live_path()).unwrap(),
            live_after_manual,
            "已被取代时不得写当前登录文件"
        );
        assert_eq!(
            fs::read(tmp.path().join("registry.toml")).unwrap(),
            registry_after_manual,
            "已被取代时不得写 registry"
        );
        assert_eq!(active_ids(&p), vec!["u3".to_string()]);
        let snapshots = AppPaths::resolve().unwrap().snapshots_dir();
        assert!(
            !snapshots.exists() || fs::read_dir(&snapshots).unwrap().next().is_none(),
            "已被取代时不进切换骨架，不留快照: {}",
            snapshots.display()
        );
    }
}
