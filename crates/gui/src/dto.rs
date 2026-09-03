//! webview 可见的展示数据。禁止放入 token 或凭证 blob。

use serde::{Deserialize, Serialize};

/// 总览「额度告急」：任一窗口已用 ≥ 此比例。告急只看真实用量，额度查询失败不告急。
pub const URGENT_USED_RATIO: f32 = 0.8;

const UI_STYLES: &str = include_str!("../../../src-ui/styles.css");
const UI_QUOTA_TS: &str = include_str!("../../../src-ui/quota.ts");

/// 额度条压力色；serde 为 `ok` / `warn` / `high` / `full`，对应 CSS `.quota-fill--*`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaTone {
    Ok,
    Warn,
    High,
    Full,
}

impl QuotaTone {
    pub const ALL: [Self; 4] = [Self::Ok, Self::Warn, Self::High, Self::Full];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::High => "high",
            Self::Full => "full",
        }
    }

    pub const fn css_selector(self) -> &'static str {
        match self {
            Self::Ok => ".quota-fill--ok",
            Self::Warn => ".quota-fill--warn",
            Self::High => ".quota-fill--high",
            Self::Full => ".quota-fill--full",
        }
    }
}

const fn slice_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        let mut j = 0;
        let mut matched = true;
        while j < needle.len() {
            if haystack[i + j] != needle[j] {
                matched = false;
                break;
            }
            j += 1;
        }
        if matched {
            return true;
        }
        i += 1;
    }
    false
}

const fn quota_tones_match_css() -> bool {
    let mut i = 0;
    while i < QuotaTone::ALL.len() {
        if !slice_contains(
            UI_STYLES.as_bytes(),
            QuotaTone::ALL[i].css_selector().as_bytes(),
        ) {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = assert!(
    quota_tones_match_css(),
    "QuotaTone CSS class missing from src-ui/styles.css"
);
const _: () = assert!(
    slice_contains(
        UI_QUOTA_TS.as_bytes(),
        br#"export type Tone = "ok" | "warn" | "high" | "full";"#,
    ),
    "src-ui/quota.ts Tone union must match QuotaTone"
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeId {
    Classic,
    JadeNight,
}

impl ThemeId {
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "jade-night" => Self::JadeNight,
            _ => Self::Classic,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::JadeNight => "jade-night",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaView {
    pub window: String,
    pub ratio: Option<f32>,
    pub text: String,
    pub reset: Option<String>,
    pub tone: QuotaTone,
}

impl QuotaView {
    pub fn tone(ratio: Option<f32>) -> QuotaTone {
        match ratio {
            Some(r) if r < 0.5 => QuotaTone::Ok,
            Some(r) if r < 0.8 => QuotaTone::Warn,
            Some(r) if r < 0.95 => QuotaTone::High,
            Some(_) => QuotaTone::Full,
            None => QuotaTone::Ok,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountRow {
    pub id: String,
    pub label: String,
    pub active: bool,
    pub membership: Option<String>,
    /// 会员到期日（东八区 `YYYY-MM-DD`，来自账号 extra 的到期瞬间，Rust 侧已格式化）。
    /// 无到期数据为 `None`；只是缺展示信息，不是错误，不计告急、不按错误红。
    /// 用户发起刷新成功时是刚拿到的新值；刷新失败时保留 extra 缓存旧值（#52）。
    pub membership_expiry: Option<String>,
    /// 剩余天数：`到期展示日 − 今天`（两边东八区日历日）。有到期日才有值。
    /// 正=剩余、0=今天到期、负=已过期天数。GUI 只拼中文，不解析日期。
    /// 会员到期不影响额度告急。
    pub membership_remaining_days: Option<i32>,
    /// 当前会员周期天数：`到期展示日 − currentStartTime 的东八区日历日`。
    /// 会员轨道「已度过比例」的分母；开始键缺失或坏数据为 `None`（不画填充）。
    pub membership_period_days: Option<i64>,
    /// 档位在且（无到期日，或本次用户发起刷新中会员获取失败）。布尔算法不变。
    /// GUI：无到期日 →「绑定后可显示有效期」+「绑定」；有到期日的刷新失败态 →
    /// 不写常驻提示句，按钮改为「重新绑定」。档位缺失时为 `false`。
    pub membership_expiry_hint: bool,
    pub quotas: Vec<QuotaView>,
    /// 失败展示文案。`credential_expired` 为 true 时是「凭证待续期」的引导文案，
    /// 否则是一般额度查询失败。
    pub error: Option<String>,
    /// 凭证待续期：凭证过期型 401（`QuotaFailureKind::CredentialExpired`）。
    /// 凭证生命周期状态而非查询故障：不计告急，前端不按错误红渲染。
    pub credential_expired: bool,
    /// 额度取得时间的本地展示文案：非陈旧为「更新于 {时分}」，陈旧回退为「缓存于 {时分}」。
    /// 查询失败且无缓存回退（含凭证待续期）为 `None`——没有可标注的取得时间。
    pub cached_at: Option<String>,
    /// 界面显示名（官方昵称 > 用户改过的别名 > 账号 id），由后端单一规则计算。
    pub display_name: String,
    /// 官方昵称缓存非空。已知时前端隐藏「重命名」入口。
    pub official_nickname_known: bool,
}

impl AccountRow {
    /// 告急只表示用量窗口真实达到阈值；额度查询失败（含凭证过期型 401）不告急。
    pub fn is_urgent(&self) -> bool {
        self.quotas
            .iter()
            .any(|q| q.ratio.is_some_and(|r| r >= URGENT_USED_RATIO))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UiMessage {
    pub text: String,
    /// `ok` / `err` / `info`
    pub tone: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub accounts: Vec<AccountRow>,
    pub current_label: String,
    pub account_count: u32,
    pub urgent_count: u32,
    pub theme: ThemeId,
    /// 「当前账号主动续期」开关（issue #42）：直读 config.toml，默认关。
    pub proactive_renewal: bool,
    /// 「额度耗尽自动切换」开关（issue #68）：直读 config.toml，默认关。
    pub auto_swap: bool,
    pub message: Option<UiMessage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthLink {
    pub url: String,
    pub user_code: String,
}

/// Tauri 命令错误：serde 内部标记为 `{ kind, message }`。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "message")]
#[serde(rename_all = "snake_case")]
pub enum CommandError {
    AccountNotFound(String),
    Credential(String),
    Config(String),
    Io(String),
    Json(String),
    QuotaFetch(String),
    Provider(String),
    Cancelled(String),
    NoPendingAuth(String),
    ReauthIdentityMismatch(String),
    ReauthUnidentifiable(String),
    ReauthLiveWriteFailed(String),
    Other(String),
}

impl CommandError {
    pub fn message(&self) -> &str {
        match self {
            Self::AccountNotFound(message)
            | Self::Credential(message)
            | Self::Config(message)
            | Self::Io(message)
            | Self::Json(message)
            | Self::QuotaFetch(message)
            | Self::Provider(message)
            | Self::Cancelled(message)
            | Self::NoPendingAuth(message)
            | Self::ReauthIdentityMismatch(message)
            | Self::ReauthUnidentifiable(message)
            | Self::ReauthLiveWriteFailed(message)
            | Self::Other(message) => message,
        }
    }

    pub fn from_anyhow(err: anyhow::Error) -> Self {
        let message = format!("{err:#}");
        for cause in err.chain() {
            if let Some(core) = cause.downcast_ref::<kimi_switch_core::Error>() {
                return Self::from_core(core, message);
            }
        }
        Self::Other(message)
    }

    fn from_core(core: &kimi_switch_core::Error, message: String) -> Self {
        use kimi_switch_core::{Error, ProviderKind, ReauthError};
        match core {
            Error::AccountNotFound { .. } => Self::AccountNotFound(message),
            Error::Credential(_) => Self::Credential(message),
            Error::Config(_) | Error::TomlSer(_) | Error::TomlDe(_) => Self::Config(message),
            Error::Io(_) => Self::Io(message),
            Error::Json(_) => Self::Json(message),
            Error::QuotaFetch(_) => Self::QuotaFetch(message),
            Error::Provider(failure) if failure.kind == ProviderKind::Cancelled => {
                Self::Cancelled(message)
            }
            Error::Provider(failure) if failure.kind == ProviderKind::NoPendingAuth => {
                Self::NoPendingAuth(message)
            }
            Error::Provider(_) => Self::Provider(message),
            Error::Reauth(ReauthError::IdentityMismatch { observed, target }) => {
                Self::ReauthIdentityMismatch(format!(
                    "登录的是账号 {observed}，不是这张卡（{target}），已拒绝更新凭证"
                ))
            }
            Error::Reauth(ReauthError::Unidentifiable) => {
                Self::ReauthUnidentifiable("无法从授权结果辨认官方账号，未更新这张卡".into())
            }
            Error::Reauth(ReauthError::LiveWriteFailed { cause }) => Self::ReauthLiveWriteFailed(
                format!(
                    "新凭证已保存到账号库，但写入当前登录文件失败：{cause}；当前仍在使用旧凭证，之后的查询或切换都会以旧凭证为准，请尽快重试重新授权"
                ),
            ),
            Error::Other(_) => Self::Other(message),
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}
