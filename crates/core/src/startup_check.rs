//! 启动一致性自检（issue #155，spec #152 实现决策 8）：以 live 为准比对
//! live 登录文件身份与注册表 active 标记。
//!
//! - live 身份在账号库、active 标记却指向别人（或无人持有 / 多人持有）→ 按
//!   ADR-0002「对外一律以 live 为准」自动纠正 active 并记审计；
//! - live 身份不在账号库（用户在工具外重新登录过）→ 清除 active 标记并记审计，
//!   由调用方向用户提示可导入收进账号库（本模块只报告，不代跑导入）；
//! - 状态一致、live 不可读、身份无法辨认或墓碑命中 → 零副作用，调用方保持零输出。
//!
//! 身份辨认复用调用方注入的 `live_account_id`（引擎侧唯一口径：`parse_metadata`
//! → `official_identity`，与 #154 写后回读校验同一套辨认），库内归属再按引擎
//! `value_matches_account` 同口径补齐 label 维度；注册表写路径复用既有
//! `set_active` / `load`+`save`（临时文件 + rename + 文件级 fsync，issue #153）。
//! 纯本地读，零网络、零 daemon；审计事件只含账号 id，不含任何凭证内容。

use std::path::Path;

use crate::account_registry::AccountRegistry;
use crate::audit::{AuditEvent, AuditLog};
use crate::error::Result;
use crate::model::{Account, AccountId};
use crate::removed::RemovedAccounts;

/// 启动自检结果（issue #155）。一致或无法安全判定时统一为 [`Self::Consistent`]，
/// 调用方据此保持零可见输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupCheckOutcome {
    /// 状态一致，或无法安全判定（live 不可读 / 身份无法辨认 / 墓碑命中 / 内部错误）。
    /// 未产生任何写入，调用方不得有任何可见输出。
    Consistent,
    /// live 身份在账号库但 active 标记不符：已按 live 纠正并记审计。
    /// `account` = 纠正后的当前账号；`previous` = 纠正前错误持有标记的账号（无则 None）。
    CorrectedActive {
        account: AccountId,
        previous: Option<AccountId>,
    },
    /// live 身份不在账号库（工具外登录）：已清除 active 标记（若有）并记审计；
    /// 调用方应提示用户可导入收进账号库。
    UntrackedLiveIdentity { live: AccountId },
}

/// live 身份在库内的归属账号，按引擎 `value_matches_account` 同口径匹配：
/// 身份串等于账号 id，或（label 非空时）等于账号 label。去重键维度已由
/// 引擎侧 `live_account_id` 在解析时收进返回的 id。
fn owner_of<'a>(accounts: &'a [Account], live: &AccountId) -> Option<&'a Account> {
    accounts
        .iter()
        .find(|a| a.id == *live || (!a.label.trim().is_empty() && a.label == live.0))
}

/// 执行启动一致性自检并按需纠正。
///
/// 任何内部错误（注册表读 / 写失败）只记 warn 并按 [`StartupCheckOutcome::Consistent`]
/// 返回，绝不阻塞启动主流程——自检是修复性维护，不是门禁。
pub fn startup_consistency_check(
    provider_id: &str,
    removed_path: &Path,
    registry: &AccountRegistry,
    audit: &AuditLog,
    live_account_id: impl FnOnce() -> Result<AccountId>,
) -> StartupCheckOutcome {
    // 1. live 身份辨认失败（未登录 / live 不可读 / 解析不出身份）≠ 不一致：
    //    与 #154 回读校验同向——无法确证就不动注册表。
    let Ok(live) = live_account_id() else {
        return StartupCheckOutcome::Consistent;
    };
    // 2. 墓碑：用户显式删除过的账号仍留在 live 里，不纠正、也不提示导入。
    if RemovedAccounts::load(removed_path).contains(provider_id, &live.0) {
        tracing::debug!(id = %live, "skip startup check for tombstoned live identity");
        return StartupCheckOutcome::Consistent;
    }
    let accounts = match registry.list_by_provider(provider_id) {
        Ok(accounts) => accounts,
        Err(e) => {
            tracing::warn!(err = %e, "startup check: registry unreadable; skipping");
            return StartupCheckOutcome::Consistent;
        }
    };

    let Some(owner) = owner_of(&accounts, &live) else {
        // 3. live 身份不在账号库：清除该 provider 的 active 标记（以 live 为准，
        //    库内无人是当前账号），并报告导入提示信号。
        return clear_active_for_untracked(provider_id, &live, registry, audit);
    };

    // 4. 一致 = owner 持有标记且全库恰有一个 active；此时零写入、零审计。
    let active_count = accounts.iter().filter(|a| a.active).count();
    if owner.active && active_count == 1 {
        return StartupCheckOutcome::Consistent;
    }

    // 5. 不一致（标记指向别人 / 无人持有 / 多人持有）：以 live 为准纠正。
    let previous = accounts
        .iter()
        .find(|a| a.active && a.id != owner.id)
        .map(|a| a.id.clone());
    match registry.set_active(provider_id, &owner.id) {
        Ok(()) => {
            audit.append(
                AuditEvent::ok("startup_check", provider_id, Some(owner.id.0.as_str()))
                    .with_detail(match &previous {
                        Some(prev) => format!("align active to live: from={prev}"),
                        None => "align active to live: from=none".to_string(),
                    }),
            );
            StartupCheckOutcome::CorrectedActive {
                account: owner.id.clone(),
                previous,
            }
        }
        Err(e) => {
            tracing::warn!(err = %e, "startup check: correct active marker failed");
            StartupCheckOutcome::Consistent
        }
    }
}

/// live 身份不在账号库：清除该 provider 全部 active 标记（走既有 `load`+`save`
/// 原子写路径），留审计；本就没有标记时不写注册表、不留审计，只报告提示信号。
fn clear_active_for_untracked(
    provider_id: &str,
    live: &AccountId,
    registry: &AccountRegistry,
    audit: &AuditLog,
) -> StartupCheckOutcome {
    let mut all = match registry.load() {
        Ok(all) => all,
        Err(e) => {
            tracing::warn!(err = %e, "startup check: registry unreadable; skipping");
            return StartupCheckOutcome::Consistent;
        }
    };
    let mut cleared: Vec<String> = Vec::new();
    for account in &mut all {
        if account.provider == provider_id && account.active {
            account.active = false;
            cleared.push(account.id.0.clone());
        }
    }
    if cleared.is_empty() {
        return StartupCheckOutcome::UntrackedLiveIdentity { live: live.clone() };
    }
    match registry.save(&all) {
        Ok(()) => {
            audit.append(
                AuditEvent::ok("startup_check", provider_id, None).with_detail(format!(
                    "cleared active: live identity {live} not in library (was: {})",
                    cleared.join(",")
                )),
            );
            StartupCheckOutcome::UntrackedLiveIdentity { live: live.clone() }
        }
        Err(e) => {
            tracing::warn!(err = %e, "startup check: clear active marker failed");
            StartupCheckOutcome::Consistent
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::account_registry::AccountRegistry;
    use crate::audit::{AuditEvent, AuditLog};
    use crate::error::Error;
    use crate::model::Account;
    use crate::removed::RemovedAccounts;

    struct Harness {
        _tmp: tempfile::TempDir,
        registry: AccountRegistry,
        audit: AuditLog,
        removed_path: std::path::PathBuf,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            Self {
                registry: AccountRegistry::new(tmp.path().join("registry.toml")),
                audit: AuditLog::new(tmp.path().join("audit.log")),
                removed_path: tmp.path().join("removed.json"),
                _tmp: tmp,
            }
        }

        fn seed(&self, id: &str, active: bool) {
            self.registry
                .upsert(account(id, active))
                .expect("seed account");
        }

        fn check(&self, live: &str) -> StartupCheckOutcome {
            startup_consistency_check(
                "kimi",
                &self.removed_path,
                &self.registry,
                &self.audit,
                || Ok(AccountId(live.to_string())),
            )
        }

        fn active_ids(&self) -> Vec<String> {
            self.registry
                .load()
                .unwrap()
                .into_iter()
                .filter(|a| a.active)
                .map(|a| a.id.0)
                .collect()
        }

        fn audit_events(&self) -> Vec<AuditEvent> {
            std::fs::read_to_string(self.audit.path())
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }
    }

    fn account(id: &str, active: bool) -> Account {
        Account {
            provider: "kimi".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: Utc::now(),
            last_used_at: None,
            extra: Default::default(),
        }
    }

    /// live 身份在库、active 标记指向别人：纠正到 live、审计留痕。
    #[test]
    fn corrects_stale_active_marker_and_audits() {
        let h = Harness::new();
        h.seed("alice", true);
        h.seed("bob", false);

        let outcome = h.check("bob");

        assert_eq!(
            outcome,
            StartupCheckOutcome::CorrectedActive {
                account: AccountId("bob".into()),
                previous: Some(AccountId("alice".into())),
            }
        );
        assert_eq!(h.active_ids(), vec!["bob".to_string()]);
        let events = h.audit_events();
        assert_eq!(events.len(), 1, "纠正恰留一条审计");
        assert_eq!(events[0].action, "startup_check");
        assert_eq!(events[0].result, "ok");
        assert_eq!(events[0].account.as_deref(), Some("bob"));
        assert!(
            events[0]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("from=alice"),
            "审计 detail 应记录纠正前标记: {:?}",
            events[0].detail
        );
    }

    /// 无人持有标记（如手工编辑 / 历史半态）：从 none 纠正到 live。
    #[test]
    fn corrects_when_no_active_marker() {
        let h = Harness::new();
        h.seed("alice", false);
        h.seed("bob", false);

        let outcome = h.check("alice");

        assert_eq!(
            outcome,
            StartupCheckOutcome::CorrectedActive {
                account: AccountId("alice".into()),
                previous: None,
            }
        );
        assert_eq!(h.active_ids(), vec!["alice".to_string()]);
        let events = h.audit_events();
        assert_eq!(events.len(), 1);
        assert!(events[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("from=none"));
    }

    /// 多人持有标记（注册表损坏态）：收敛到 live 持有者、恰一个 active。
    #[test]
    fn collapses_multiple_active_markers_to_live_owner() {
        let h = Harness::new();
        h.seed("alice", true);
        h.seed("bob", true);
        h.seed("carol", false);

        let outcome = h.check("carol");

        assert_eq!(
            outcome,
            StartupCheckOutcome::CorrectedActive {
                account: AccountId("carol".into()),
                previous: Some(AccountId("alice".into())),
            }
        );
        assert_eq!(h.active_ids(), vec!["carol".to_string()]);
    }

    /// 状态一致：零副作用——注册表内容不变、审计文件不创建。
    #[test]
    fn consistent_state_is_zero_side_effect() {
        let h = Harness::new();
        h.seed("alice", true);
        h.seed("bob", false);
        let before = std::fs::read_to_string(h.registry.path()).unwrap();

        assert_eq!(h.check("alice"), StartupCheckOutcome::Consistent);

        assert_eq!(
            std::fs::read_to_string(h.registry.path()).unwrap(),
            before,
            "一致时不得重写注册表"
        );
        assert!(
            !h.audit.path().exists(),
            "一致时不得新增审计记录（文件都不应创建）"
        );
    }

    /// live 身份不在库（工具外登录）：清除 active、审计留痕、返回导入提示信号。
    #[test]
    fn untracked_live_clears_active_and_audits() {
        let h = Harness::new();
        h.seed("alice", true);
        h.seed("bob", false);

        let outcome = h.check("carol");

        assert_eq!(
            outcome,
            StartupCheckOutcome::UntrackedLiveIdentity {
                live: AccountId("carol".into()),
            }
        );
        assert!(h.active_ids().is_empty(), "active 标记应被清除");
        let events = h.audit_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].action, "startup_check");
        assert_eq!(events[0].result, "ok");
        let detail = events[0].detail.as_deref().unwrap_or_default();
        assert!(detail.contains("carol"), "detail 应含 live 身份: {detail}");
        assert!(
            detail.contains("alice"),
            "detail 应记录被清除的标记: {detail}"
        );
    }

    /// live 身份不在库且本无标记：不写注册表、不留审计，仍返回提示信号。
    #[test]
    fn untracked_live_without_marker_is_report_only() {
        let h = Harness::new();
        h.seed("alice", false);
        let before = std::fs::read_to_string(h.registry.path()).unwrap();

        let outcome = h.check("carol");

        assert_eq!(
            outcome,
            StartupCheckOutcome::UntrackedLiveIdentity {
                live: AccountId("carol".into()),
            }
        );
        assert_eq!(std::fs::read_to_string(h.registry.path()).unwrap(), before);
        assert!(!h.audit.path().exists());
    }

    /// 墓碑命中的 live 身份（用户删过的账号仍留在登录文件）：整体跳过，
    /// 不纠正、不清标记、不提示导入。
    #[test]
    fn tombstoned_live_identity_is_skipped_entirely() {
        let h = Harness::new();
        RemovedAccounts::load(&h.removed_path)
            .add("kimi", "ghost")
            .unwrap();
        h.seed("alice", true);

        let outcome = h.check("ghost");

        assert_eq!(outcome, StartupCheckOutcome::Consistent);
        assert_eq!(
            h.active_ids(),
            vec!["alice".to_string()],
            "墓碑命中不得动标记"
        );
        assert!(!h.audit.path().exists());
    }

    /// live 不可读 / 身份无法辨认：无法确证 ≠ 不一致，零副作用跳过。
    #[test]
    fn unreadable_live_identity_is_skipped() {
        let h = Harness::new();
        h.seed("alice", true);

        let outcome =
            startup_consistency_check("kimi", &h.removed_path, &h.registry, &h.audit, || {
                Err(Error::provider("read live credentials failed"))
            });

        assert_eq!(outcome, StartupCheckOutcome::Consistent);
        assert_eq!(h.active_ids(), vec!["alice".to_string()]);
        assert!(!h.audit.path().exists());
    }

    /// 库内归属按引擎同口径补 label 维度：身份串命中账号 label 也算在库
    /// （与 #154 回读校验 `metadata_matches` 的比较范围一致）。
    #[test]
    fn owner_matched_via_account_label() {
        let h = Harness::new();
        let mut labeled = account("old-key", false);
        labeled.label = "user@example.com".into();
        h.registry.upsert(labeled).unwrap();
        h.seed("alice", true);

        let outcome = h.check("user@example.com");

        assert_eq!(
            outcome,
            StartupCheckOutcome::CorrectedActive {
                account: AccountId("old-key".into()),
                previous: Some(AccountId("alice".into())),
            }
        );
        assert_eq!(h.active_ids(), vec!["old-key".to_string()]);
    }

    /// label 归属一致时同样零副作用（owner 恰好已持有标记）。
    #[test]
    fn owner_matched_via_label_consistent_is_zero_side_effect() {
        let h = Harness::new();
        let mut labeled = account("old-key", true);
        labeled.label = "user@example.com".into();
        h.registry.upsert(labeled).unwrap();
        let before = std::fs::read_to_string(h.registry.path()).unwrap();

        assert_eq!(h.check("user@example.com"), StartupCheckOutcome::Consistent);
        assert_eq!(std::fs::read_to_string(h.registry.path()).unwrap(), before);
        assert!(!h.audit.path().exists());
    }

    /// 空 label 不得参与归属匹配（引擎 `value_matches_account` 的空串防护同款）。
    #[test]
    fn blank_label_never_matches() {
        let h = Harness::new();
        let mut blank = account("old-key", true);
        blank.label = "   ".into();
        h.registry.upsert(blank).unwrap();

        let outcome = h.check("whatever");

        assert_eq!(
            outcome,
            StartupCheckOutcome::UntrackedLiveIdentity {
                live: AccountId("whatever".into()),
            }
        );
        assert!(h.active_ids().is_empty(), "空 label 命中不该保住标记");
    }
}
