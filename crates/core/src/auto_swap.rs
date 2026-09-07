//! 额度耗尽自动切换与回驻：纯决策 + 上次切换时刻 / 手动保持期的可丢弃状态。
//!
//! 决策函数无 IO、无网络；状态文件读写失败静默忽略。

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

/// 回驻目标（issue #95 / ADR-0008）：从哪个账号切回哪个账号。
///
/// 与 [`AutoSwapTarget`] 不同，没有触发窗口——回驻不因耗尽而起。
#[derive(Debug, Clone, PartialEq)]
pub struct AutoSwapReturn {
    pub from: AccountId,
    pub to: AccountId,
}

/// 回驻判定：当前账号未达耗尽阈值、但存在档位严格更低的合格停泊账号时，切回其中
/// 档位最低者。纯函数，不碰盘、不联网。
///
/// 闸门与 [`decide_auto_swap`] 共用语义：总开关或回驻子开关关、无当前账号、冷却期
/// （与耗尽切换共享一把锁）、安顿宽限、手动保持期都不回驻。当前账号快照不可用、
/// 额度未知、任一窗口已达阈值（那是耗尽路径的事）或档位未知时不回驻；候选须快照
/// 可用、所有窗口低于阈值、档位已知且严格低于当前档。无档位（`None` / 未识别）
/// 不参与比较：既不当目标，也不让无档位的当前账号被切走（ADR-0008 第 3 条）。
/// 无目标返回 `None`，调用方静默，不产生「没有可切换的账号」类提示。
pub fn decide_resident_return(
    accounts: &[Account],
    snapshots: &[QuotaSnapshot],
    settings: &AutoSwap,
    now: DateTime<Utc>,
    last_auto_swap_at: Option<DateTime<Utc>>,
    manual_hold_until: Option<DateTime<Utc>>,
) -> Option<AutoSwapReturn> {
    if !settings.enabled || !settings.resident_lower_tier {
        return None;
    }
    let Some(current) = accounts.iter().find(|account| account.active) else {
        return None;
    };
    if in_cooldown(now, last_auto_swap_at, settings.cooldown_ms) {
        return None;
    }
    if in_settle_grace(now, current.last_used_at, settings.settle_grace_ms) {
        return None;
    }
    if manual_hold_until.is_some_and(|until| now < until) {
        return None;
    }
    let Some(current_snap) = snapshot_of(snapshots, &current.id) else {
        return None;
    };
    let Some(current_quotas) = usable_quotas(current_snap) else {
        return None;
    };
    let current_rank = membership_tier_rank(current_snap.membership.as_deref());
    if current_rank >= UNKNOWN_TIER_RANK {
        return None;
    }
    let current_ratios = known_ratios(current_quotas);
    if current_ratios.is_empty()
        || current_ratios
            .iter()
            .any(|(_, ratio)| *ratio >= settings.threshold)
    {
        return None;
    }

    let mut eligible: Vec<(&Account, &[Quota], &str)> = accounts
        .iter()
        .filter(|account| account.id != current.id)
        .filter_map(|account| {
            let snap = snapshot_of(snapshots, &account.id)?;
            let quotas = usable_quotas(snap)?;
            if !is_eligible_candidate(quotas, settings.threshold) {
                return None;
            }
            let membership = snap.membership.as_deref()?;
            let rank = membership_tier_rank(Some(membership));
            if rank >= UNKNOWN_TIER_RANK || rank >= current_rank {
                return None;
            }
            Some((account, quotas, membership))
        })
        .collect();
    if eligible.is_empty() {
        return None;
    }
    eligible.sort_by(|(a, aq, am), (b, bq, bm)| {
        membership_tier_rank(Some(am))
            .cmp(&membership_tier_rank(Some(bm)))
            .then_with(|| compare_candidates(a, aq, Some(am), b, bq, Some(bm), false))
    });
    let (best, _, _) = eligible[0];
    Some(AutoSwapReturn {
        from: current.id.clone(),
        to: best.id.clone(),
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

/// 未知档位序号：`None` 与未识别枚举。耗尽路径按「排最后」用；回驻干脆不比较（ADR-0008）。
const UNKNOWN_TIER_RANK: u8 = 5;

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
        _ => UNKNOWN_TIER_RANK,
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
    /// 上次自动切换成功时刻。缺失视为从未自动切换：没有切换史就不伪造任何时刻
    /// （issue #96），序列化时 None 跳过该字段，读侧 `#[serde(default)]` 同语义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_auto_swap_at: Option<DateTime<Utc>>,
    /// 手动切换后的回驻保持期截止（issue #95）。缺失视为无保持期，存量文件兼容。
    #[serde(default)]
    manual_hold_until: Option<DateTime<Utc>>,
}

/// 整份状态文件；文件缺失或损坏返回 `None`（既有语义：视为从未自动切换）。
fn read_state(path: &Path) -> Option<AutoSwapStateFile> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<AutoSwapStateFile>(&raw).ok()
}

/// 读取上次自动切换成功时刻。文件缺失、损坏或无切换史字段视为从未自动切换。
pub fn read_last_auto_swap_at(path: &Path) -> Option<DateTime<Utc>> {
    read_state(path).and_then(|state| state.last_auto_swap_at)
}

/// 读取手动保持期截止。文件缺失、损坏或无该字段视为无保持期。
pub fn read_manual_hold_until(path: &Path) -> Option<DateTime<Utc>> {
    read_state(path).and_then(|state| state.manual_hold_until)
}

/// 写入上次自动切换成功时刻，读改写保留手动保持期。失败只 warn，不回传错误。
pub fn write_last_auto_swap_at(path: &Path, at: DateTime<Utc>) {
    let mut state = read_state(path).unwrap_or(AutoSwapStateFile {
        last_auto_swap_at: None,
        manual_hold_until: None,
    });
    state.last_auto_swap_at = Some(at);
    write_state_or_warn(path, &state);
}

/// 写入手动保持期截止，读改写不动上次自动切换时刻（没有切换史保持 None）。
/// 失败只 warn，不回传错误。
pub fn set_manual_hold_until(path: &Path, until: DateTime<Utc>) {
    // 文件缺失或损坏时重建：没有切换史就不伪造任何时刻（issue #96）。保持期
    // 只约束回驻，不得借 last_auto_swap_at 把耗尽切换也锁进冷却（ADR-0008 第 4 条）。
    let mut state = read_state(path).unwrap_or(AutoSwapStateFile {
        last_auto_swap_at: None,
        manual_hold_until: None,
    });
    state.manual_hold_until = Some(until);
    write_state_or_warn(path, &state);
}

fn write_state_or_warn(path: &Path, state: &AutoSwapStateFile) {
    if let Err(err) = try_write_state(path, state) {
        tracing::warn!(
            err = %err,
            path = %path.display(),
            "auto_swap state write failed"
        );
    }
}

fn try_write_state(path: &Path, state: &AutoSwapStateFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(state)
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
            resident_lower_tier: false,
            manual_hold_ms: crate::defaults::AUTO_SWAP_MANUAL_HOLD_MS,
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

    fn resident() -> AutoSwap {
        AutoSwap {
            resident_lower_tier: true,
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

    /// issue #95：两个写入点读改写互不覆盖，存量无 hold 字段文件兼容。
    #[test]
    fn manual_hold_roundtrip_preserves_last_swap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto_swap.json");
        let at = now();
        write_last_auto_swap_at(&path, at);
        let hold = now() + chrono::Duration::hours(8);
        set_manual_hold_until(&path, hold);
        assert_eq!(read_last_auto_swap_at(&path), Some(at));
        assert_eq!(read_manual_hold_until(&path), Some(hold));

        // 反向：写冷却不丢保持期。
        let at2 = now() + chrono::Duration::minutes(10);
        write_last_auto_swap_at(&path, at2);
        assert_eq!(read_last_auto_swap_at(&path), Some(at2));
        assert_eq!(read_manual_hold_until(&path), Some(hold));
    }

    /// issue #95：存量只有 last_auto_swap_at 的文件（升级前格式）仍可读，无保持期。
    #[test]
    fn manual_hold_missing_file_or_field_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_manual_hold_until(&dir.path().join("nope.json")), None);
        let path = dir.path().join("auto_swap.json");
        std::fs::write(&path, r#"{"last_auto_swap_at":"2026-09-03T12:00:00Z"}"#).unwrap();
        assert_eq!(read_last_auto_swap_at(&path), Some(now()));
        assert_eq!(read_manual_hold_until(&path), None);
    }

    /// issue #96：全新文件（或状态文件被删）上只写保持期，不伪造切换史。
    #[test]
    fn manual_hold_on_fresh_file_keeps_last_swap_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto_swap.json");
        let hold = now() + chrono::Duration::hours(8);
        set_manual_hold_until(&path, hold);
        assert_eq!(read_last_auto_swap_at(&path), None);
        assert_eq!(read_manual_hold_until(&path), Some(hold));
    }

    /// issue #96 / ADR-0008 第 4 条：保持期内耗尽切换不被冷却锁死。
    /// 按调用方接线模拟：手动切换后状态文件只有保持期，read_last_auto_swap_at
    /// 读出 None，当前账号 99% 时应 Swap 而非 Stay{Cooldown}。
    #[test]
    fn exhaustion_swaps_during_manual_hold_without_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto_swap.json");
        let hold = now() + chrono::Duration::hours(8);
        set_manual_hold_until(&path, hold);
        assert_eq!(read_manual_hold_until(&path), Some(hold));

        let five = QuotaWindow::FiveHour;
        let accounts = vec![settled("alice", true), settled("bob", false)];
        let snapshots = vec![
            ready("alice", &[(five, 99, 100)]),
            ready("bob", &[(five, 10, 100)]),
        ];
        let got = decide_auto_swap(
            &accounts,
            &snapshots,
            &on(),
            now(),
            read_last_auto_swap_at(&path),
        );
        match got {
            AutoSwapDecision::Swap(target) => {
                assert_eq!(target.from.0, "alice");
                assert_eq!(target.to.0, "bob");
            }
            other => panic!("保持期内耗尽应 Swap，got {other:?}"),
        }
    }

    /// issue #95 / ADR-0008：回驻判定全矩阵。默认场景：当前高档未耗尽 + 低档停泊可用。
    #[test]
    fn decide_resident_return_table() {
        let five = QuotaWindow::FiveHour;
        let seven = QuotaWindow::SevenDay;
        let w_low = &[(five, 10, 100), (seven, 20, 100)];
        let adv = "LEVEL_ADVANCED";
        let mid = "LEVEL_INTERMEDIATE";

        struct ReturnCase {
            name: &'static str,
            settings: AutoSwap,
            last_auto_swap_at: Option<DateTime<Utc>>,
            manual_hold_until: Option<DateTime<Utc>>,
            accounts: Vec<Account>,
            snapshots: Vec<QuotaSnapshot>,
            expect_to: Option<&'static str>,
        }

        let cases = [
            ReturnCase {
                name: "高档当前 + 低档停泊可用 → 回驻",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: Some("bob"),
            },
            ReturnCase {
                name: "总开关关不回驻",
                settings: AutoSwap {
                    enabled: false,
                    ..resident()
                },
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
            },
            ReturnCase {
                name: "回驻子开关关不回驻",
                settings: on(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
            },
            ReturnCase {
                name: "冷却期内不回驻（与耗尽共享）",
                settings: resident(),
                last_auto_swap_at: Some(now() - chrono::Duration::minutes(1)),
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
            },
            ReturnCase {
                name: "安顿宽限期内不回驻",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![
                    acc("alice", true, Some(now() - chrono::Duration::seconds(10))),
                    settled("bob", false),
                ],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
            },
            ReturnCase {
                name: "手动保持期内不回驻",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: Some(now() + chrono::Duration::hours(1)),
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
            },
            ReturnCase {
                name: "保持期刚过即可回驻",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: Some(now() - chrono::Duration::seconds(1)),
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: Some("bob"),
            },
            ReturnCase {
                name: "当前已达阈值不回驻（耗尽路径接管）",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", &[(five, 99, 100)], adv),
                    membered("bob", w_low, mid),
                ],
                expect_to: None,
            },
            ReturnCase {
                name: "当前额度未知（limit=0）不回驻",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", &[(five, 50, 0)], adv),
                    membered("bob", w_low, mid),
                ],
                expect_to: None,
            },
            ReturnCase {
                name: "当前快照 Failed 不回驻",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![failed_other("alice"), membered("bob", w_low, mid)],
                expect_to: None,
            },
            ReturnCase {
                name: "当前档位未知不回驻",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w_low), membered("bob", w_low, mid)],
                expect_to: None,
            },
            ReturnCase {
                name: "低档停泊额度任一窗口达阈值不回驻",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", w_low, adv),
                    membered("bob", &[(five, 10, 100), (seven, 99, 100)], mid),
                ],
                expect_to: None,
            },
            ReturnCase {
                name: "同档停泊不回驻（须严格更低）",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", w_low, mid),
                    membered("bob", &[(five, 0, 100)], mid),
                ],
                expect_to: None,
            },
            ReturnCase {
                name: "无档位停泊不作为目标",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), ready("bob", w_low)],
                expect_to: None,
            },
            ReturnCase {
                name: "未识别档位停泊不作为目标",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", w_low, adv),
                    membered("bob", w_low, "LEVEL_FUTURE"),
                ],
                expect_to: None,
            },
            ReturnCase {
                name: "候选快照 Failed 跳过",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), failed_other("bob")],
                expect_to: None,
            },
            ReturnCase {
                name: "多档更低取最低档（Andante 胜 Allegretto）",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    membered("alice", w_low, adv),
                    membered("bob", w_low, mid),
                    membered("carol", &[(five, 80, 100)], "LEVEL_ANDANTE"),
                ],
                expect_to: Some("carol"),
            },
            ReturnCase {
                name: "同档多候选回落已用比例升序",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    membered("alice", w_low, adv),
                    membered("bob", &[(five, 50, 100)], mid),
                    membered("carol", &[(five, 20, 100)], mid),
                ],
                expect_to: Some("carol"),
            },
            ReturnCase {
                name: "低档已 90%（未达阈值）仍回驻：资格线沿用合格线",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", w_low, adv),
                    membered("bob", &[(five, 0, 100), (seven, 90, 100)], mid),
                ],
                expect_to: Some("bob"),
            },
            ReturnCase {
                name: "当前已是最低档不回驻",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", w_low, "LEVEL_ANDANTE"),
                    membered("bob", w_low, mid),
                ],
                expect_to: None,
            },
        ];

        for case in &cases {
            let got = decide_resident_return(
                &case.accounts,
                &case.snapshots,
                &case.settings,
                now(),
                case.last_auto_swap_at,
                case.manual_hold_until,
            );
            match (got, case.expect_to) {
                (Some(target), Some(to)) => {
                    assert_eq!(target.from.0, "alice", "{}", case.name);
                    assert_eq!(target.to.0, to, "{}", case.name);
                }
                (None, None) => {}
                (got, expected) => panic!("{}: got {got:?}, expect {expected:?}", case.name),
            }
        }
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
