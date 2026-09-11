//! kimi-switch 核心：数据模型、Provider、凭证仓库、额度快照、路径、配置与自动切换决策。

pub mod account_registry;
pub mod audit;
pub mod auto_swap;
pub mod auto_swap_opt_out;
pub mod defaults;
pub mod display_name;
pub mod error;
pub mod membership_expiry;
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
pub use auto_swap::{
    burn_trip_from_target, decide_auto_swap, decide_auto_swap_from_state, decide_resident_return,
    decide_resident_return_from_state, read_burn_trip, read_last_auto_swap_at,
    read_manual_hold_until, read_resident_confirm_counts, record_auto_swap_success,
    set_manual_hold_until, write_burn_trip_and_last_auto_swap_at, write_last_auto_swap_at,
    write_resident_confirm_counts, AutoSwapDecision, AutoSwapKind, AutoSwapReturn,
    AutoSwapSkipReason, AutoSwapTarget, BurnTrip,
};
pub use auto_swap_opt_out::{
    account_auto_swap_opt_out, auto_swap_opt_out_from_extra, persist_auto_swap_opt_out,
    write_auto_swap_opt_out_extra, EXTRA_AUTO_SWAP_OPT_OUT,
};
pub use display_name::{
    display_name, official_nickname_from_extra, official_nickname_known, persist_official_nickname,
    write_official_nickname_extra, EXTRA_OFFICIAL_NICKNAME,
};
pub use error::{
    BindWebError, Error, ProviderFailure, ProviderKind, QuotaFailure, QuotaFailureKind,
    ReauthError, Result,
};
pub use membership_expiry::{
    format_membership_expiry, membership_expiry_from_extra, membership_period_days,
    membership_remaining_days, membership_start_from_extra, prettify_membership,
    write_membership_extra, EXTRA_CURRENT_END_TIME, EXTRA_CURRENT_START_TIME,
    EXTRA_NEXT_BILLING_TIME, WEB_STORE_FIELD,
};
pub use model::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};
pub use provider::Provider;
pub use quota_cache::{CachedEntry, QuotaCache, ValidEntry};
pub use quota_query::query_quota_with_retry;
pub use quota_snapshot::{
    earliest_reset_at, QuotaCollectOutcome, QuotaCollectReason, QuotaCollectStats, QuotaSnapshot,
    QuotaState,
};
pub use remove_account::remove_account;
pub use removed::RemovedAccounts;
pub use store::{CredentialStore, FileStore, KeyringStore};
pub use sync_active::sync_local_active;
