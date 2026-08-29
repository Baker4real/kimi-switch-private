//! kimi-switch 核心：数据模型 + Provider 抽象 + 凭证仓库 + 路径与配置。

pub mod account_registry;
pub mod audit;
pub mod defaults;
pub mod error;
pub mod model;
pub mod paths;
pub mod provider;
pub mod quota_cache;
pub mod quota_query;
pub mod quota_snapshot;
pub mod remove_account;
pub mod removed;
pub mod settings;
pub mod store;
pub mod swap;
mod switch_lock;
pub mod sync_active;
pub mod time;

pub use account_registry::AccountRegistry;
pub use audit::{AuditEvent, AuditLog};
pub use error::{
    Error, ProviderFailure, ProviderKind, QuotaFailure, QuotaFailureKind, ReauthError, Result,
};
pub use model::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};
pub use provider::Provider;
pub use quota_cache::{CachedEntry, QuotaCache, ValidEntry};
pub use quota_query::query_quota_with_retry;
pub use quota_snapshot::{QuotaSnapshot, QuotaState};
pub use remove_account::remove_account;
pub use removed::RemovedAccounts;
pub use store::{CredentialStore, FileStore, KeyringStore};
pub use sync_active::sync_local_active;
