//! 文件型 provider 的 adapter 契约：每个 runtime 只实现差异点，机制在 [`crate::engine`]。

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use kimi_switch_core::error::{Error, Result};
use kimi_switch_core::{Account, Quota};

/// 从凭证 blob 解析出的最小元数据。所有可选字段最终可能进 registry.toml，
/// 注意：写进 `extra` 的 `Option` 值不要保留 null（引擎只写 `Some`）。
#[derive(Debug, Clone, Default)]
pub struct BlobMetadata {
    /// account 主键候选（如 Beta account_key / Kimi user_id）。
    pub primary_id: Option<String>,
    /// 展示 label（如 email / user_id）。
    pub label: Option<String>,
    /// 跨主键去重用的稳定键（如某些客户端的 account id 字段）；无则 None。
    pub dedup_key: Option<String>,
    /// 额外落进 registry.toml `extra` 的字段（provider 私有，如会员档/额度用 header）。
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 凭证的过期信息（ADR-0019）：回灌新旧比对据过期时刻判断两份凭证谁更新，停泊按需轮换据它
/// 与寿命判定临期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialExpiry {
    /// 过期时刻（Unix 秒）：access token 的 JWT `exp` 优先，缺失时回退凭证的 `expires_at`。
    pub expires_at: i64,
    /// 凭证声明的 access token 寿命（秒，即 `expires_in`）；缺失为 `None`。
    pub expires_in: Option<i64>,
}

/// 刷新结果。
pub enum RefreshOutcome {
    /// 刷新成功，返回轮换后的完整 blob。
    Rotated(String),
    /// refresh token 已失效（invalid_grant / 401 / 403），需重新登录。
    DeadToken,
    /// 该 runtime 不支持刷新（如纯 API key）。
    Unsupported,
}

/// 每个文件型 runtime 的差异点契约。机制（切换/回滚/回灌）在引擎里，不在这里。
#[async_trait]
pub trait FileBlobRuntime: Send + Sync + 'static {
    /// provider 标识，如 "kimi"。
    fn id(&self) -> &'static str;
    /// store 里存 blob 的字段名。默认 "blob"；Beta 为兼容历史数据返回 "auth_json"。
    fn store_field(&self) -> &'static str {
        "blob"
    }
    /// registry.toml `extra` 里存跨主键去重键的字段名。默认 "dedup_key"；
    /// 可覆盖为旧字段名（如 "legacy_account_id"）以兼容迁移前已存在的账号数据
    /// （迁移前的 `registry.toml` 只有这个键名，没有通用的 "dedup_key"）。
    fn dedup_extra_key(&self) -> &'static str {
        "dedup_key"
    }
    /// 解析 provider 工作目录（读 env + 默认目录）。
    fn home(&self) -> PathBuf;
    /// 工作目录内的 live 凭证文件路径。
    fn live_cred_path(&self, home: &Path) -> PathBuf;
    /// 从 blob 抽最小元数据。解析失败返回 `Default`（透传策略，不 panic）。
    fn parse_metadata(&self, blob: &str) -> BlobMetadata;

    /// 刷新一个 blob，返回轮换后的完整 blob。对 parked 账号按需调用；
    /// 当前账号仅在主动续期判定通过时调用（见 `proactive_renewal_wanted`）。
    async fn refresh(&self, blob: &str) -> Result<RefreshOutcome>;
    /// 查询额度。parked 账号的 access token 临期时引擎已先轮换，未临期遇 401 时引擎强制
    /// 轮换一次再重试一次（停泊按需轮换，ADR-0019；剩余时间不够时留给下次查询，见
    /// [`Self::refresh_timeout`]）；当前账号开启主动续期时已按需轮换。
    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>>;

    /// 可选：在 store/live 都拿不到时，从 provider 私有 legacy 布局恢复 blob。默认无。
    fn recover_legacy(&self, _home: &Path, _account: &Account) -> Option<String> {
        None
    }

    /// 凭证的过期信息（ADR-0019）：过期时刻优先取 access token 的 JWT `exp`，缺失时回退凭证里的
    /// `expires_at`，`expires_in` 一并读出；过期时刻解析不出为 `None`。阈值与比对都在引擎：
    /// 回灌新旧比对任一方为 `None` 即不可比较、维持以 live 为准；停泊按需轮换为 `None` 时
    /// 先轮换再查询。默认 `None`：不提供该能力的 runtime 回灌照旧以 live 为准，停泊账号与旧版
    /// 一样每次查询前都轮换。
    fn credential_expiry(&self, blob: &str) -> Option<CredentialExpiry> {
        let _ = blob;
        None
    }

    /// 调用 [`Self::refresh`] 最多多久必定返回（响应或错误，含发请求前的本地准备）。停泊账号
    /// 遇 401 的强制轮换只在本次额度查询剩余时间盖得住它加余量时才发，免得外层超时在响应返回前
    /// 取消查询、丢掉服务端已消耗的一次性 refresh token（ADR-0019）。
    /// 默认 `None`：上限不明，有外层超时时一律不强制轮换（fail-closed）。
    fn refresh_timeout(&self) -> Option<Duration> {
        None
    }

    // -----------------------------------------------------------------------
    // 当前账号主动续期（issue #44，ADR-0003 第 2 条）。
    // 四个钩子的默认实现都是「不主动续期」：不提供该能力的 runtime 与
    // 有官方锁协议的平台行为完全不变（维持官方锁协调）。机制（现读 live /
    // 轮换 / 先库后 live 写回）在引擎里，runtime 只回答差异点。
    // -----------------------------------------------------------------------

    /// 查询前是否应主动轮换这份当前账号 blob。
    ///
    /// 实现须一次说清三件事：设置开关（issue #42）已开启、本平台组合**确定无**
    /// 官方刷新锁协议（fail-closed：探测失败/不可解析/版本不识一律不放行）、
    /// access token 已过期或剩余不足轮换窗口。默认 false。
    async fn proactive_renewal_wanted(&self, blob: &str) -> bool {
        let _ = blob;
        false
    }

    /// DeadToken 纪律检查：该 blob 的 refresh token 是否已被官方拒绝在案
    /// （指纹命中）。在案则停止重试。默认 false。
    fn proactive_renewal_blocked(&self, home: &Path, blob: &str) -> bool {
        let _ = (home, blob);
        false
    }

    /// DeadToken 纪律处置：记录被拒 refresh token 的指纹（幂等）并停止重试；
    /// 本地凭证不删除、不墓碑化。返回本次查询应呈现的错误——「凭证过期型 401」
    /// 引导文案（issue #41）。
    fn proactive_renewal_reject(&self, home: &Path, blob: &str) -> Result<Error> {
        let _ = (home, blob);
        Ok(Error::quota_credential_expired(
            "proactive renewal rejected by the official client; re-authorize the account",
        ))
    }

    /// 轮换成功收尾：只清掉这次轮换前 blob 对应的拒止指纹，其它账号的指纹保留。
    /// 默认 no-op。
    fn proactive_renewal_rotated(&self, home: &Path, blob: &str) -> Result<()> {
        let _ = (home, blob);
        Ok(())
    }
}
