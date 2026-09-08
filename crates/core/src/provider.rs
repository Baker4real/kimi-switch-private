//! Provider 抽象。生产实现是 Kimi 的文件型引擎。

use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::model::{AccountId, Quota};
use async_trait::async_trait;

/// Provider 接口。
///
/// 设计要点：
/// - 所有可能阻塞的方法都是 async，统一在 tokio 上调度。
/// - 凭证读写不直接暴露 token；Provider 持有 [`crate::store::CredentialStore`] 引用。
/// - `activate` 必须保证失败回滚，由实现内部加文件锁。
/// - provider 标识由实现内部常量自报，不经本 trait。
#[async_trait]
pub trait Provider: Send + Sync {
    /// 把指定账号切为激活态。
    async fn activate(&self, id: &AccountId) -> Result<()>;

    /// 查询某账号的额度。可能返回多窗口（例如 5h + 7d）。
    /// 实现允许返回 `Vec` 为空表示"暂无可查"，但应优先返回 status=Unknown 的占位。
    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>>;

    /// 该账号是否出现凭证恢复信号：当前登录文件在 `failed_at` 之后被重写。
    ///
    /// 默认不放行。共享引擎只对当前账号比较 live 文件 mtime；停泊账号恒为 false。
    fn credential_changed_since(&self, _account_id: &AccountId, _failed_at: DateTime<Utc>) -> bool {
        false
    }
}
