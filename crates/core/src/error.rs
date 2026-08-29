//! 统一错误类型。分类（鉴权失败 / 可否重试 / HTTP 状态）只在本类型上定义一次。
//! Provider 实现用 [`Error::provider`] / [`Error::quota_http`] 等构造器包装内部错误。

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 额度查询失败的种类。HTTP 状态另存在 [`QuotaFailure::status`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuotaFailureKind {
    Timeout,
    Http,
    Network,
    MissingCredential,
    #[default]
    Other,
}

/// 额度查询失败的结构化载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaFailure {
    pub kind: QuotaFailureKind,
    #[serde(default)]
    pub status: Option<u16>,
    pub message: String,
}

impl QuotaFailure {
    pub fn timeout() -> Self {
        Self {
            kind: QuotaFailureKind::Timeout,
            status: None,
            message: "quota fetch timeout".into(),
        }
    }

    pub fn http(status: u16, message: impl AsRef<str>) -> Self {
        Self {
            kind: QuotaFailureKind::Http,
            status: Some(status),
            message: message.as_ref().to_string(),
        }
    }

    pub fn network(message: impl AsRef<str>) -> Self {
        Self {
            kind: QuotaFailureKind::Network,
            status: None,
            message: message.as_ref().to_string(),
        }
    }

    pub fn missing_credential(message: impl AsRef<str>) -> Self {
        Self {
            kind: QuotaFailureKind::MissingCredential,
            status: None,
            message: message.as_ref().to_string(),
        }
    }

    pub fn other(message: impl AsRef<str>) -> Self {
        Self {
            kind: QuotaFailureKind::Other,
            status: None,
            message: message.as_ref().to_string(),
        }
    }
}

impl fmt::Display for QuotaFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Provider 路径失败种类。HTTP 状态另存在 [`ProviderFailure::status`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Cancelled,
    NoPendingAuth,
    #[default]
    Other,
}

/// Provider 失败的结构化载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFailure {
    pub kind: ProviderKind,
    #[serde(default)]
    pub status: Option<u16>,
    pub message: String,
}

impl fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// 按卡重新授权失败。身份门与写序见 ADR-0002；GUI 只把枚举映成文案，不重解析凭证。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReauthError {
    /// 授权结果的官方身份与目标卡不一致。
    IdentityMismatch { observed: String, target: String },
    /// 无法从授权结果辨认官方身份。
    Unidentifiable,
    /// 库存已写入新凭证，但 live 原子写失败。
    LiveWriteFailed { cause: String },
}

impl fmt::Display for ReauthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentityMismatch { observed, target } => write!(
                f,
                "authorized account {observed} does not match target account {target}; nothing written"
            ),
            Self::Unidentifiable => {
                f.write_str("cannot identify authorized account from credentials; nothing written")
            }
            Self::LiveWriteFailed { cause } => write!(
                f,
                "新凭证已保存到账号库，但写入当前登录文件失败：{cause}；当前仍在使用旧凭证，之后的查询或切换都会以旧凭证为准，请尽快重试重新授权"
            ),
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("account not found: provider={provider} id={id}")]
    AccountNotFound { provider: String, id: String },

    #[error("credential store: {0}")]
    Credential(String),

    #[error("config: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml serialize: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("toml deserialize: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("quota fetch: {0}")]
    QuotaFetch(QuotaFailure),

    #[error("provider: {0}")]
    Provider(ProviderFailure),

    #[error("reauth: {0}")]
    Reauth(ReauthError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    pub fn quota_timeout() -> Self {
        Self::QuotaFetch(QuotaFailure::timeout())
    }

    pub fn quota_http(status: u16, message: impl AsRef<str>) -> Self {
        Self::QuotaFetch(QuotaFailure::http(status, message))
    }

    pub fn quota_network(message: impl AsRef<str>) -> Self {
        Self::QuotaFetch(QuotaFailure::network(message))
    }

    pub fn quota_missing_credential(message: impl AsRef<str>) -> Self {
        Self::QuotaFetch(QuotaFailure::missing_credential(message))
    }

    pub fn quota_other(message: impl AsRef<str>) -> Self {
        Self::QuotaFetch(QuotaFailure::other(message))
    }

    pub fn quota_fetch(failure: QuotaFailure) -> Self {
        Self::QuotaFetch(failure)
    }

    pub fn provider(message: impl AsRef<str>) -> Self {
        Self::Provider(ProviderFailure {
            kind: ProviderKind::Other,
            status: None,
            message: message.as_ref().to_string(),
        })
    }

    pub fn provider_http(status: u16, message: impl AsRef<str>) -> Self {
        Self::Provider(ProviderFailure {
            kind: ProviderKind::Other,
            status: Some(status),
            message: message.as_ref().to_string(),
        })
    }

    pub fn cancelled(message: impl AsRef<str>) -> Self {
        Self::Provider(ProviderFailure {
            kind: ProviderKind::Cancelled,
            status: None,
            message: message.as_ref().to_string(),
        })
    }

    pub fn no_pending_auth() -> Self {
        Self::Provider(ProviderFailure {
            kind: ProviderKind::NoPendingAuth,
            status: None,
            message: "没有进行中的授权".into(),
        })
    }

    pub fn reauth(error: ReauthError) -> Self {
        Self::Reauth(error)
    }

    pub fn reauth_identity_mismatch(
        observed: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self::Reauth(ReauthError::IdentityMismatch {
            observed: observed.into(),
            target: target.into(),
        })
    }

    pub fn reauth_unidentifiable() -> Self {
        Self::Reauth(ReauthError::Unidentifiable)
    }

    pub fn reauth_live_write_failed(cause: impl fmt::Display) -> Self {
        Self::Reauth(ReauthError::LiveWriteFailed {
            cause: cause.to_string(),
        })
    }

    /// 结构化 HTTP 状态。与 Display 文案无关。
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::QuotaFetch(failure) => failure.status,
            Self::Provider(failure) => failure.status,
            _ => None,
        }
    }

    /// 是否鉴权/缺凭据。只看结构化状态，不嗅探文案。
    pub fn is_authentication_failure(&self) -> bool {
        match self {
            Self::QuotaFetch(failure) => {
                failure.kind == QuotaFailureKind::MissingCredential
                    || matches!(failure.status, Some(401 | 403))
            }
            Self::Provider(failure) => matches!(failure.status, Some(401 | 403)),
            _ => false,
        }
    }

    /// 额度查询是否值得再试。401 / 403 / 429 否，其余默认是。
    pub fn is_retryable(&self) -> bool {
        !matches!(self.http_status(), Some(401 | 403 | 429))
    }

    /// 耗尽重试后把尝试次数附到文案上，保留原分类与 HTTP 状态。
    pub fn with_attempt_count(self, max_attempts: u32) -> Self {
        if max_attempts <= 1 {
            return self;
        }
        match self {
            Self::QuotaFetch(failure) => Self::QuotaFetch(QuotaFailure {
                kind: failure.kind,
                status: failure.status,
                message: format!("{} after {max_attempts} attempts", failure.message),
            }),
            other => {
                let status = other.http_status();
                Self::QuotaFetch(QuotaFailure {
                    kind: QuotaFailureKind::Other,
                    status,
                    message: format!("{other} after {max_attempts} attempts"),
                })
            }
        }
    }

    pub fn quota_failure(&self) -> Option<&QuotaFailure> {
        match self {
            Self::QuotaFetch(failure) => Some(failure),
            _ => None,
        }
    }

    pub fn provider_kind(&self) -> Option<ProviderKind> {
        match self {
            Self::Provider(failure) => Some(failure.kind),
            _ => None,
        }
    }

    pub fn reauth_error(&self) -> Option<&ReauthError> {
        match self {
            Self::Reauth(error) => Some(error),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_401_is_auth_failure_and_not_retryable() {
        let err = Error::quota_http(401, "wording that never mentions the status");
        assert_eq!(err.http_status(), Some(401));
        assert!(err.is_authentication_failure());
        assert!(!err.is_retryable());
    }

    #[test]
    fn http_403_is_auth_failure_and_not_retryable() {
        let err = Error::quota_http(403, "wording that never mentions the status");
        assert_eq!(err.http_status(), Some(403));
        assert!(err.is_authentication_failure());
        assert!(!err.is_retryable());
    }

    #[test]
    fn http_429_is_not_auth_and_not_retryable() {
        let err = Error::quota_http(429, "wording that never mentions the status");
        assert_eq!(err.http_status(), Some(429));
        assert!(!err.is_authentication_failure());
        assert!(!err.is_retryable());
    }

    #[test]
    fn timeout_is_retryable_not_auth() {
        let err = Error::quota_timeout();
        assert_eq!(err.http_status(), None);
        assert!(!err.is_authentication_failure());
        assert!(err.is_retryable());
        assert!(err.to_string().contains("quota fetch timeout"));
    }

    #[test]
    fn missing_credential_is_auth_failure() {
        let err = Error::quota_missing_credential("schema may have changed");
        assert!(err.is_authentication_failure());
        assert_eq!(err.http_status(), None);
    }

    #[test]
    fn message_text_is_not_classification() {
        let err = Error::quota_other("kimi usages HTTP 401 Unauthorized: leftover wording");
        assert_eq!(err.http_status(), None);
        assert!(!err.is_authentication_failure());
        assert!(err.is_retryable());
    }

    #[test]
    fn attempt_count_preserves_http_status() {
        let err = Error::quota_http(401, "kimi usages HTTP 401 Unauthorized: expired")
            .with_attempt_count(2);
        assert_eq!(err.http_status(), Some(401));
        assert!(err.is_authentication_failure());
        assert!(!err.is_retryable());
        assert!(err.to_string().contains("after 2 attempts"));
    }

    #[test]
    fn cancelled_is_a_provider_kind() {
        let err = Error::cancelled("授权已取消");
        assert_eq!(err.provider_kind(), Some(ProviderKind::Cancelled));
        assert!(!err.is_authentication_failure());
        assert_eq!(err.to_string(), "provider: 授权已取消");
    }

    #[test]
    fn reauth_variants_keep_structured_payload() {
        let mismatch = Error::reauth_identity_mismatch("u9", "u1");
        assert_eq!(
            mismatch.reauth_error(),
            Some(&ReauthError::IdentityMismatch {
                observed: "u9".into(),
                target: "u1".into(),
            })
        );
        assert!(!mismatch.is_authentication_failure());

        let unknown = Error::reauth_unidentifiable();
        assert_eq!(unknown.reauth_error(), Some(&ReauthError::Unidentifiable));

        let live = Error::reauth_live_write_failed("os error 5");
        match live.reauth_error() {
            Some(ReauthError::LiveWriteFailed { cause }) => assert_eq!(cause, "os error 5"),
            other => panic!("{other:?}"),
        }
        assert!(live.to_string().contains("已保存到账号库"));
        assert!(live.to_string().contains("os error 5"));
    }
}
