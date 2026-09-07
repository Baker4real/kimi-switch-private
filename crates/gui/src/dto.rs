//! webview 可见的展示数据。禁止放入 token 或凭证 blob。

use serde::{Deserialize, Serialize};

/// 总览「额度告急」：任一窗口已用 ≥ 此比例。告急只看真实用量，额度查询失败不告急。
pub const URGENT_USED_RATIO: f32 = 0.8;

const UI_STYLES: &str = include_str!("../../../src-ui/styles.css");
const UI_QUOTA_TS: &str = include_str!("../../../src-ui/quota.ts");
const UI_API_TS: &str = include_str!("../../../src-ui/api.ts");

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

const fn all_needles_present(haystack: &[u8], needles: &[&[u8]]) -> bool {
    let mut i = 0;
    while i < needles.len() {
        if !slice_contains(haystack, needles[i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// 钉住前端字段声明 `  name:`，后端删改字段名时须同步改此列表，编译或 JSON 键测试变红。
macro_rules! pin_dto_fields {
    ($keys:ident, $src:expr, $($field:literal),+ $(,)?) => {
        #[cfg(test)]
        pub(crate) const $keys: &'static [&'static str] = &[$($field),+];
        $(
            const _: () = assert!(
                slice_contains($src.as_bytes(), concat!("  ", $field, ":").as_bytes()),
                concat!("frontend DTO missing field ", $field)
            );
        )+
    };
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
const _: () = assert!(
    slice_contains(
        UI_API_TS.as_bytes(),
        br#"export type MessageTone = "ok" | "err" | "info";"#,
    ),
    "src-ui/api.ts MessageTone union must match MessageTone"
);
const _: () = assert!(
    slice_contains(
        UI_API_TS.as_bytes(),
        br#""generic" | "auto_swap_success" | "auto_swap_no_candidate" | "auto_swap_failed";"#,
    ),
    "src-ui/api.ts UiMessageKind union must match UiMessageKind"
);
const _: () = assert!(
    slice_contains(UI_API_TS.as_bytes(), b"  kind: UiMessageKind;"),
    "src-ui/api.ts UiMessage.kind must be UiMessageKind"
);
const _: () = assert!(
    slice_contains(UI_API_TS.as_bytes(), b"  tone: MessageTone;"),
    "src-ui/api.ts UiMessage.tone must be MessageTone"
);
const _: () = assert!(
    all_needles_present(
        UI_API_TS.as_bytes(),
        &[
            br#"import type { QuotaView } from "./quota";"#,
            b"quotas: QuotaView[]",
        ],
    ),
    "src-ui/api.ts must import QuotaView and use it on AccountRow"
);

pin_dto_fields!(
    QUOTA_VIEW_JSON_KEYS,
    UI_QUOTA_TS,
    "window",
    "ratio",
    "text",
    "reset",
    "tone",
);
pin_dto_fields!(
    ACCOUNT_ROW_JSON_KEYS,
    UI_API_TS,
    "id",
    "label",
    "active",
    "membership",
    "membership_expiry",
    "membership_remaining_days",
    "membership_period_days",
    "membership_expiry_hint",
    "quotas",
    "error",
    "credential_expired",
    "cached_at",
    "display_name",
    "official_nickname_known",
);
pin_dto_fields!(
    SNAPSHOT_JSON_KEYS,
    UI_API_TS,
    "accounts",
    "current_label",
    "account_count",
    "urgent_count",
    "theme",
    "proactive_renewal",
    "auto_swap",
    "auto_swap_prefer_lower_tier",
    "auto_swap_resident_lower_tier",
    "message",
);
pin_dto_fields!(UI_MESSAGE_JSON_KEYS, UI_API_TS, "text", "tone", "kind",);

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

/// 快照 toast 严重度；serde 为 `ok` / `err` / `info`。未知值前端不得当完成吞掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageTone {
    Ok,
    Err,
    Info,
}

impl MessageTone {
    pub const ALL: [Self; 3] = [Self::Ok, Self::Err, Self::Info];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Err => "err",
            Self::Info => "info",
        }
    }
}

/// 快照 toast 语义；serde 为 snake_case。自动切换通知靠 kind 过滤，不靠中文。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiMessageKind {
    /// 导入、切换、绑定、节流提示等普通 toast。
    Generic,
    AutoSwapSuccess,
    AutoSwapNoCandidate,
    AutoSwapFailed,
}

impl UiMessageKind {
    pub const ALL: [Self; 4] = [
        Self::Generic,
        Self::AutoSwapSuccess,
        Self::AutoSwapNoCandidate,
        Self::AutoSwapFailed,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::AutoSwapSuccess => "auto_swap_success",
            Self::AutoSwapNoCandidate => "auto_swap_no_candidate",
            Self::AutoSwapFailed => "auto_swap_failed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UiMessage {
    pub text: String,
    pub tone: MessageTone,
    pub kind: UiMessageKind,
}

impl UiMessage {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: MessageTone::Ok,
            kind: UiMessageKind::Generic,
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: MessageTone::Err,
            kind: UiMessageKind::Generic,
        }
    }

    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: MessageTone::Info,
            kind: UiMessageKind::Generic,
        }
    }

    pub fn auto_swap_success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: MessageTone::Ok,
            kind: UiMessageKind::AutoSwapSuccess,
        }
    }

    pub fn auto_swap_no_candidate() -> Self {
        Self {
            text: "没有可自动切换的账号".into(),
            tone: MessageTone::Err,
            kind: UiMessageKind::AutoSwapNoCandidate,
        }
    }

    pub fn auto_swap_failed() -> Self {
        Self {
            text: "自动切换失败".into(),
            tone: MessageTone::Err,
            kind: UiMessageKind::AutoSwapFailed,
        }
    }
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
    /// 「自动切换时优先低档位账号」子开关（issue #71）：直读 config.toml，默认关。
    pub auto_swap_prefer_lower_tier: bool,
    /// 「额度恢复后回驻低档账号」子开关（issue #95）：直读 config.toml，默认关。
    pub auto_swap_resident_lower_tier: bool,
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
