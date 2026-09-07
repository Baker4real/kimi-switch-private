//! 额度耗尽自动切换：纯决策 + 上次切换时刻的可丢弃状态。
//!
//! 决策函数无 IO、无网络；冷却时刻读写失败静默忽略。

use std::cmp::Ordering;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{Account, AccountId, Quota, QuotaWindow};
use crate::quota_snapshot::{QuotaSnapshot, QuotaState};
use crate::settings::AutoSwap;

/// 不切换的原因。调用方只在「本应触发但无合格候选」时提示用户。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoSwapSkipReason {
    /// 开关关闭。
    Disabled,
    /// 账号库没有当前账号。
    NoActiveAccount,
    /// 距上次自动切换成功不足冷却间隔。
    Cooldown,
    /// 当前账号仍在安顿宽限期内（刚被手动选中）。
    SettleGrace,
    /// 当前账号本次额度不可用（Failed / stale 回落 / 凭证待续期 / 无快照）。
    CurrentUnavailable,
    /// 当前账号没有已知 limit 的窗口。
    CurrentUnknown,
    /// 当前账号已用比例低于阈值。
    BelowThreshold,
    /// 已达阈值但没有合格停泊账号。
    NoEligibleCandidate,
}

/// 切到哪个账号，以及触发判定用的窗口。
#[derive(Debug, Clone, PartialEq)]
pub struct AutoSwapTarget {
    pub from: AccountId,
    pub to: AccountId,
    /// 当前账号达到阈值的窗口中，已用比例最高者。
    pub trigger_window: QuotaWindow,
    /// 该窗口已用比例 0.0~1.0。
    pub usage_ratio: f64,
}

/// 自动切换判定结果。
#[derive(Debug, Clone, PartialEq)]
pub enum AutoSwapDecision {
    Stay { reason: AutoSwapSkipReason },
    Swap(AutoSwapTarget),
}

/// 根据已收集的额度快照判定是否自动切换。纯函数，不碰盘、不联网。
pub fn decide_auto_swap(
    accounts: &[Account],
    snapshots: &[QuotaSnapshot],
    settings: &AutoSwap,
    now: DateTime<Utc>,
    last_auto_swap_at: Option<DateTime<Utc>>,
) -> AutoSwapDecision {
    if !settings.enabled {
        return stay(AutoSwapSkipReason::Disabled);
    }

    let Some(current) = accounts.iter().find(|account| account.active) else {
        return stay(AutoSwapSkipReason::NoActiveAccount);
    };

    if in_cooldown(now, last_auto_swap_at, settings.cooldown_ms) {
        return stay(AutoSwapSkipReason::Cooldown);
    }

    if in_settle_grace(now, current.last_used_at, settings.settle_grace_ms) {
        return stay(AutoSwapSkipReason::SettleGrace);
    }

    let Some(current_snap) = snapshot_of(snapshots, &current.id) else {
        return stay(AutoSwapSkipReason::CurrentUnavailable);
    };
    let Some(current_quotas) = usable_quotas(current_snap) else {
        return stay(AutoSwapSkipReason::CurrentUnavailable);
    };
    let Some((trigger_window, usage_ratio)) = trigger_window(current_quotas, settings.threshold)
    else {
        if known_ratios(current_quotas).is_empty() {
            return stay(AutoSwapSkipReason::CurrentUnknown);
        }
        return stay(AutoSwapSkipReason::BelowThreshold);
    };

    let mut eligible: Vec<(&Account, &[Quota], Option<&str>)> = accounts
        .iter()
        .filter(|account| account.id != current.id)
        .filter_map(|account| {
            let snap = snapshot_of(snapshots, &account.id)?;
            let quotas = usable_quotas(snap)?;
            if !is_eligible_candidate(quotas, settings.threshold) {
                return None;
            }
            Some((account, quotas, snap.membership.as_deref()))
        })
        .collect();

    if eligible.is_empty() {
        return stay(AutoSwapSkipReason::NoEligibleCandidate);
    }

    eligible.sort_by(|(a, aq, am), (b, bq, bm)| {
        compare_candidates(a, aq, *am, b, bq, *bm, settings.prefer_lower_tier)
    });
    let (best, _, _) = eligible[0];
    AutoSwapDecision::Swap(AutoSwapTarget {
        from: current.id.clone(),
        to: best.id.clone(),
        trigger_window,
        usage_ratio,
    })
}

fn stay(reason: AutoSwapSkipReason) -> AutoSwapDecision {
    AutoSwapDecision::Stay { reason }
}

fn snapshot_of<'a>(snapshots: &'a [QuotaSnapshot], id: &AccountId) -> Option<&'a QuotaSnapshot> {
    snapshots.iter().find(|snapshot| snapshot.account_id == *id)
}

/// Ready 一律可用；Cached 仅在非 stale 回落时可用；Failed（含凭证待续期）不可用。
fn usable_quotas(snapshot: &QuotaSnapshot) -> Option<&[Quota]> {
    match &snapshot.state {
        QuotaState::Failed(_) => None,
        QuotaState::Cached(_) if snapshot.stale => None,
        QuotaState::Ready(quotas) | QuotaState::Cached(quotas) => Some(quotas.as_slice()),
    }
}

fn known_ratios(quotas: &[Quota]) -> Vec<(QuotaWindow, f64)> {
    quotas
        .iter()
        .filter_map(|quota| quota.usage_ratio().map(|ratio| (quota.window, ratio)))
        .collect()
}

fn trigger_window(quotas: &[Quota], threshold: f64) -> Option<(QuotaWindow, f64)> {
    let mut best: Option<(QuotaWindow, f64)> = None;
    for quota in quotas {
        let Some(ratio) = quota.usage_ratio() else {
            continue;
        };
        if ratio < threshold {
            continue;
        }
        match best {
            None => best = Some((quota.window, ratio)),
            Some((_, best_ratio)) if ratio > best_ratio => best = Some((quota.window, ratio)),
            _ => {}
        }
    }
    best
}

fn is_eligible_candidate(quotas: &[Quota], threshold: f64) -> bool {
    let ratios = known_ratios(quotas);
    !ratios.is_empty() && ratios.iter().all(|(_, ratio)| *ratio < threshold)
}

fn candidate_max_ratio(quotas: &[Quota]) -> f64 {
    known_ratios(quotas)
        .into_iter()
        .map(|(_, ratio)| ratio)
        .fold(f64::NEG_INFINITY, f64::max)
}

/// `/usages` 快照 `membership` 原始字符串的档位优先级。
/// 序号越小越优先被选中；`None` 与未列出的字符串同为最低优先级（宁晚烧、不错烧高档）。
///
/// 全序按官网档位从低到高。`LEVEL_INTERMEDIATE` / `LEVEL_ADVANCED` 已与本机
/// `/usages` 及 `/me` 交叉；Andante / Moderato 无本机账号，按同一 `LEVEL_`
/// 前缀对应官网顺序，不要求真实响应再核实。
fn membership_tier_rank(membership: Option<&str>) -> u8 {
    match membership {
        Some("LEVEL_ANDANTE") => 0,
        Some("LEVEL_MODERATO") => 1,
        Some("LEVEL_INTERMEDIATE") => 2,
        Some("LEVEL_ADVANCED") => 3,
        Some("LEVEL_TEAM") => 4,
        _ => 5,
    }
}

fn compare_candidates(
    a: &Account,
    a_quotas: &[Quota],
    a_membership: Option<&str>,
    b: &Account,
    b_quotas: &[Quota],
    b_membership: Option<&str>,
    prefer_lower_tier: bool,
) -> Ordering {
    let tier = if prefer_lower_tier {
        membership_tier_rank(a_membership).cmp(&membership_tier_rank(b_membership))
    } else {
        Ordering::Equal
    };
    let ra = candidate_max_ratio(a_quotas);
    let rb = candidate_max_ratio(b_quotas);
    tier.then(
        ra.partial_cmp(&rb)
            .unwrap_or(Ordering::Equal)
            .then_with(|| match (a.last_used_at, b.last_used_at) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(ta), Some(tb)) => ta.cmp(&tb),
            })
            .then_with(|| a.id.0.cmp(&b.id.0)),
    )
}

fn in_cooldown(
    now: DateTime<Utc>,
    last_auto_swap_at: Option<DateTime<Utc>>,
    cooldown_ms: i64,
) -> bool {
    let Some(last) = last_auto_swap_at else {
        return false;
    };
    now < last + chrono::Duration::milliseconds(cooldown_ms)
}

fn in_settle_grace(
    now: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    settle_grace_ms: i64,
) -> bool {
    let Some(last_used) = last_used_at else {
        return false;
    };
    now < last_used + chrono::Duration::milliseconds(settle_grace_ms)
}

#[derive(Debug, Serialize, Deserialize)]
struct AutoSwapStateFile {
    last_auto_swap_at: DateTime<Utc>,
}

/// 读取上次自动切换成功时刻。文件缺失或损坏视为从未自动切换。
pub fn read_last_auto_swap_at(path: &Path) -> Option<DateTime<Utc>> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<AutoSwapStateFile>(&raw)
        .ok()
        .map(|state| state.last_auto_swap_at)
}

/// 写入上次自动切换成功时刻。失败只 warn，不回传错误。
pub fn write_last_auto_swap_at(path: &Path, at: DateTime<Utc>) {
    if let Err(err) = try_write_last_auto_swap_at(path, at) {
        tracing::warn!(
            err = %err,
            path = %path.display(),
            "auto_swap state write failed"
        );
    }
}

fn try_write_last_auto_swap_at(path: &Path, at: DateTime<Utc>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(&AutoSwapStateFile {
        last_auto_swap_at: at,
    })
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    use crate::error::QuotaFailure;
    use crate::model::{QuotaStatus, QuotaWindow};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 3, 12, 0, 0).unwrap()
    }

    fn on() -> AutoSwap {
        AutoSwap {
            enabled: true,
            threshold: 0.99,
            cooldown_ms: 5 * 60 * 1000,
            settle_grace_ms: 60 * 1000,
            prefer_lower_tier: false,
        }
    }

    fn off() -> AutoSwap {
        AutoSwap {
            enabled: false,
            ..on()
        }
    }

    fn prefer_lower() -> AutoSwap {
        AutoSwap {
            prefer_lower_tier: true,
            ..on()
        }
    }

    fn acc(id: &str, active: bool, last_used: Option<DateTime<Utc>>) -> Account {
        Account {
            provider: "kimi".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: now(),
            last_used_at: last_used,
            extra: Default::default(),
        }
    }

    /// 默认：当前账号已过安顿宽限期。
    fn settled(id: &str, active: bool) -> Account {
        acc(id, active, Some(now() - chrono::Duration::seconds(120)))
    }

    fn quota(id: &str, window: QuotaWindow, used: u64, limit: u64) -> Quota {
        Quota {
            provider: "kimi".into(),
            account_id: AccountId(id.into()),
            window,
            used,
            limit,
            reset_at: None,
            status: QuotaStatus::Ok,
            note: None,
            membership: None,
        }
    }

    fn quotas(id: &str, parts: &[(QuotaWindow, u64, u64)]) -> Vec<Quota> {
        parts
            .iter()
            .map(|(window, used, limit)| quota(id, *window, *used, *limit))
            .collect()
    }

    fn snap(id: &str, stale: bool, state: QuotaState) -> QuotaSnapshot {
        QuotaSnapshot {
            account_id: AccountId(id.into()),
            fetched_at: now(),
            stale,
            membership: None,
            state,
        }
    }

    fn ready(id: &str, parts: &[(QuotaWindow, u64, u64)]) -> QuotaSnapshot {
        snap(id, false, QuotaState::Ready(quotas(id, parts)))
    }

    fn membered(id: &str, parts: &[(QuotaWindow, u64, u64)], level: &str) -> QuotaSnapshot {
        let mut snap = ready(id, parts);
        snap.membership = Some(level.into());
        snap
    }

    fn cached_membered(id: &str, parts: &[(QuotaWindow, u64, u64)], level: &str) -> QuotaSnapshot {
        let mut snap = cached(id, false, parts);
        snap.membership = Some(level.into());
        snap
    }

    fn cached(id: &str, stale: bool, parts: &[(QuotaWindow, u64, u64)]) -> QuotaSnapshot {
        snap(id, stale, QuotaState::Cached(quotas(id, parts)))
    }

    fn failed_other(id: &str) -> QuotaSnapshot {
        snap(
            id,
            false,
            QuotaState::Failed(QuotaFailure::other("quota fetch failed")),
        )
    }

    fn failed_expired(id: &str) -> QuotaSnapshot {
        snap(
            id,
            false,
            QuotaState::Failed(QuotaFailure::credential_expired("credential expired")),
        )
    }

    #[derive(Debug)]
    enum Expect {
        Stay(AutoSwapSkipReason),
        Swap(&'static str, &'static str, QuotaWindow),
    }

    struct Case {
        name: &'static str,
        settings: AutoSwap,
        last_auto_swap_at: Option<DateTime<Utc>>,
        accounts: Vec<Account>,
        snapshots: Vec<QuotaSnapshot>,
        expect: Expect,
    }

    fn check(case: &Case) {
        let got = decide_auto_swap(
            &case.accounts,
            &case.snapshots,
            &case.settings,
            now(),
            case.last_auto_swap_at,
        );
        match (got, &case.expect) {
            (AutoSwapDecision::Stay { reason }, Expect::Stay(expected)) => {
                assert_eq!(reason, *expected, "{}", case.name);
            }
            (
                AutoSwapDecision::Swap(AutoSwapTarget {
                    from,
                    to,
                    trigger_window,
                    usage_ratio,
                }),
                Expect::Swap(from_id, to_id, window),
            ) => {
                assert_eq!(from.0, *from_id, "{}", case.name);
                assert_eq!(to.0, *to_id, "{}", case.name);
                assert_eq!(trigger_window, *window, "{}", case.name);
                assert!(
                    usage_ratio >= case.settings.threshold,
                    "{}: usage_ratio {usage_ratio} below threshold",
                    case.name
                );
            }
            (got, expected) => panic!("{}: got {got:?}, expect {expected:?}", case.name),
        }
    }

    #[test]
    fn decide_auto_swap_table() {
        let five = QuotaWindow::FiveHour;
        let seven = QuotaWindow::SevenDay;
        let w99 = &[(five, 99, 100)];
        let w98 = &[(five, 98, 100)];
        let w10 = &[(five, 10, 100)];
        let w0_unknown = &[(five, 50, 0)];
        let w100 = &[(five, 100, 100)];

        let cases = [
            Case {
                name: "开关关闭不产生切换",
                settings: off(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w100), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::Disabled),
            },
            Case {
                name: "恰好等于阈值触发",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w10)],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "低于阈值不触发",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w98), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "limit=0 不触发",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w0_unknown), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::CurrentUnknown),
            },
            Case {
                name: "当前 Failed 不触发",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![failed_other("alice"), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::CurrentUnavailable),
            },
            Case {
                name: "当前凭证待续期不触发",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![failed_expired("alice"), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::CurrentUnavailable),
            },
            Case {
                name: "当前 stale 回落不触发",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![cached("alice", true, w99), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::CurrentUnavailable),
            },
            Case {
                name: "当前 Cached 且未过期可用",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![cached("alice", false, w99), ready("bob", w10)],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "候选 Failed 跳过",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    failed_other("bob"),
                    ready("carol", w10),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "候选凭证待续期跳过",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    failed_expired("bob"),
                    ready("carol", w10),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "候选 stale 回落跳过",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    cached("bob", true, w10),
                    ready("carol", &[(five, 20, 100)]),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "候选 Unknown（limit=0）跳过",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w0_unknown)],
                expect: Expect::Stay(AutoSwapSkipReason::NoEligibleCandidate),
            },
            Case {
                name: "冷却期内不切",
                settings: on(),
                last_auto_swap_at: Some(now() - chrono::Duration::minutes(1)),
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::Cooldown),
            },
            Case {
                name: "安顿宽限期内不切走",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![
                    acc("alice", true, Some(now() - chrono::Duration::seconds(10))),
                    settled("bob", false),
                ],
                snapshots: vec![ready("alice", w99), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::SettleGrace),
            },
            Case {
                name: "无合格候选",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w99)],
                expect: Expect::Stay(AutoSwapSkipReason::NoEligibleCandidate),
            },
            Case {
                name: "候选任一已知窗口达到阈值不合格",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    ready("alice", w99),
                    ready("bob", &[(five, 10, 100), (seven, 99, 100)]),
                ],
                expect: Expect::Stay(AutoSwapSkipReason::NoEligibleCandidate),
            },
            Case {
                name: "墓碑不在输入列表中不会被选中",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true)],
                snapshots: vec![ready("alice", w99), ready("ghost", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::NoEligibleCandidate),
            },
            Case {
                name: "排序：已用比例升序",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    ready("bob", &[(five, 50, 100)]),
                    ready("carol", &[(five, 20, 100)]),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "排序：比例并列时 last_used_at 较久优先（None 最久）",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    acc("bob", false, None),
                    acc("carol", false, Some(now() - chrono::Duration::hours(10))),
                ],
                snapshots: vec![ready("alice", w99), ready("bob", w10), ready("carol", w10)],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "排序：再并列按 id 字典序",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    acc("carol", false, None),
                    acc("bob", false, None),
                ],
                snapshots: vec![ready("alice", w99), ready("carol", w10), ready("bob", w10)],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "触发窗口取已用比例最高者",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    ready("alice", &[(five, 99, 100), (seven, 100, 100)]),
                    ready("bob", w10),
                ],
                expect: Expect::Swap("alice", "bob", seven),
            },
            Case {
                name: "prefer on：低档高用量胜过高档低用量",
                settings: prefer_lower(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered("bob", &[(five, 50, 100)], "LEVEL_INTERMEDIATE"),
                    membered("carol", w10, "LEVEL_ADVANCED"),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer off：低档高用量 vs 高档低用量仍按比例选高档",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered("bob", &[(five, 50, 100)], "LEVEL_INTERMEDIATE"),
                    membered("carol", w10, "LEVEL_ADVANCED"),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "prefer on：同档回落已用比例升序",
                settings: prefer_lower(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered("bob", &[(five, 50, 100)], "LEVEL_INTERMEDIATE"),
                    membered("carol", &[(five, 20, 100)], "LEVEL_INTERMEDIATE"),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "prefer on：同档比例并列时 last_used_at 较久优先",
                settings: prefer_lower(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    acc("bob", false, None),
                    acc("carol", false, Some(now() - chrono::Duration::hours(10))),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered("bob", w10, "LEVEL_INTERMEDIATE"),
                    membered("carol", w10, "LEVEL_INTERMEDIATE"),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer on：同档再并列按 id 字典序",
                settings: prefer_lower(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    acc("carol", false, None),
                    acc("bob", false, None),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered("carol", w10, "LEVEL_INTERMEDIATE"),
                    membered("bob", w10, "LEVEL_INTERMEDIATE"),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer on：None 与未识别档位排最后",
                settings: prefer_lower(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                    settled("dave", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered("bob", &[(five, 50, 100)], "LEVEL_INTERMEDIATE"),
                    ready("carol", w10),
                    membered("dave", &[(five, 5, 100)], "LEVEL_FUTURE"),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer on：Allegro 胜 Team 即使 Team 用量更低",
                settings: prefer_lower(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered("bob", &[(five, 50, 100)], "LEVEL_ADVANCED"),
                    membered("carol", w10, "LEVEL_TEAM"),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer on：只剩更高档合格候选时仍然切换",
                settings: prefer_lower(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", w99, "LEVEL_INTERMEDIATE"),
                    membered("bob", w10, "LEVEL_TEAM"),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer on：Andante 胜 Moderato 即使 Moderato 用量更低",
                settings: prefer_lower(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered("bob", &[(five, 50, 100)], "LEVEL_ANDANTE"),
                    membered("carol", w10, "LEVEL_MODERATO"),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer on：未过期 Cached 快照上的档位同样参与排序",
                settings: prefer_lower(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    cached_membered("bob", &[(five, 50, 100)], "LEVEL_INTERMEDIATE"),
                    membered("carol", w10, "LEVEL_ADVANCED"),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer on：会员到期日不参与排序",
                settings: prefer_lower(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), acc("bob", false, None), {
                    let mut carol = acc("carol", false, None);
                    carol.extra.insert(
                        crate::membership_expiry::EXTRA_NEXT_BILLING_TIME.into(),
                        serde_json::Value::String("2026-01-01".into()),
                    );
                    carol
                }],
                snapshots: vec![
                    ready("alice", w99),
                    membered("bob", w10, "LEVEL_INTERMEDIATE"),
                    membered("carol", w10, "LEVEL_INTERMEDIATE"),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
        ];

        for case in &cases {
            check(case);
        }
    }

    #[test]
    fn last_auto_swap_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto_swap.json");
        let at = now();
        write_last_auto_swap_at(&path, at);
        assert_eq!(read_last_auto_swap_at(&path), Some(at));
    }

    #[test]
    fn last_auto_swap_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_last_auto_swap_at(&dir.path().join("nope.json")), None);
    }

    #[test]
    fn last_auto_swap_corrupt_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto_swap.json");
        std::fs::write(&path, "not-json").unwrap();
        assert_eq!(read_last_auto_swap_at(&path), None);
    }

    #[test]
    fn last_auto_swap_write_failure_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        write_last_auto_swap_at(dir.path(), now());
        assert_eq!(read_last_auto_swap_at(dir.path()), None);
    }
}
