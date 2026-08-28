//! webview 可见的展示数据。禁止放入 token 或凭证 blob。

use serde::{Deserialize, Serialize};

/// 总览「额度告急」：任一窗口已用 ≥ 此比例。
pub const URGENT_USED_RATIO: f32 = 0.8;

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
    /// `ok` / `warn` / `high` / `full`，对应用量压力色。
    pub tone: String,
}

impl QuotaView {
    pub fn tone(ratio: Option<f32>) -> &'static str {
        match ratio {
            Some(r) if r < 0.5 => "ok",
            Some(r) if r < 0.8 => "warn",
            Some(r) if r < 0.95 => "high",
            Some(_) => "full",
            None => "ok",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountRow {
    pub id: String,
    pub label: String,
    pub active: bool,
    pub membership: Option<String>,
    pub quotas: Vec<QuotaView>,
    pub error: Option<String>,
}

impl AccountRow {
    pub fn is_urgent(&self) -> bool {
        self.error.is_some()
            || self
                .quotas
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
    pub message: Option<UiMessage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthLink {
    pub url: String,
    pub user_code: String,
}
