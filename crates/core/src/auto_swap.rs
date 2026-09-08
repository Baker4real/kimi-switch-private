//! 额度耗尽自动切换、pace 预切换与回驻：纯决策 + 上次切换时刻 / 手动保持期 / 回驻确认计数的可丢弃状态。
//!
//! 决策函数无 IO、无网络；状态文件读写失败静默忽略。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auto_swap_opt_out::account_auto_swap_opt_out;
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

/// `decide_auto_swap` 命中切换时的路径。回驻走 [`AutoSwapReturn`]，不在此枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoSwapKind {
    /// 额度耗尽路径（已达 `threshold`）。
    #[default]
    Exhausted,
    /// pace 预切换（issue #114）：未达耗尽阈值，但预测撑不到 reset。
    Pace,
}

/// 切到哪个账号，以及触发判定用的窗口。
#[derive(Debug, Clone, PartialEq)]
pub struct AutoSwapTarget {
    pub from: AccountId,
    pub to: AccountId,
    /// 当前账号达到阈值的窗口中，已用比例最高者；pace 路径则为 pace>1 中最高者。
    pub trigger_window: QuotaWindow,
    /// 该窗口已用比例 0.0~1.0。
    pub usage_ratio: f64,
    /// 耗尽或 pace。审计 / JSON 用此区分路径。
    pub kind: AutoSwapKind,
}

/// 自动切换判定结果。
#[derive(Debug, Clone, PartialEq)]
pub enum AutoSwapDecision {
    Stay { reason: AutoSwapSkipReason },
    Swap(AutoSwapTarget),
}

/// 根据已收集的额度快照判定是否自动切换。纯函数，不碰盘、不联网。
///
/// 停泊候选须先过阈值资格线，再满足最大已用比例 ≤ 当前最大已用 − [`AutoSwap::candidate_margin`]。
/// 已达 `threshold` 走耗尽路径（[`AutoSwapKind::Exhausted`]）。未达阈值时，若开启
/// [`AutoSwap::pace_pre_swap`] 且当前账号预测撑不到 reset、候选更健康，则返回
/// `Swap` 且 [`AutoSwapTarget::kind`] 为 [`AutoSwapKind::Pace`]。pace 不抢占耗尽。
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
        if let Some(target) =
            decide_pace_swap(current, current_quotas, accounts, snapshots, settings, now)
        {
            return AutoSwapDecision::Swap(target);
        }
        return stay(AutoSwapSkipReason::BelowThreshold);
    };

    let Some(best) = pick_best_eligible(accounts, snapshots, current, current_quotas, settings)
    else {
        return stay(AutoSwapSkipReason::NoEligibleCandidate);
    };
    AutoSwapDecision::Swap(AutoSwapTarget {
        from: current.id.clone(),
        to: best.id.clone(),
        trigger_window,
        usage_ratio,
        kind: AutoSwapKind::Exhausted,
    })
}

/// 未达耗尽阈值时的 pace 预切换。子开关关、低于预警水位、没有 pace>1 的已知时长窗口、
/// 或没有更健康候选时返回 `None`（调用方仍报 [`AutoSwapSkipReason::BelowThreshold`]）。
fn decide_pace_swap(
    current: &Account,
    current_quotas: &[Quota],
    accounts: &[Account],
    snapshots: &[QuotaSnapshot],
    settings: &AutoSwap,
    now: DateTime<Utc>,
) -> Option<AutoSwapTarget> {
    if !settings.pace_pre_swap {
        return None;
    }
    if candidate_max_ratio(current_quotas) < settings.pace_warn_threshold {
        return None;
    }
    let (trigger_window, usage_ratio) = overpacing_trigger(current_quotas, now)?;
    let best = pick_best_eligible(accounts, snapshots, current, current_quotas, settings)?;
    Some(AutoSwapTarget {
        from: current.id.clone(),
        to: best.id.clone(),
        trigger_window,
        usage_ratio,
        kind: AutoSwapKind::Pace,
    })
}

fn pick_best_eligible<'a>(
    accounts: &'a [Account],
    snapshots: &'a [QuotaSnapshot],
    current: &Account,
    current_quotas: &[Quota],
    settings: &AutoSwap,
) -> Option<&'a Account> {
    let current_max = candidate_max_ratio(current_quotas);
    let mut eligible: Vec<(&Account, &[Quota], Option<&str>)> = accounts
        .iter()
        .filter(|account| account.id != current.id)
        .filter(|account| !account_auto_swap_opt_out(account))
        .filter_map(|account| {
            let snap = snapshot_of(snapshots, &account.id)?;
            let quotas = usable_quotas(snap)?;
            if !is_eligible_candidate(quotas, settings.threshold) {
                return None;
            }
            if !beats_current_by_margin(quotas, current_max, settings.candidate_margin) {
                return None;
            }
            Some((account, quotas, snap.membership.as_deref()))
        })
        .collect();

    if eligible.is_empty() {
        return None;
    }

    eligible.sort_by(|(a, aq, am), (b, bq, bm)| {
        compare_candidates(
            a,
            aq,
            *am,
            b,
            bq,
            *bm,
            settings.prefer_lower_tier,
            settings.prefer_soonest_reset,
        )
    });
    Some(eligible[0].0)
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
///
/// 确认期（issue #111 / ADR-0012）：目标须连续 [`AutoSwap::resident_confirm_n`] 个
/// 快照周期保持合格才返回 `Some`。`confirm_counts` 是本函数读写的 per-account 连续
/// 合格计数：闸门挡住时不改；本周期无合格目标则清零；目标更换则只保留新目标。
/// 0 与 1 同义。跨周期持久化见 [`decide_resident_return_from_state`]。
pub fn decide_resident_return(
    accounts: &[Account],
    snapshots: &[QuotaSnapshot],
    settings: &AutoSwap,
    now: DateTime<Utc>,
    last_auto_swap_at: Option<DateTime<Utc>>,
    manual_hold_until: Option<DateTime<Utc>>,
    confirm_counts: &mut HashMap<AccountId, u32>,
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
        .filter(|account| !account_auto_swap_opt_out(account))
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
        confirm_counts.clear();
        return None;
    }
    eligible.sort_by(|(a, aq, am), (b, bq, bm)| {
        membership_tier_rank(Some(am))
            .cmp(&membership_tier_rank(Some(bm)))
            .then_with(|| compare_candidates(a, aq, Some(am), b, bq, Some(bm), false, false))
    });
    let (best, _, _) = eligible[0];
    let to = best.id.clone();
    if note_resident_confirm(confirm_counts, &to, settings.resident_confirm_n) {
        Some(AutoSwapReturn {
            from: current.id.clone(),
            to,
        })
    } else {
        None
    }
}

/// 本周期目标连续合格计数 +1，丢掉其它账号的计数。返回是否已满 N 个周期。
/// `n == 0` 视为 1。
fn note_resident_confirm(
    confirm_counts: &mut HashMap<AccountId, u32>,
    to: &AccountId,
    n: u32,
) -> bool {
    let next = confirm_counts
        .get(to)
        .copied()
        .unwrap_or(0)
        .saturating_add(1);
    confirm_counts.clear();
    confirm_counts.insert(to.clone(), next);
    next >= n.max(1)
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

/// 耗尽 / pace 路径额外资格：停泊账号最大已用比例须比当前账号低出边际（ADR-0010）。
/// 回驻路径不调用。`margin == 0` 时，阈值合格的停泊账号已满足此式。
fn beats_current_by_margin(quotas: &[Quota], current_max: f64, margin: f64) -> bool {
    candidate_max_ratio(quotas) <= current_max - margin
}

fn candidate_max_ratio(quotas: &[Quota]) -> f64 {
    known_ratios(quotas)
        .into_iter()
        .map(|(_, ratio)| ratio)
        .fold(f64::NEG_INFINITY, f64::max)
}

/// 仅 5h / 7d 有已知窗口时长；`Month` / `Custom` 等不参与 pace。
fn known_window_duration(window: QuotaWindow) -> Option<chrono::Duration> {
    match window {
        QuotaWindow::FiveHour => Some(chrono::Duration::hours(5)),
        QuotaWindow::SevenDay => Some(chrono::Duration::days(7)),
        QuotaWindow::Month
        | QuotaWindow::FirstPartyModels
        | QuotaWindow::Api
        | QuotaWindow::Custom => None,
    }
}

/// pace = 已用比例 / 窗口已流逝比例。起点 = `reset_at − 已知时长`，不用 now−时长。
/// 缺 `reset_at`、时长未知、尚未进入窗口、或已过 reset → 该窗口不参与。
fn window_pace(quota: &Quota, now: DateTime<Utc>) -> Option<f64> {
    let duration = known_window_duration(quota.window)?;
    let reset_at = quota.reset_at?;
    let usage = quota.usage_ratio()?;
    if now >= reset_at {
        return None;
    }
    let start = reset_at - duration;
    if now <= start {
        return None;
    }
    let elapsed_ms = (now - start).num_milliseconds();
    let duration_ms = duration.num_milliseconds();
    if elapsed_ms <= 0 || duration_ms <= 0 {
        return None;
    }
    let elapsed_ratio = elapsed_ms as f64 / duration_ms as f64;
    Some(usage / elapsed_ratio)
}

/// 参与 pace 且 pace>1 的窗口中，取 pace 最高者；并列保留先见到的。
fn overpacing_trigger(quotas: &[Quota], now: DateTime<Utc>) -> Option<(QuotaWindow, f64)> {
    let mut best: Option<(QuotaWindow, f64, f64)> = None;
    for quota in quotas {
        let Some(pace) = window_pace(quota, now) else {
            continue;
        };
        if pace <= 1.0 {
            continue;
        }
        let Some(usage) = quota.usage_ratio() else {
            continue;
        };
        match best {
            None => best = Some((quota.window, usage, pace)),
            Some((_, _, best_pace)) if pace > best_pace => {
                best = Some((quota.window, usage, pace));
            }
            _ => {}
        }
    }
    best.map(|(window, usage, _)| (window, usage))
}

/// 账号级快重置键：已知窗口（limit > 0）上 `reset_at` 的最早值。
/// 窗口未知或该窗口缺 `reset_at` 则忽略；没有任何已知窗口给出时刻则未知。
fn candidate_soonest_reset(quotas: &[Quota]) -> Option<DateTime<Utc>> {
    quotas
        .iter()
        .filter(|quota| quota.limit > 0)
        .filter_map(|quota| quota.reset_at)
        .min()
}

/// 已知时刻升序（越早越优先）；未知排最后，不占「先烧快重置」优先位。
fn compare_soonest_reset(a_quotas: &[Quota], b_quotas: &[Quota]) -> Ordering {
    match (
        candidate_soonest_reset(a_quotas),
        candidate_soonest_reset(b_quotas),
    ) {
        (Some(ta), Some(tb)) => ta.cmp(&tb),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
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
    prefer_soonest_reset: bool,
) -> Ordering {
    let tier = if prefer_lower_tier {
        membership_tier_rank(a_membership).cmp(&membership_tier_rank(b_membership))
    } else {
        Ordering::Equal
    };
    let reset = if prefer_soonest_reset {
        compare_soonest_reset(a_quotas, b_quotas)
    } else {
        Ordering::Equal
    };
    let ra = candidate_max_ratio(a_quotas);
    let rb = candidate_max_ratio(b_quotas);
    tier.then(reset).then(
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

#[derive(Debug, Default, Serialize, Deserialize)]
struct AutoSwapStateFile {
    /// 上次自动切换成功时刻。缺失视为从未自动切换：没有切换史就不伪造任何时刻
    /// （issue #96），序列化时 None 跳过该字段，读侧 `#[serde(default)]` 同语义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_auto_swap_at: Option<DateTime<Utc>>,
    /// 手动切换后的回驻保持期截止（issue #95）。缺失视为无保持期，存量文件兼容。
    #[serde(default)]
    manual_hold_until: Option<DateTime<Utc>>,
    /// 回驻确认期 per-account 连续合格计数（issue #111）。键为账号 id。
    /// 缺字段 / 空表视为全 0；空表序列化时跳过。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    resident_confirm: HashMap<String, u32>,
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

/// 读取回驻确认计数。文件缺失、损坏或无该字段视为全 0。
pub fn read_resident_confirm_counts(path: &Path) -> HashMap<AccountId, u32> {
    read_state(path)
        .map(|state| {
            state
                .resident_confirm
                .into_iter()
                .map(|(id, n)| (AccountId(id), n))
                .collect()
        })
        .unwrap_or_default()
}

/// 写入回驻确认计数，读改写保留冷却时刻与手动保持期。失败只 warn，不回传错误。
/// 文件本不存在且计数全 0 时不建文件（保持「从未写过」）。
pub fn write_resident_confirm_counts(path: &Path, counts: &HashMap<AccountId, u32>) {
    let mapped: HashMap<String, u32> = counts.iter().map(|(id, n)| (id.0.clone(), *n)).collect();
    match read_state(path) {
        None if mapped.is_empty() => {}
        None => {
            write_state_or_warn(
                path,
                &AutoSwapStateFile {
                    resident_confirm: mapped,
                    ..AutoSwapStateFile::default()
                },
            );
        }
        Some(mut state) => {
            if state.resident_confirm == mapped {
                return;
            }
            state.resident_confirm = mapped;
            write_state_or_warn(path, &state);
        }
    }
}

/// 读状态文件中的回驻确认计数、判定、写回。调用方不必自己搬 HashMap。
pub fn decide_resident_return_from_state(
    path: &Path,
    accounts: &[Account],
    snapshots: &[QuotaSnapshot],
    settings: &AutoSwap,
    now: DateTime<Utc>,
    last_auto_swap_at: Option<DateTime<Utc>>,
    manual_hold_until: Option<DateTime<Utc>>,
) -> Option<AutoSwapReturn> {
    let mut confirm_counts = read_resident_confirm_counts(path);
    let target = decide_resident_return(
        accounts,
        snapshots,
        settings,
        now,
        last_auto_swap_at,
        manual_hold_until,
        &mut confirm_counts,
    );
    write_resident_confirm_counts(path, &confirm_counts);
    target
}

/// 写入上次自动切换成功时刻，读改写保留手动保持期，清掉回驻确认计数
/// （账号角色已变，确认期从头计）。失败只 warn，不回传错误。
pub fn write_last_auto_swap_at(path: &Path, at: DateTime<Utc>) {
    let mut state = read_state(path).unwrap_or_default();
    state.last_auto_swap_at = Some(at);
    state.resident_confirm.clear();
    write_state_or_warn(path, &state);
}

/// 写入手动保持期截止，读改写不动上次自动切换时刻（没有切换史保持 None），
/// 并清掉回驻确认计数（手动决定作废未完成的确认）。失败只 warn，不回传错误。
pub fn set_manual_hold_until(path: &Path, until: DateTime<Utc>) {
    // 文件缺失或损坏时重建：没有切换史就不伪造任何时刻（issue #96）。保持期
    // 只约束回驻，不得借 last_auto_swap_at 把耗尽切换也锁进冷却（ADR-0008 第 4 条）。
    let mut state = read_state(path).unwrap_or_default();
    state.manual_hold_until = Some(until);
    state.resident_confirm.clear();
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
            ..AutoSwap::default()
        }
    }

    fn with_margin(margin: f64) -> AutoSwap {
        AutoSwap {
            candidate_margin: margin,
            ..on()
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

    fn prefer_soonest() -> AutoSwap {
        AutoSwap {
            prefer_soonest_reset: true,
            ..on()
        }
    }

    fn prefer_lower_and_soonest() -> AutoSwap {
        AutoSwap {
            prefer_lower_tier: true,
            prefer_soonest_reset: true,
            ..on()
        }
    }

    fn resident() -> AutoSwap {
        AutoSwap {
            resident_lower_tier: true,
            // 资格闸门表用 N=1，确认期另测。
            resident_confirm_n: 1,
            ..on()
        }
    }

    fn pace_on() -> AutoSwap {
        AutoSwap {
            pace_pre_swap: true,
            ..on()
        }
    }

    fn pace_with_margin(margin: f64) -> AutoSwap {
        AutoSwap {
            candidate_margin: margin,
            ..pace_on()
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

    fn opted_out_acc(id: &str, active: bool) -> Account {
        let mut account = settled(id, active);
        crate::write_auto_swap_opt_out_extra(&mut account.extra, true);
        account
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

    fn ready_reset(
        id: &str,
        parts: &[(QuotaWindow, u64, u64)],
        reset_at: DateTime<Utc>,
    ) -> QuotaSnapshot {
        let mut snap = ready(id, parts);
        if let QuotaState::Ready(quotas) = &mut snap.state {
            for quota in quotas {
                quota.reset_at = Some(reset_at);
            }
        }
        snap
    }

    fn membered_reset(
        id: &str,
        parts: &[(QuotaWindow, u64, u64)],
        level: &str,
        reset_at: DateTime<Utc>,
    ) -> QuotaSnapshot {
        let mut snap = ready_reset(id, parts, reset_at);
        snap.membership = Some(level.into());
        snap
    }

    fn ready_resets(
        id: &str,
        parts: &[(QuotaWindow, u64, u64, Option<DateTime<Utc>>)],
    ) -> QuotaSnapshot {
        let quotas = parts
            .iter()
            .map(|(window, used, limit, reset_at)| {
                let mut q = quota(id, *window, *used, *limit);
                q.reset_at = *reset_at;
                q
            })
            .collect();
        snap(id, false, QuotaState::Ready(quotas))
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
        Pace(&'static str, &'static str, QuotaWindow),
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
            (AutoSwapDecision::Swap(target), Expect::Swap(from_id, to_id, window)) => {
                assert_eq!(target.from.0, *from_id, "{}", case.name);
                assert_eq!(target.to.0, *to_id, "{}", case.name);
                assert_eq!(target.trigger_window, *window, "{}", case.name);
                assert_eq!(target.kind, AutoSwapKind::Exhausted, "{}", case.name);
                assert!(
                    target.usage_ratio >= case.settings.threshold,
                    "{}: usage_ratio {} below threshold",
                    case.name,
                    target.usage_ratio
                );
            }
            (AutoSwapDecision::Swap(target), Expect::Pace(from_id, to_id, window)) => {
                assert_eq!(target.from.0, *from_id, "{}", case.name);
                assert_eq!(target.to.0, *to_id, "{}", case.name);
                assert_eq!(target.trigger_window, *window, "{}", case.name);
                assert_eq!(target.kind, AutoSwapKind::Pace, "{}", case.name);
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
        let soon = now() + chrono::Duration::hours(1);
        let later = now() + chrono::Duration::hours(4);

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
            Case {
                name: "默认边际 0：候选仅略低于当前仍切",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w98)],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "边际：差值小于边际不切",
                settings: with_margin(0.05),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w98)],
                expect: Expect::Stay(AutoSwapSkipReason::NoEligibleCandidate),
            },
            Case {
                name: "边际：差值大于边际则切",
                settings: with_margin(0.05),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w10)],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "边际：差值等于边际则切",
                settings: with_margin(0.5),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w100), ready("bob", &[(five, 50, 100)])],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "边际不降低触发线：当前低于阈值不切",
                settings: with_margin(0.05),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w98), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "边际不放宽资格线：候选达阈值仍不合格",
                settings: with_margin(0.0),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w100), ready("bob", w99)],
                expect: Expect::Stay(AutoSwapSkipReason::NoEligibleCandidate),
            },
            Case {
                name: "边际：略低者不合格，拉开边际的候选可切",
                settings: with_margin(0.05),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![ready("alice", w99), ready("bob", w98), ready("carol", w10)],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "边际够但冷却期内仍 Stay",
                settings: with_margin(0.05),
                last_auto_swap_at: Some(now() - chrono::Duration::minutes(1)),
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::Cooldown),
            },
            Case {
                name: "过了冷却但边际不够仍不切",
                settings: with_margin(0.05),
                last_auto_swap_at: Some(now() - chrono::Duration::minutes(6)),
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w98)],
                expect: Expect::Stay(AutoSwapSkipReason::NoEligibleCandidate),
            },
            Case {
                name: "安顿宽限期内边际够仍不切走",
                settings: with_margin(0.05),
                last_auto_swap_at: None,
                accounts: vec![
                    acc("alice", true, Some(now() - chrono::Duration::seconds(10))),
                    settled("bob", false),
                ],
                snapshots: vec![ready("alice", w99), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::SettleGrace),
            },
            Case {
                name: "过了安顿宽限但边际不够仍不切",
                settings: with_margin(0.05),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w98)],
                expect: Expect::Stay(AutoSwapSkipReason::NoEligibleCandidate),
            },
            Case {
                name: "prefer soonest off：更早 reset 的高用量仍按比例选低用量",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    ready_reset("bob", &[(five, 50, 100)], soon),
                    ready_reset("carol", w10, later),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "prefer soonest on：同比例先选更早 reset",
                settings: prefer_soonest(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    ready_reset("bob", w10, later),
                    ready_reset("carol", w10, soon),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "prefer soonest on：更早 reset 胜更低用量",
                settings: prefer_soonest(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    ready_reset("bob", &[(five, 50, 100)], soon),
                    ready_reset("carol", w10, later),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer soonest on：缺 reset_at 不占优先位",
                settings: prefer_soonest(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    acc("bob", false, None),
                    acc("carol", false, None),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    ready("bob", w10),
                    ready_reset("carol", w10, later),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "prefer soonest on：双方都缺 reset_at 回落已用比例",
                settings: prefer_soonest(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    ready("bob", &[(five, 50, 100)]),
                    ready("carol", w10),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "prefer soonest on：双方都缺 reset_at 比例并列回落 last_used_at",
                settings: prefer_soonest(),
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
                name: "prefer soonest on：reset_at 相同回落已用比例",
                settings: prefer_soonest(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    ready_reset("bob", &[(five, 50, 100)], soon),
                    ready_reset("carol", w10, soon),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "prefer soonest on：窗口未知的 reset_at 不占优先位",
                settings: prefer_soonest(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    acc("bob", false, None),
                    acc("carol", false, None),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    ready_resets("bob", &[(five, 10, 100, None), (seven, 50, 0, Some(soon))]),
                    ready_reset("carol", w10, later),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "prefer soonest on：多窗口取最早 reset_at",
                settings: prefer_soonest(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("aaron", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    ready_resets(
                        "bob",
                        &[(five, 10, 100, Some(later)), (seven, 20, 100, Some(soon))],
                    ),
                    ready_resets(
                        "aaron",
                        &[
                            (five, 10, 100, Some(now() + chrono::Duration::hours(2))),
                            (seven, 20, 100, Some(now() + chrono::Duration::days(6))),
                        ],
                    ),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer soonest on：唯一候选缺 reset_at 仍切",
                settings: prefer_soonest(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w10)],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer soonest 不放宽资格线：候选达阈值仍不合格",
                settings: prefer_soonest(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready_reset("bob", w99, soon)],
                expect: Expect::Stay(AutoSwapSkipReason::NoEligibleCandidate),
            },
            Case {
                name: "prefer_lower + soonest：低档晚 reset 胜高档早 reset",
                settings: prefer_lower_and_soonest(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered_reset("bob", &[(five, 50, 100)], "LEVEL_INTERMEDIATE", later),
                    membered_reset("carol", w10, "LEVEL_ADVANCED", soon),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer_lower + soonest：同档早 reset 胜低用量",
                settings: prefer_lower_and_soonest(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered_reset("bob", &[(five, 50, 100)], "LEVEL_INTERMEDIATE", soon),
                    membered_reset("carol", w10, "LEVEL_INTERMEDIATE", later),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "prefer_lower + soonest：同档缺 reset_at 不占优先位",
                settings: prefer_lower_and_soonest(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    acc("bob", false, None),
                    acc("carol", false, None),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    membered("bob", w10, "LEVEL_INTERMEDIATE"),
                    membered_reset("carol", w10, "LEVEL_INTERMEDIATE", later),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "回驻确认期不影响耗尽路径",
                settings: AutoSwap {
                    resident_lower_tier: true,
                    resident_confirm_n: 99,
                    ..on()
                },
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", w99, "LEVEL_ADVANCED"),
                    membered("bob", w10, "LEVEL_INTERMEDIATE"),
                ],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "摘出停泊账号不作为耗尽候选，选下一合格账号",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    opted_out_acc("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready("alice", w99),
                    ready("bob", w10),
                    ready("carol", &[(five, 20, 100)]),
                ],
                expect: Expect::Swap("alice", "carol", five),
            },
            Case {
                name: "仅有的停泊账号已摘出 → 无合格候选",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), opted_out_acc("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::NoEligibleCandidate),
            },
            Case {
                name: "被摘出的当前账号耗尽时仍被自动切走",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![opted_out_acc("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w99), ready("bob", w10)],
                expect: Expect::Swap("alice", "bob", five),
            },
        ];

        for case in &cases {
            check(case);
        }
    }

    /// issue #114：pace 预切换。5h 窗时长 5 小时；`now` 为 12:00。
    /// half：reset=14:30，已流逝 50%；almost：reset=12:30，已流逝 90%。
    #[test]
    fn decide_auto_swap_pace_table() {
        let five = QuotaWindow::FiveHour;
        let seven = QuotaWindow::SevenDay;
        let month = QuotaWindow::Month;
        let w10 = &[(five, 10, 100)];
        let w85 = &[(five, 85, 100)];
        let w80 = &[(five, 80, 100)];
        let w79 = &[(five, 79, 100)];
        let w99 = &[(five, 99, 100)];
        let half = now() + chrono::Duration::minutes(150);
        let almost = now() + chrono::Duration::minutes(30);
        let half_7d = now() + chrono::Duration::hours(84);

        let cases = [
            Case {
                name: "子开关默认关：即使撑不到 reset 也不提前切",
                settings: on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready_reset("alice", w85, half), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "撑不到 reset 且候选更健康 → 提前切",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready_reset("alice", w85, half), ready("bob", w10)],
                expect: Expect::Pace("alice", "bob", five),
            },
            Case {
                name: "当前撑得到 reset → 不切",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready_reset("alice", w85, almost), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "pace=1（恰好撑到）不切",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    ready_reset("alice", w85, now() + chrono::Duration::minutes(45)),
                    ready("bob", w10),
                ],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "缺 reset_at → pace 路径不参与",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready("alice", w85), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "Month 时长未知 → pace 路径不参与",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    ready_reset("alice", &[(month, 85, 100)], half),
                    ready("bob", w10),
                ],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "Custom 时长未知 → pace 路径不参与",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    ready_reset("alice", &[(QuotaWindow::Custom, 85, 100)], half),
                    ready("bob", w10),
                ],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "reset_at 远在时长之外（尚未进入窗口）不拿 now-时长推起点",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    ready_reset("alice", w85, now() + chrono::Duration::hours(10)),
                    ready("bob", w10),
                ],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "已达耗尽阈值走耗尽路径，pace 不抢占",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready_reset("alice", w99, half), ready("bob", w10)],
                expect: Expect::Swap("alice", "bob", five),
            },
            Case {
                name: "最大已用低于预警水位即使 pace>1 也不切",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready_reset("alice", w79, half), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "预警水位恰好 0.8 且 pace>1 → 切",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready_reset("alice", w80, half), ready("bob", w10)],
                expect: Expect::Pace("alice", "bob", five),
            },
            Case {
                name: "候选不够健康（差值小于边际）不切",
                settings: pace_with_margin(0.05),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    ready_reset("alice", w85, half),
                    ready("bob", &[(five, 82, 100)]),
                ],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "候选更健康（差值等于边际）则切",
                settings: pace_with_margin(0.5),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    // 7/8 与 3/8 均为二进制有限小数，0.875 − 0.5 = 0.375。
                    ready_reset("alice", &[(five, 7, 8)], half),
                    ready("bob", &[(five, 3, 8)]),
                ],
                expect: Expect::Pace("alice", "bob", five),
            },
            Case {
                name: "默认边际 0：候选仅略低仍切",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    ready_reset("alice", w85, half),
                    ready("bob", &[(five, 84, 100)]),
                ],
                expect: Expect::Pace("alice", "bob", five),
            },
            Case {
                name: "摘出停泊账号不能当 pace 候选",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), opted_out_acc("bob", false)],
                snapshots: vec![ready_reset("alice", w85, half), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
            },
            Case {
                name: "摘出账号跳过，选下一更健康候选",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![
                    settled("alice", true),
                    opted_out_acc("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    ready_reset("alice", w85, half),
                    ready("bob", w10),
                    ready("carol", &[(five, 20, 100)]),
                ],
                expect: Expect::Pace("alice", "carol", five),
            },
            Case {
                name: "7d 已知时长参与 pace",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    ready_reset("alice", &[(seven, 85, 100)], half_7d),
                    ready("bob", w10),
                ],
                expect: Expect::Pace("alice", "bob", seven),
            },
            Case {
                name: "多窗口取 pace>1 中最高者作触发窗",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    ready_resets(
                        "alice",
                        &[
                            (five, 85, 100, Some(almost)),
                            (seven, 85, 100, Some(half_7d)),
                        ],
                    ),
                    ready("bob", w10),
                ],
                expect: Expect::Pace("alice", "bob", seven),
            },
            Case {
                name: "pace 命中冷却期仍 Stay Cooldown",
                settings: pace_on(),
                last_auto_swap_at: Some(now() - chrono::Duration::minutes(1)),
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready_reset("alice", w85, half), ready("bob", w10)],
                expect: Expect::Stay(AutoSwapSkipReason::Cooldown),
            },
            Case {
                name: "无合格 pace 候选仍是 BelowThreshold，不报无候选",
                settings: pace_on(),
                last_auto_swap_at: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![ready_reset("alice", w85, half), ready("bob", w99)],
                expect: Expect::Stay(AutoSwapSkipReason::BelowThreshold),
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
            ReturnCase {
                name: "回驻不受防抖边际影响：用量差小于边际仍回驻",
                settings: AutoSwap {
                    candidate_margin: 0.05,
                    ..resident()
                },
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", &[(five, 20, 100)], adv),
                    membered("bob", &[(five, 18, 100)], mid),
                ],
                expect_to: Some("bob"),
            },
            ReturnCase {
                name: "回驻不受防抖边际影响：低档高用量仍回驻",
                settings: AutoSwap {
                    candidate_margin: 0.5,
                    ..resident()
                },
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
                name: "回驻不套用快重置：同档仍按已用比例",
                settings: AutoSwap {
                    prefer_soonest_reset: true,
                    ..resident()
                },
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    membered("alice", w_low, adv),
                    membered_reset(
                        "bob",
                        &[(five, 50, 100)],
                        mid,
                        now() + chrono::Duration::hours(1),
                    ),
                    membered_reset(
                        "carol",
                        &[(five, 20, 100)],
                        mid,
                        now() + chrono::Duration::hours(4),
                    ),
                ],
                expect_to: Some("carol"),
            },
            ReturnCase {
                name: "摘出停泊账号不作为回驻候选，选下一合格低档",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![
                    settled("alice", true),
                    opted_out_acc("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    membered("alice", w_low, adv),
                    membered("bob", w_low, mid),
                    membered("carol", w_low, mid),
                ],
                expect_to: Some("carol"),
            },
            ReturnCase {
                name: "仅有的低档停泊已摘出 → 不回驻",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![settled("alice", true), opted_out_acc("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
            },
            ReturnCase {
                name: "当前账号已摘出仍可被回驻切走",
                settings: resident(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                accounts: vec![opted_out_acc("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: Some("bob"),
            },
        ];

        for case in &cases {
            let mut counts = HashMap::new();
            let got = decide_resident_return(
                &case.accounts,
                &case.snapshots,
                &case.settings,
                now(),
                case.last_auto_swap_at,
                case.manual_hold_until,
                &mut counts,
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

    /// issue #111 / ADR-0012：回驻确认期。默认 N=2；闸门挡住时不改计数。
    #[test]
    fn decide_resident_confirm_table() {
        let five = QuotaWindow::FiveHour;
        let seven = QuotaWindow::SevenDay;
        let w_low = &[(five, 10, 100), (seven, 20, 100)];
        let adv = "LEVEL_ADVANCED";
        let mid = "LEVEL_INTERMEDIATE";

        fn default_confirm() -> AutoSwap {
            AutoSwap {
                resident_lower_tier: true,
                ..on()
            }
        }

        fn confirm_n(n: u32) -> AutoSwap {
            AutoSwap {
                resident_confirm_n: n,
                ..default_confirm()
            }
        }

        fn to_map(pairs: &[(&str, u32)]) -> HashMap<AccountId, u32> {
            pairs
                .iter()
                .map(|(id, n)| (AccountId((*id).into()), *n))
                .collect()
        }

        struct ConfirmCase {
            name: &'static str,
            settings: AutoSwap,
            last_auto_swap_at: Option<DateTime<Utc>>,
            manual_hold_until: Option<DateTime<Utc>>,
            prev: Vec<(&'static str, u32)>,
            accounts: Vec<Account>,
            snapshots: Vec<QuotaSnapshot>,
            expect_to: Option<&'static str>,
            expect_counts: Vec<(&'static str, u32)>,
        }

        let cases = [
            ConfirmCase {
                name: "默认 N=2：首个合格周期不切回，计数 0→1",
                settings: default_confirm(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                prev: vec![],
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
                expect_counts: vec![("bob", 1)],
            },
            ConfirmCase {
                name: "默认 N=2：第 2 个连续合格周期才切回",
                settings: default_confirm(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                prev: vec![("bob", 1)],
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: Some("bob"),
                expect_counts: vec![("bob", 2)],
            },
            ConfirmCase {
                name: "N=3：第 2 个合格周期仍不切",
                settings: confirm_n(3),
                last_auto_swap_at: None,
                manual_hold_until: None,
                prev: vec![("bob", 1)],
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
                expect_counts: vec![("bob", 2)],
            },
            ConfirmCase {
                name: "N=1：首个合格周期即切回",
                settings: confirm_n(1),
                last_auto_swap_at: None,
                manual_hold_until: None,
                prev: vec![],
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: Some("bob"),
                expect_counts: vec![("bob", 1)],
            },
            ConfirmCase {
                name: "N=0 视为 1：首个合格周期即切回",
                settings: confirm_n(0),
                last_auto_swap_at: None,
                manual_hold_until: None,
                prev: vec![],
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: Some("bob"),
                expect_counts: vec![("bob", 1)],
            },
            ConfirmCase {
                name: "中间不合格清零：计数 1 后目标达阈值",
                settings: default_confirm(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                prev: vec![("bob", 1)],
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![
                    membered("alice", w_low, adv),
                    membered("bob", &[(five, 10, 100), (seven, 99, 100)], mid),
                ],
                expect_to: None,
                expect_counts: vec![],
            },
            ConfirmCase {
                name: "清零后须重新连续 N 个周期：再合格只计 1、不切回",
                settings: default_confirm(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                prev: vec![],
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
                expect_counts: vec![("bob", 1)],
            },
            ConfirmCase {
                name: "目标更换：旧计数丢弃，新目标从 1 起、不切回",
                settings: default_confirm(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                prev: vec![("bob", 1)],
                accounts: vec![
                    settled("alice", true),
                    settled("bob", false),
                    settled("carol", false),
                ],
                snapshots: vec![
                    membered("alice", w_low, adv),
                    membered("bob", w_low, mid),
                    membered("carol", w_low, "LEVEL_ANDANTE"),
                ],
                expect_to: None,
                expect_counts: vec![("carol", 1)],
            },
            ConfirmCase {
                name: "冷却期内不改计数、不回驻",
                settings: default_confirm(),
                last_auto_swap_at: Some(now() - chrono::Duration::minutes(1)),
                manual_hold_until: None,
                prev: vec![("bob", 1)],
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
                expect_counts: vec![("bob", 1)],
            },
            ConfirmCase {
                name: "手动保持期内不改计数、不回驻",
                settings: default_confirm(),
                last_auto_swap_at: None,
                manual_hold_until: Some(now() + chrono::Duration::hours(1)),
                prev: vec![("bob", 1)],
                accounts: vec![settled("alice", true), settled("bob", false)],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
                expect_counts: vec![("bob", 1)],
            },
            ConfirmCase {
                name: "安顿宽限期内不改计数、不回驻",
                settings: default_confirm(),
                last_auto_swap_at: None,
                manual_hold_until: None,
                prev: vec![("bob", 1)],
                accounts: vec![
                    acc("alice", true, Some(now() - chrono::Duration::seconds(10))),
                    settled("bob", false),
                ],
                snapshots: vec![membered("alice", w_low, adv), membered("bob", w_low, mid)],
                expect_to: None,
                expect_counts: vec![("bob", 1)],
            },
        ];

        for case in &cases {
            let mut counts = to_map(&case.prev);
            let got = decide_resident_return(
                &case.accounts,
                &case.snapshots,
                &case.settings,
                now(),
                case.last_auto_swap_at,
                case.manual_hold_until,
                &mut counts,
            );
            match (got, case.expect_to) {
                (Some(target), Some(to)) => {
                    assert_eq!(target.from.0, "alice", "{}", case.name);
                    assert_eq!(target.to.0, to, "{}", case.name);
                }
                (None, None) => {}
                (got, expected) => panic!("{}: got {got:?}, expect {expected:?}", case.name),
            }
            assert_eq!(counts, to_map(&case.expect_counts), "{}", case.name);
        }
    }

    /// issue #111：确认计数读改写兼容；缺字段 / 损坏视为 0；空计数不建文件。
    #[test]
    fn resident_confirm_roundtrip_and_compat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto_swap.json");
        assert!(read_resident_confirm_counts(&path).is_empty());
        write_resident_confirm_counts(&path, &HashMap::new());
        assert!(!path.exists(), "全 0 且无文件时不建文件");

        let mut counts = HashMap::new();
        counts.insert(AccountId("bob".into()), 1);
        write_resident_confirm_counts(&path, &counts);
        assert_eq!(read_resident_confirm_counts(&path), counts);

        let at = now();
        write_last_auto_swap_at(&path, at);
        assert_eq!(read_last_auto_swap_at(&path), Some(at));
        assert!(
            read_resident_confirm_counts(&path).is_empty(),
            "成功切换时刻写入应清掉确认计数"
        );

        write_resident_confirm_counts(&path, &counts);
        let hold = now() + chrono::Duration::hours(8);
        set_manual_hold_until(&path, hold);
        assert_eq!(read_manual_hold_until(&path), Some(hold));
        assert_eq!(read_last_auto_swap_at(&path), Some(at));
        assert!(
            read_resident_confirm_counts(&path).is_empty(),
            "手动保持期写入应清掉确认计数"
        );

        let stock = dir.path().join("stock.json");
        std::fs::write(&stock, r#"{"last_auto_swap_at":"2026-09-03T12:00:00Z"}"#).unwrap();
        assert_eq!(read_last_auto_swap_at(&stock), Some(now()));
        assert!(read_resident_confirm_counts(&stock).is_empty());

        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, "not-json").unwrap();
        assert!(read_resident_confirm_counts(&corrupt).is_empty());
    }

    /// issue #111：状态文件跨周期：首个合格不切，第 2 个才切。
    #[test]
    fn decide_resident_return_from_state_two_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto_swap.json");
        let five = QuotaWindow::FiveHour;
        let settings = AutoSwap {
            resident_lower_tier: true,
            ..on()
        };
        let accounts = vec![settled("alice", true), settled("bob", false)];
        let snapshots = vec![
            membered("alice", &[(five, 10, 100)], "LEVEL_ADVANCED"),
            membered("bob", &[(five, 20, 100)], "LEVEL_INTERMEDIATE"),
        ];

        let first = decide_resident_return_from_state(
            &path,
            &accounts,
            &snapshots,
            &settings,
            now(),
            None,
            None,
        );
        assert!(first.is_none(), "首个合格周期不切回");
        let counts = read_resident_confirm_counts(&path);
        assert_eq!(counts.get(&AccountId("bob".into())).copied(), Some(1));

        let second = decide_resident_return_from_state(
            &path,
            &accounts,
            &snapshots,
            &settings,
            now(),
            None,
            None,
        );
        assert_eq!(second.as_ref().map(|t| t.to.0.as_str()), Some("bob"));
        assert_eq!(
            read_resident_confirm_counts(&path)
                .get(&AccountId("bob".into()))
                .copied(),
            Some(2)
        );
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
