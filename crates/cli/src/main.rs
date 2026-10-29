//! kimi-switch（Kimi Code 裁剪版）CLI 入口。
//!
//! 命令面（刻意保持最小）：
//! - `kimi-switch`              — 默认入口：启动一致性自检（以 live 为准）后列出 Kimi 账号 + 5h/7d 额度。
//!   `[auto_swap] enabled = true` 时，额度收集完成后若达阈值则自动切换并追加一行英文结果；
//!   `resident_lower_tier` 子开关开启时，未达阈值也可能因档位更低而回驻（ADR-0008）。
//!   `pace_pre_swap` 开启时，未达阈值但预测撑不到 reset 也可能提前切（ADR-0013）。
//!   `pre_reset_burn` 开启时，未达阈值且 pace 未命中，也可能切到临近重置的停泊账号烧尾巴（ADR-0014）。
//!   退出码只作用于该默认路径：0 已切换 / 1 错误 / 2 无事 / 3 已达阈值但无合格候选。
//!   `--json` 向 stdout 写行式事件流（每条带 `schemaVersion`，契约 additive：字段只增不改、
//!   事件类型可新增，脚本须忽略未知字段）；人类可读提示走 stderr。非 `--json` 时 stdout 不变。
//! - `kimi-switch login kimi`   — 导入当前本机 Kimi Code 已登录账号
//!   （`~/.kimi-code/credentials/kimi-code.json`）。`moonshot` 为 kimi 的公司名兼容别名。
//! - `kimi-switch swap <id|N>`  — 切换激活账号。原子写 + 快照回滚，不依赖网络/quota。
//!   无参数时只打印编号列表，不做切换。
//! - `kimi-switch rm <id|N>`    — 删除账号（registry + 凭证仓库 + 墓碑；网页会员
//!   凭证与会员到期 extra 一并清理）。
//! - `kimi-switch bind-web <id|N>` — 绑定网页登录（issue #50，ADR-0004）：从 stdin
//!   读网页 token（TTY 隐藏回显，**没有 `--token` 旗标**），身份门通过后把会员
//!   到期写进账号 extra，默认列表即可见「有效期至」。
//! - `kimi-switch config`       — 打印生效配置（含 `proactive_renewal.enabled`，只读）。
//!   与 GUI 设置页同源：都是 `<config_dir>/config.toml`。
//! - `kimi-switch on-quota-failure` — 配额失败被动切换（issue #162，spec #152 决策 14；
//!   #184 改认官方 429）：Kimi Code `StopFailure` hook 观察型入口，stdin 读事件 JSON，
//!   命中额度耗尽（命中条件见 [`kimi_switch_core::auto_swap::classify_stop_failure`]）
//!   即以该原文为当前账号耗尽的权威证据，零新增网络查询，按耗尽路径语义（安顿宽限与
//!   手动保持期不阻挡、冷却照常）复用既有自动切换判定与 activate。每个事件在审计日志
//!   追加一行留痕（action `on_quota_failure`：分类与最终决定，不含会话标题与工作目录）；
//!   任何情况下退出码恒 0，stdin 畸形同样只留痕放行。
//! - `kimi-switch on-session-start` — 跟随启动（issue #197，ADR-0021）：Kimi Code
//!   `SessionStart` hook 观察型入口。自动切换总开关开着、当前进程在 Windows 桌面交互会话里、
//!   本会话又没有 GUI 进程时，拉起 CLI 同目录的 GUI（`--background`：最小化、不抢焦点），
//!   不判定、不切换、不查额度。每个事件在审计日志追加一行留痕（action `on_session_start`，
//!   不含会话标题与工作目录）；stdout 不输出任何内容，退出码恒 0，stdin 畸形照常判定。
//! - `kimi-switch install-hook [--uninstall]` — 幂等写入 / 卸载 Kimi Code
//!   `config.toml` 的 `[[hooks]]` StopFailure 条目（指向 `on-quota-failure`）。
//!   除此命令外，工具任何路径不得隐式修改 Kimi Code 配置（spec #152 决策 16）。
//!
//! `<id>` 是账号 id（Kimi user_id）、label 或 `kimi/<id>`；`<N>` 是默认入口
//! 显示的编号（1 起）。`login` / `swap` / `rm` / `bind-web` / `config` 仍是 anyhow 的 0/1。
//!
//! 启动一致性自检（issue #155，spec #152 决策 8）：除 `install-hook` /
//! `on-quota-failure`（观察型，自带 live 对齐）/ `on-session-start`（只读设置，
//! 自检留给被拉起的 GUI）外，每次 CLI 启动都以 live 登录文件
//! 为准比对并纠正注册表 active 标记；live 身份不在账号库时清除标记并在 stderr
//! 提示可导入。纯本地读，一致时零副作用、零可见输出；提示只走 stderr，
//! 不污染 stdout 的账号列表与 `--json` 事件流。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::{
    decide_auto_swap_from_state, decide_resident_return_from_state, earliest_recovery_at,
    locally_invalid_accounts, membership_expiry_from_extra, prettify_membership,
    read_last_auto_swap_at, read_manual_hold_until, record_auto_swap_success,
    set_manual_hold_until, settings, write_last_auto_swap_at, Account, AccountId, AccountRegistry,
    ActivateOutcome, AuditEvent, AuditLog, AutoSwapDecision, AutoSwapKind, AutoSwapLock,
    AutoSwapReturn, AutoSwapSkipReason, AutoSwapTarget, CredentialStore, FileStore, KeyringStore,
    Provider, Quota, QuotaCache, QuotaSnapshot, QuotaState, QuotaWindow, RemovedAccounts,
};
use kimi_switch_kimi::KimiProvider;
use serde::Serialize;

const LONG_ABOUT: &str = "\
Run `kimi-switch` with no arguments to list Kimi accounts and their quota. \
Use `login kimi` / `swap` / `rm` for explicit actions.

Default status path (no subcommand) exit codes:
  0  swapped (exhaustion auto-swap, pace pre-swap, or resident return)
  1  error (including activate failure)
  2  stay / no-op (auto-swap off, below threshold, cooldown, empty list, ...)
  3  threshold reached but no eligible candidate

`--json` writes a line-oriented JSON event stream to stdout. Each event has \
schemaVersion (starts at 1). Event types: swap, stay, no_candidate, error. \
Event fields (omitted when N/A): from, to, kind (exhausted|resident|pace|burn|burn_return), \
window, usagePct, reason, earliestReset, message. \
The contract is additive: fields are only added, never changed; new event types \
may appear; scripts must ignore unknown fields and unknown event types. \
Human-readable listing and auto_swap lines go to stderr when `--json` is set. \
Without `--json`, stdout is the existing account list plus auto_swap lines.

`--json` and these exit codes apply only to the default status path. \
`login` / `swap` / `rm` / `bind-web` / `config` keep exit 0 or 1. \
`on-quota-failure` (StopFailure hook observer) always exits 0. \
`on-session-start` (SessionStart hook observer; launches the GUI when needed) \
always exits 0 and writes nothing to stdout. \
`install-hook` manages the Kimi Code StopFailure hook entry (idempotent).";

#[derive(Parser)]
#[command(
    name = "kimi-switch",
    version,
    about = "Manage and swap between multiple Kimi Code accounts.",
    long_about = LONG_ABOUT
)]
struct Cli {
    /// Log level (equivalent to RUST_LOG).
    #[arg(long, global = true, default_value = "warn")]
    log: String,

    /// Line-oriented JSON events on stdout (default status path). Human-readable output goes to stderr.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import the currently signed-in Kimi Code account.
    Login {
        /// Provider to log in: `kimi` is supported; `moonshot` is an alias.
        provider: String,
    },

    /// Swap to <id|N>. With no argument, prints numbered accounts and exits.
    Swap {
        /// Account index (e.g. `1`), id, label, or `kimi/<id>`.
        id: Option<String>,
    },

    /// Remove <id|N> from the registry and credential store.
    Rm {
        /// Account index (e.g. `1`), id, label, or `kimi/<id>`.
        id: String,
    },

    /// Bind the web login of <id|N> to show membership expiry (reads token from stdin).
    BindWeb {
        /// Account index (e.g. `1`), id, label, or `kimi/<id>`.
        id: String,
    },

    /// Print the effective settings (read-only), as loaded from config.toml.
    Config,

    /// Observe a Kimi Code StopFailure hook event (stdin JSON). On a
    /// quota-exhaustion signal (429 "You've reached your ... usage limit", or
    /// legacy 403 + "usage limit"), run one passive auto-swap reusing the
    /// existing exhaustion-path decision. Other 429s (overloaded, too many
    /// requests, concurrent limit) are ignored. Every event appends one
    /// `on_quota_failure` trace line to the audit log (classification and final
    /// decision; never the session title or cwd). Zero new network queries.
    /// Always exits 0 (observer hook; never blocks the session).
    OnQuotaFailure,

    /// Observe a Kimi Code SessionStart hook event (stdin JSON). Invoked by the
    /// hook; do not run it manually. When auto-swap is enabled and no
    /// kimi-switch GUI runs in this interactive Windows desktop session, launch
    /// the GUI next to this CLI in the background (minimized, no focus steal),
    /// so polling-based auto-swap is present during the session. SSH and other
    /// non-interactive sessions, and non-Windows platforms, never launch. Every
    /// event appends one `on_session_start` trace line to the audit log (never
    /// the session title or cwd). Writes nothing to stdout. Always exits 0.
    OnSessionStart,

    /// Idempotently install (or remove with --uninstall) the Kimi Code
    /// StopFailure [[hooks]] entry wiring events to `on-quota-failure`.
    /// No other code path ever modifies Kimi Code's config.toml.
    InstallHook {
        /// Remove the hook entry instead of installing it.
        #[arg(long)]
        uninstall: bool,
    },
}

/// 进程级共享上下文：明文文件凭证仓库 + registry + Kimi provider。
struct AppContext {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    kimi: Arc<KimiProvider>,
    audit: AuditLog,
}

impl AppContext {
    fn build() -> Result<Self> {
        let paths = AppPaths::resolve()?;
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::with_legacy_keyring(
            paths.credentials_file(),
            KeyringStore::new(),
        ));
        let registry = Arc::new(AccountRegistry::from_default_paths()?);
        let kimi = Arc::new(kimi_switch_kimi::new(store.clone(), registry.clone()));
        let audit = AuditLog::from_default_paths()?;
        Ok(Self {
            store,
            registry,
            kimi,
            audit,
        })
    }

    /// 账号显示顺序（只有 kimi 一个 provider）。`kimi-switch`、`swap N`、`rm N` 共用同一编号映射。
    fn list_ordered(&self) -> Result<Vec<Account>> {
        Ok(self.registry.list_by_provider("kimi")?)
    }

    fn load_removed() -> Result<RemovedAccounts> {
        Ok(RemovedAccounts::load(&AppPaths::resolve()?.removed_file()))
    }
}

/// 把用户传入的引用解析到具体账号。
///
/// 歧义定义：trim 后若能 `parse::<usize>()`，**一律按 1 起编号**，即使该数字
/// 字符串同时也是某个账号的 id / label。`0` 非法；越界报错、不回退别名。
/// 编号命中后不走 `find_unique`。仅当输入不是纯数字时才按 id / label /
/// `kimi/<id>` 解析。
fn resolve_account(ctx: &AppContext, input: &str) -> Result<Account> {
    let trimmed = input.trim();
    if let Ok(n) = trimmed.parse::<usize>() {
        if n == 0 {
            anyhow::bail!("invalid account index 0; numbering starts at 1");
        }
        let ordered = ctx.list_ordered()?;
        return ordered.into_iter().nth(n - 1).with_context(|| {
            format!("no account at index {n}; run `kimi-switch` to see the list")
        });
    }
    ctx.registry
        .find_unique(trimmed)?
        .filter(|a| a.provider == "kimi")
        .with_context(|| format!("account not found: {trimmed}"))
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cli.log.clone()));
    // 日志只走 stderr（fmt 默认写 stdout）：stdout 留给账号列表、`--json` 事件流，hook 子命令
    // 的 stdout 必须为空（SessionStart 的 stdout 可能被加进模型上下文，issue #197）。
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    // 启动时加载 config.toml（缺失 / 解析失败时沿用默认值 + warn）。
    if let Err(e) = settings::reload_from_file() {
        tracing::warn!(err = %e, "load config failed; using built-in defaults");
    }

    // 跟随启动拉起器（issue #197）只读设置、只写审计：不构建账号库 / 凭证 / provider 上下文，
    // 也不跑启动一致性自检（被拉起的 GUI 自己会跑）。退出码恒 0，stdout 不输出任何内容。
    if matches!(cli.cmd, Some(Cmd::OnSessionStart)) {
        on_session_start();
        return Ok(ExitCode::SUCCESS);
    }

    // ctx 按命令消费：`on-quota-failure` 是观察型 hook，任何内部错误（含 ctx 构建
    // 失败）只写 stderr 日志，退出码恒 0，绝不阻断用户会话（issue #162）。
    let ctx = AppContext::build();

    match cli.cmd {
        Some(Cmd::OnQuotaFailure) => {
            match ctx.as_ref() {
                Ok(ctx) => on_quota_failure(ctx).await,
                Err(err) => tracing::warn!(err = %err, "on-quota-failure: context unavailable"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::InstallHook { uninstall }) => install_hook(uninstall).map(|()| ExitCode::SUCCESS),
        other => {
            let ctx = ctx?;
            // 启动一致性自检（issue #155）：以 live 为准纠正 active / 清除工具外身份
            // 标记；一致时零副作用零输出。提示走 stderr，stdout 保持机器可读契约。
            if let Some(notice) = startup_check_notice(&startup_check(&ctx)) {
                eprintln!("{notice}");
            }
            match other {
                None => match status(&ctx, cli.json).await {
                    Ok(code) => Ok(code),
                    Err(err) => {
                        if cli.json {
                            emit_status_output(
                                true,
                                &[],
                                &StatusJsonEvent::error(None, format!("{err:#}")),
                            )?;
                        }
                        Err(err)
                    }
                },
                Some(Cmd::Login { provider }) => {
                    login(&ctx, &provider).await.map(|()| ExitCode::SUCCESS)
                }
                Some(Cmd::Swap { id }) => {
                    swap(&ctx, id.as_deref()).await.map(|()| ExitCode::SUCCESS)
                }
                Some(Cmd::Rm { id }) => rm(&ctx, &id).await.map(|()| ExitCode::SUCCESS),
                Some(Cmd::BindWeb { id }) => bind_web(&ctx, &id).await.map(|()| ExitCode::SUCCESS),
                Some(Cmd::Config) => config().map(|()| ExitCode::SUCCESS),
                // OnSessionStart / OnQuotaFailure / InstallHook 已在前面的分支消费，这里不可达。
                Some(Cmd::OnSessionStart)
                | Some(Cmd::OnQuotaFailure)
                | Some(Cmd::InstallHook { .. }) => unreachable!(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 默认入口：账号列表 + 额度
// ---------------------------------------------------------------------------

async fn status(ctx: &AppContext, json: bool) -> Result<ExitCode> {
    // 1. 列出账号（启动一致性自检已在 main 统一执行，见文件头说明）。
    let mut accounts = ctx.list_ordered()?;
    if accounts.is_empty() {
        let effect = AutoSwapStatusEffect::stay("no_accounts");
        emit_status_output(
            json,
            &["No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.".to_string()],
            &effect.json,
        )?;
        return Ok(ExitCode::from(effect.exit_code()));
    }

    // 2. 额度快照（节流 / 退避 / 并发 / 陈旧回退收口在 core）。
    let cache_path = AppPaths::resolve()?.quota_cache_file();
    let ids: Vec<_> = accounts.iter().map(|account| account.id.clone()).collect();
    let mut snapshots = kimi_switch_core::quota_snapshot::collect(
        ctx.kimi.clone(),
        "kimi",
        &ids,
        &cache_path,
        kimi_switch_core::QuotaCollectReason::Scheduled,
    )
    .await;
    // `/usages` 移除 user 对象（2026-09-14）后查询不带档位：用 extra 的 /me 缓存
    // 补章（下方渲染与自动切换判定共用这批快照）。
    kimi_switch_core::apply_cached_membership(&mut snapshots, &accounts);

    // 3. 自动切换：开关关闭则短路（零判定、不读冷却文件）。触发时走既有 activate。
    let auto_swap = maybe_run_status_auto_swap(ctx, &accounts, &snapshots).await;
    if auto_swap.swapped {
        // 自动切换后再对齐一次（activate 已保证一致，正常零副作用的自检兜底）；
        // 不渲染提示：此刻的账号列表即是最新的自检结果。
        startup_check(ctx);
        accounts = ctx.list_ordered()?;
    }

    // 4. 渲染。账号 id 旁带档位名与会员到期（extra 有 → `有效期至 <日期>`；
    //    无 → 绑定弱提示）；节流命中不标 stale，失败回落标 `(stale)`。
    let mut human_lines = Vec::with_capacity(accounts.len() + 2);
    human_lines.push("kimi".to_string());
    for (idx, account) in accounts.iter().enumerate() {
        let n = idx + 1;
        let star = if account.active { "*" } else { " " };
        let snapshot = snapshots.iter().find(|item| item.account_id == account.id);
        let plan_text = snapshot
            .and_then(|item| membership_status_text(item.membership.as_deref(), &account.extra))
            .map(|plan| format!(" {plan}"))
            .unwrap_or_default();
        let quota_text = match snapshot {
            None => "quota: n/a".to_string(),
            Some(snapshot) => match &snapshot.state {
                QuotaState::Failed(error) => format!("quota: {error}"),
                QuotaState::Ready(quotas) | QuotaState::Cached(quotas) if snapshot.stale => {
                    format!("{} (stale)", format_quotas(quotas))
                }
                QuotaState::Ready(quotas) | QuotaState::Cached(quotas) => format_quotas(quotas),
            },
        };
        human_lines.push(format!(
            "  {star} {n:>2}  {:<24}{plan_text} {quota_text}",
            account.id
        ));
    }
    if let Some(line) = &auto_swap.line {
        human_lines.push(line.clone());
    }
    emit_status_output(json, &human_lines, &auto_swap.json)?;
    Ok(ExitCode::from(auto_swap.exit_code()))
}

/// 默认 status 输出：非 `--json` 时人类可读行进 stdout；`--json` 时事件进 stdout、
/// 人类可读进 stderr。写完立即 flush，避免 Windows 管道下退出码抢在缓冲之前。
fn emit_status_output(json: bool, human_lines: &[String], event: &StatusJsonEvent) -> Result<()> {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    emit_status_output_to(
        json,
        &mut stdout.lock(),
        &mut stderr.lock(),
        human_lines,
        event,
    )
}

fn emit_status_output_to(
    json: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    human_lines: &[String],
    event: &StatusJsonEvent,
) -> Result<()> {
    if json {
        writeln!(stdout, "{}", serde_json::to_string(event)?)?;
        stdout.flush()?;
        for line in human_lines {
            writeln!(stderr, "{line}")?;
        }
        stderr.flush()?;
    } else {
        for line in human_lines {
            writeln!(stdout, "{line}")?;
        }
        stdout.flush()?;
    }
    Ok(())
}

/// JSON 事件 `schemaVersion`（issue #113）；只增不改，从 1 起。
const JSON_SCHEMA_VERSION: u32 = 1;

/// 默认 status 路径的语义化退出码（issue #113）：
/// 0 已切换 / 1 错误 / 2 无事 / 3 已达阈值但无合格候选。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusExitKind {
    Swapped,
    Error,
    Stay,
    NoCandidate,
}

impl StatusExitKind {
    fn code(self) -> u8 {
        match self {
            Self::Swapped => 0,
            Self::Error => 1,
            Self::Stay => 2,
            Self::NoCandidate => 3,
        }
    }
}

/// 行式 JSON 事件（schemaVersion 从 1 起）。字段只增不改；未知字段由脚本忽略。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusJsonEvent {
    schema_version: u32,
    #[serde(rename = "type")]
    event_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_pct: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    earliest_reset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl StatusJsonEvent {
    fn base(event_type: &'static str) -> Self {
        Self {
            schema_version: JSON_SCHEMA_VERSION,
            event_type,
            from: None,
            to: None,
            kind: None,
            window: None,
            usage_pct: None,
            reason: None,
            earliest_reset: None,
            message: None,
        }
    }

    fn swap_exhausted(target: &AutoSwapTarget) -> Self {
        Self::swap_target(target, "exhausted")
    }

    fn swap_pace(target: &AutoSwapTarget) -> Self {
        Self::swap_target(target, "pace")
    }

    fn swap_burn(target: &AutoSwapTarget) -> Self {
        Self::swap_target(target, "burn")
    }

    fn swap_burn_return(target: &AutoSwapTarget) -> Self {
        Self::swap_target(target, "burn_return")
    }

    fn swap_target(target: &AutoSwapTarget, kind: &'static str) -> Self {
        Self {
            from: Some(target.from.to_string()),
            to: Some(target.to.to_string()),
            kind: Some(kind),
            window: Some(quota_window_short(target.trigger_window)),
            usage_pct: Some(auto_swap_pct(target.usage_ratio)),
            ..Self::base("swap")
        }
    }

    fn swap_resident(target: &AutoSwapReturn) -> Self {
        Self {
            from: Some(target.from.to_string()),
            to: Some(target.to.to_string()),
            kind: Some("resident"),
            ..Self::base("swap")
        }
    }

    fn stay(reason: &'static str) -> Self {
        Self {
            reason: Some(reason),
            ..Self::base("stay")
        }
    }

    fn no_candidate(earliest_recovery: Option<DateTime<Utc>>) -> Self {
        Self {
            earliest_reset: earliest_recovery
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            ..Self::base("no_candidate")
        }
    }

    fn error(to: Option<&AccountId>, message: String) -> Self {
        Self {
            to: to.map(ToString::to_string),
            message: Some(message),
            ..Self::base("error")
        }
    }
}

fn skip_reason_json(reason: AutoSwapSkipReason) -> &'static str {
    match reason {
        AutoSwapSkipReason::Disabled => "disabled",
        AutoSwapSkipReason::NoActiveAccount => "no_active_account",
        AutoSwapSkipReason::Cooldown => "cooldown",
        AutoSwapSkipReason::SettleGrace => "settle_grace",
        AutoSwapSkipReason::CurrentUnavailable => "current_unavailable",
        AutoSwapSkipReason::CurrentUnknown => "current_unknown",
        AutoSwapSkipReason::BelowThreshold => "below_threshold",
        AutoSwapSkipReason::NoEligibleCandidate => "no_eligible_candidate",
    }
}

/// CLI status 自动切换的可观察结果：列表后追加的一行、是否已成功切换、
/// 以及默认 status 路径的退出码 / JSON 事件（issue #113）。
struct AutoSwapStatusEffect {
    line: Option<String>,
    swapped: bool,
    exit: StatusExitKind,
    json: StatusJsonEvent,
}

impl AutoSwapStatusEffect {
    fn stay(reason: &'static str) -> Self {
        Self {
            line: None,
            swapped: false,
            exit: StatusExitKind::Stay,
            json: StatusJsonEvent::stay(reason),
        }
    }

    fn swapped(line: String, json: StatusJsonEvent) -> Self {
        Self {
            line: Some(line),
            swapped: true,
            exit: StatusExitKind::Swapped,
            json,
        }
    }

    /// `earliest_recovery` 由调用方经 [`earliest_recovery_at`] 算一次，说明行与 JSON 同口径。
    fn no_candidate(earliest_recovery: Option<DateTime<Utc>>) -> Self {
        Self {
            line: Some(auto_swap_no_candidate_line(earliest_recovery)),
            swapped: false,
            exit: StatusExitKind::NoCandidate,
            json: StatusJsonEvent::no_candidate(earliest_recovery),
        }
    }

    fn activate_failed(to: &AccountId, err: impl std::fmt::Display) -> Self {
        let message = err.to_string();
        Self {
            line: Some(auto_swap_failed_line(to, &message)),
            swapped: false,
            exit: StatusExitKind::Error,
            json: StatusJsonEvent::error(Some(to), message),
        }
    }

    fn exit_code(&self) -> u8 {
        self.exit.code()
    }

    #[cfg(test)]
    fn json_line(&self) -> String {
        serde_json::to_string(&self.json).expect("StatusJsonEvent 可序列化")
    }
}

async fn maybe_run_status_auto_swap(
    ctx: &AppContext,
    accounts: &[Account],
    snapshots: &[QuotaSnapshot],
) -> AutoSwapStatusEffect {
    let settings = settings::current();
    if !settings.auto_swap.enabled {
        return AutoSwapStatusEffect::stay("disabled");
    }
    let Ok(paths) = AppPaths::resolve() else {
        return AutoSwapStatusEffect::stay("unavailable");
    };
    let lock_path = paths.auto_swap_lock_file();
    let kimi = ctx.kimi.clone();
    apply_status_auto_swap(
        &settings.auto_swap,
        accounts,
        snapshots,
        Utc::now(),
        move || AutoSwapLock::acquire_async(lock_path),
        move |to, expected_current| async move {
            kimi.activate_if_current(&to, &expected_current).await
        },
        &ctx.audit,
        &paths.auto_swap_state_file(),
    )
    .await
}

/// collect 完成后的自动切换编排，整段在自动切换串行化锁内（ADR-0020）：拿锁 → 重读状态 →
/// 判定 → 带期望当前账号的切换 → 写冷却。开关关闭立即返回，不拿锁。`acquire_lock` 拿锁，3 秒
/// 等不到（另一自动触发正在切换）时以 `busy` 为不切原因：退出码 2，与 `superseded` 同为 Stay，
/// 不判定、不切换、不审计、不写状态。冷却与手动保持期在拿到锁之后才读，往返记忆、回驻确认计数
/// 与耗尽账本由判定自己从状态文件读，前一个持锁者刚写下的状态因此都看得见。activate 失败不写
/// 冷却。耗尽判定 Stay 时再评估回驻（issue #97 / ADR-0008）：无合格目标完全静默，不套用「没有
/// 可切换的账号」提示；回驻 activate 失败同样不写冷却。
/// `activate(目标, 期望当前账号)` 是带前置条件的切换（期望当前账号，ADR-0020），期望当前账号
/// 取决定时的当前账号；返回「已被取代」时以 `superseded` 为不切原因，不审计、不写冷却。
async fn apply_status_auto_swap<L, LFut, G, F, Fut>(
    auto_swap: &kimi_switch_core::settings::AutoSwap,
    accounts: &[Account],
    snapshots: &[QuotaSnapshot],
    now: DateTime<Utc>,
    acquire_lock: L,
    activate: F,
    audit: &AuditLog,
    state_path: &Path,
) -> AutoSwapStatusEffect
where
    L: FnOnce() -> LFut,
    LFut: std::future::Future<Output = Option<G>>,
    F: FnOnce(AccountId, AccountId) -> Fut,
    Fut: std::future::Future<Output = kimi_switch_core::Result<ActivateOutcome>>,
{
    if !auto_swap.enabled {
        return AutoSwapStatusEffect::stay("disabled");
    }
    let Some(_serialized) = acquire_lock().await else {
        return AutoSwapStatusEffect::stay("busy");
    };
    let last_auto_swap_at = read_last_auto_swap_at(state_path);
    // 手动保持期与冷却同住一个状态文件（ADR-0008 第 4/5 条）。
    let manual_hold_until = read_manual_hold_until(state_path);
    match decide_auto_swap_from_state(
        state_path,
        accounts,
        snapshots,
        auto_swap,
        now,
        last_auto_swap_at,
    ) {
        AutoSwapDecision::Stay {
            reason: AutoSwapSkipReason::NoEligibleCandidate,
        } => {
            let current = accounts.iter().find(|account| account.active);
            audit.append(AuditEvent::err(
                "auto_swap",
                "kimi",
                current.map(|account| account.id.0.as_str()),
                "no eligible candidate",
            ));
            AutoSwapStatusEffect::no_candidate(earliest_recovery_at(state_path, snapshots, now))
        }
        // 未达耗尽阈值 / 冷却 / 宽限期等 Stay：耗尽路径不动作，转交回驻判定。
        AutoSwapDecision::Stay { reason } => {
            match decide_resident_return_from_state(
                state_path,
                accounts,
                snapshots,
                auto_swap,
                now,
                last_auto_swap_at,
                manual_hold_until,
            ) {
                None => AutoSwapStatusEffect::stay(skip_reason_json(reason)),
                // activate 段计时（issue #153 续跑）：自动切换与手动切换同口径，
                // 只覆盖 activate 段，耗时写入审计 duration_ms。
                Some(target) => {
                    let activate_started = std::time::Instant::now();
                    match activate(target.to.clone(), target.from.clone()).await {
                        Ok(ActivateOutcome::Superseded) => AutoSwapStatusEffect::stay("superseded"),
                        Ok(ActivateOutcome::Activated) => {
                            write_last_auto_swap_at(state_path, Utc::now());
                            audit.append(
                                AuditEvent::ok("auto_swap", "kimi", Some(target.to.0.as_str()))
                                    .with_detail(resident_return_audit_detail(&target))
                                    .with_duration_ms(
                                        activate_started.elapsed().as_millis() as u64,
                                    ),
                            );
                            AutoSwapStatusEffect::swapped(
                                resident_return_line(&target),
                                StatusJsonEvent::swap_resident(&target),
                            )
                        }
                        Err(err) => {
                            audit.append(AuditEvent::err(
                                "auto_swap",
                                "kimi",
                                Some(target.to.0.as_str()),
                                &err.to_string(),
                            ));
                            AutoSwapStatusEffect::activate_failed(&target.to, err)
                        }
                    }
                }
            }
        }
        AutoSwapDecision::Swap(target) => {
            // activate 段计时（issue #153 续跑）：耗尽 / pace / 抢用 / 回程共用，
            // 与手动切换同口径写入审计 duration_ms。
            let activate_started = std::time::Instant::now();
            match activate(target.to.clone(), target.from.clone()).await {
                Ok(ActivateOutcome::Superseded) => AutoSwapStatusEffect::stay("superseded"),
                Ok(ActivateOutcome::Activated) => {
                    record_auto_swap_success(state_path, Utc::now(), &target, snapshots);
                    audit.append(
                        AuditEvent::ok("auto_swap", "kimi", Some(target.to.0.as_str()))
                            .with_detail(auto_swap_audit_detail(&target))
                            .with_duration_ms(activate_started.elapsed().as_millis() as u64),
                    );
                    AutoSwapStatusEffect::swapped(
                        auto_swap_success_line(&target),
                        match target.kind {
                            AutoSwapKind::Pace => StatusJsonEvent::swap_pace(&target),
                            AutoSwapKind::Burn => StatusJsonEvent::swap_burn(&target),
                            AutoSwapKind::BurnReturn => StatusJsonEvent::swap_burn_return(&target),
                            AutoSwapKind::Exhausted => StatusJsonEvent::swap_exhausted(&target),
                        },
                    )
                }
                Err(err) => {
                    audit.append(AuditEvent::err(
                        "auto_swap",
                        "kimi",
                        Some(target.to.0.as_str()),
                        &err.to_string(),
                    ));
                    AutoSwapStatusEffect::activate_failed(&target.to, err)
                }
            }
        }
    }
}

fn auto_swap_pct(ratio: f64) -> i64 {
    (ratio * 100.0).round() as i64
}

fn quota_window_short(window: QuotaWindow) -> &'static str {
    match window {
        QuotaWindow::FiveHour => "5h",
        QuotaWindow::SevenDay => "7d",
        QuotaWindow::Month => "30d",
        _ => "custom",
    }
}

fn auto_swap_success_line(target: &AutoSwapTarget) -> String {
    let window = quota_window_short(target.trigger_window);
    let pct = auto_swap_pct(target.usage_ratio);
    match target.kind {
        AutoSwapKind::Pace => format!(
            "auto_swap kimi/{} → kimi/{} (pace {window} {pct}%)",
            target.from, target.to
        ),
        AutoSwapKind::Burn => format!(
            "auto_swap kimi/{} → kimi/{} (burn {window} {pct}%)",
            target.from, target.to
        ),
        AutoSwapKind::BurnReturn => format!(
            "auto_swap kimi/{} → kimi/{} (burn_return {window} {pct}%)",
            target.from, target.to
        ),
        AutoSwapKind::Exhausted => format!(
            "auto_swap kimi/{} → kimi/{} ({window} {pct}%)",
            target.from, target.to
        ),
    }
}

fn auto_swap_no_candidate_line(earliest_recovery: Option<DateTime<Utc>>) -> String {
    match earliest_recovery {
        Some(at) => format!(
            "auto_swap: no eligible candidate (earliest reset {})",
            at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        None => "auto_swap: no eligible candidate".to_string(),
    }
}

fn auto_swap_failed_line(to: &AccountId, err: impl std::fmt::Display) -> String {
    format!("auto_swap kimi/{to} failed: {err}")
}

/// 回驻成功行：与耗尽成功行同风格，括号内标注 resident return（ADR-0008）。
fn resident_return_line(target: &AutoSwapReturn) -> String {
    format!(
        "auto_swap kimi/{} → kimi/{} (resident return)",
        target.from, target.to
    )
}

/// 回驻审计细节：kind=return 标识非耗尽触发；目标账号在事件的 account 字段里。
fn resident_return_audit_detail(target: &AutoSwapReturn) -> String {
    format!("kind=return from={}", target.from)
}

fn auto_swap_audit_detail(target: &AutoSwapTarget) -> String {
    let body = format!(
        "from={} window={} used={}%",
        target.from,
        quota_window_short(target.trigger_window),
        auto_swap_pct(target.usage_ratio)
    );
    match target.kind {
        AutoSwapKind::Pace => format!("kind=pace {body}"),
        AutoSwapKind::Burn => format!("kind=burn {body}"),
        AutoSwapKind::BurnReturn => format!("kind=burn_return from={}", target.from),
        AutoSwapKind::Exhausted => body,
    }
}

/// 账号行的会员档位段（issue #49）：档位名 + `有效期至 <东八区日期>`
/// （extra 有到期瞬间）或 `bind web login to show expiry`（绑定弱提示）。
/// 档位缺失（无额度数据）时返回 `None`，该行保持现状，不强加提示。
fn membership_status_text(
    membership: Option<&str>,
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let name = prettify_membership(membership?);
    Some(match membership_expiry_from_extra(extra) {
        Some(expiry) => format!("{name} 有效期至 {expiry}"),
        None => format!("{name} bind web login to show expiry"),
    })
}

/// 启动一致性自检（issue #155，spec #152 决策 8）：以 live 为准比对并纠正注册表
/// active 标记；live 身份不在账号库（工具外登录）时清除标记并返回导入提示信号。
/// 零网络；一致、live 不可读或墓碑命中时零副作用。
fn startup_check(ctx: &AppContext) -> kimi_switch_core::StartupCheckOutcome {
    let Ok(paths) = AppPaths::resolve() else {
        return kimi_switch_core::StartupCheckOutcome::Consistent;
    };
    kimi_switch_core::startup_consistency_check(
        "kimi",
        &paths.removed_file(),
        ctx.registry.as_ref(),
        &ctx.audit,
        || ctx.kimi.live_account_id(),
    )
}

/// 自检结果的用户可见提示（issue #155）。一致或无法判定时返回 `None`
/// （零可见输出）；其余输出简洁中文，由调用方写 stderr。
fn startup_check_notice(outcome: &kimi_switch_core::StartupCheckOutcome) -> Option<String> {
    use kimi_switch_core::StartupCheckOutcome;
    match outcome {
        StartupCheckOutcome::Consistent => None,
        StartupCheckOutcome::CorrectedActive { account, previous } => Some(match previous {
            Some(prev) => {
                format!("已按登录文件纠正当前账号标记：kimi/{account}（原 kimi/{prev}）")
            }
            None => format!("已按登录文件纠正当前账号标记：kimi/{account}"),
        }),
        StartupCheckOutcome::UntrackedLiveIdentity { live } => Some(format!(
            "检测到工具外身份 kimi/{live}，不在账号库；可运行 kimi-switch login kimi 导入收进账号库"
        )),
    }
}

/// 把多窗口额度渲染成一行：`5h 18% (18/100) resets 08-15 20:52 · 7d 4% (4/100)`。
fn format_quotas(quotas: &[Quota]) -> String {
    if quotas.is_empty() {
        return "quota: n/a".to_string();
    }
    quotas
        .iter()
        .map(|q| {
            let window = match q.window {
                QuotaWindow::FiveHour => "5h",
                QuotaWindow::SevenDay => "7d",
                QuotaWindow::Month => "30d",
                _ => "custom",
            };
            let pct = q
                .usage_ratio()
                .map(|r| format!("{:.0}%", r * 100.0))
                .unwrap_or_else(|| "?".to_string());
            let mut text = format!("{window} {pct} ({}/{})", q.used, q.limit);
            if let Some(reset) = q.reset_at {
                text.push_str(&format!(
                    " resets {}",
                    reset.with_timezone(&chrono::Local).format("%m-%d %H:%M")
                ));
            }
            text
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

// ---------------------------------------------------------------------------
// login kimi（`moonshot` 为公司名兼容别名，不是第二套 provider）
// ---------------------------------------------------------------------------

async fn login(ctx: &AppContext, provider: &str) -> Result<()> {
    match provider {
        "kimi" | "moonshot" => {
            // Kimi 登录是交互式 TUI：约定用户先在 kimi 里登录好，这里只导入当前登录的凭证。
            let account = ctx
                .kimi
                .import_active(None)
                .context("import Kimi login; sign in to Kimi Code first")?;
            ctx.registry
                .set_active("kimi", &account.id)
                .context("mark Kimi login active")?;
            if let Ok(Some(blob)) =
                ctx.store
                    .get("kimi", account.id.0.as_str(), ctx.kimi.store_field())
            {
                kimi_switch_kimi::cache_official_profile_from_blob(
                    ctx.registry.as_ref(),
                    &account.id,
                    &blob,
                )
                .await;
            }
            kimi_switch_kimi::web_membership::restore_membership_after_import(
                ctx.registry.as_ref(),
                ctx.store.as_ref(),
                &account.id,
            )
            .await;
            if let Ok(mut removed) = AppContext::load_removed() {
                if let Err(e) = removed.clear("kimi", account.id.0.as_str()) {
                    tracing::warn!(err=%e, "failed to clear removed-account marker");
                }
            }
            ctx.audit
                .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
            println!("login → kimi/{}", account.id);
            Ok(())
        }
        other => {
            anyhow::bail!("unknown provider: {other} (`kimi` is supported; `moonshot` is an alias)")
        }
    }
}

// ---------------------------------------------------------------------------
// swap
// ---------------------------------------------------------------------------

/// 手动切换死卡提示行（issue #156，spec #152 决策 9）：目标凭证本地已确认失效
/// （invalid_grant 指纹命中）时切换照常成功，stdout 成功行之后向 stderr 补这行
/// 中文警告；与 GUI toast 警告文本同文。
const DEAD_CREDENTIAL_WARNING: &str = "该账号凭证已失效，建议重新授权";

/// 手动切换死卡快检（issue #156，spec #152 决策 9）：目标凭证 refresh token
/// 指纹命中本地 invalid_grant 拒止集合时返回警告行（调用方写 stderr）。复用
/// core 共享原语 [`locally_invalid_accounts`]（#158 裁定口径：只认指纹、不认
/// JWT exp——access 过期但 refresh 有效的账号激活后自愈，不是死卡）。纯本地
/// 只读 credentials.json 与拒止指纹文件，零网络；任何读取/解析失败 fail-open
/// 返回 `None`（不因提示机制破坏切换）。
fn dead_credential_warning(account: &Account) -> Option<&'static str> {
    let paths = AppPaths::resolve().ok()?;
    let dead = locally_invalid_accounts(
        &paths.auto_swap_state_file(),
        std::slice::from_ref(account),
        Utc::now(),
    );
    dead.contains(&account.id)
        .then_some(DEAD_CREDENTIAL_WARNING)
}

/// 把死卡警告写往 stderr（issue #156）：`None` 零输出；写失败由调用方忽略——
/// 提示机制绝不影响切换结果。测试经 writer 断言行内容（与
/// `emit_status_output_to` 同款 seam）。
fn write_dead_credential_warning(
    warning: Option<&str>,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    if let Some(text) = warning {
        writeln!(out, "{text}")?;
        out.flush()?;
    }
    Ok(())
}

/// 显式切换激活账号。手动入口，不依赖网络/quota（原子写 + 快照回滚在 provider 内部）。
async fn swap(ctx: &AppContext, id_input: Option<&str>) -> Result<()> {
    let Some(input) = id_input else {
        print_listing(ctx)?;
        return Ok(());
    };

    let acc = resolve_account(ctx, input)?;
    // activate 段计时（issue #153）：纯本地原子写 + 快照回滚 + 文件锁，零网络；
    // 耗时写入审计 duration_ms，预算对照 P50 < 200ms（spec #152 决策 4）。
    let activate_started = std::time::Instant::now();
    match ctx.kimi.activate(&acc.id).await {
        Ok(()) => {
            ctx.audit.append(
                AuditEvent::ok("activate", "kimi", Some(acc.id.0.as_str()))
                    .with_duration_ms(activate_started.elapsed().as_millis() as u64),
            );
            // 手动切换写保持期（issue #97 / ADR-0008，ADR-0014 第 10 条）：期间回驻与
            // 新开抢用不触发，耗尽被迫切换不受影响。只写本机状态文件，不引入网络依赖；
            // 写失败在 core 内只 warn，不影响 swap 成功结果。
            if let Ok(paths) = AppPaths::resolve() {
                set_manual_hold_until(
                    &paths.auto_swap_state_file(),
                    Utc::now()
                        + chrono::Duration::milliseconds(
                            settings::current().auto_swap.manual_hold_ms,
                        ),
                );
            }
            println!("swap → kimi/{}", acc.id);
            // 死卡提示（issue #156，spec #152 决策 9）：切换已照常成功，stdout
            // 成功行之后向 stderr 补一行中文警告；判定纯本地 fail-open，写失败
            // 也忽略，绝不影响切换结果。
            let _ = write_dead_credential_warning(
                dead_credential_warning(&acc).as_deref(),
                &mut std::io::stderr(),
            );
            Ok(())
        }
        Err(e) => {
            ctx.audit.append(AuditEvent::err(
                "activate",
                "kimi",
                Some(acc.id.0.as_str()),
                &e.to_string(),
            ));
            Err(anyhow::Error::from(e).context(format!("swap kimi/{} failed", acc.id)))
        }
    }
}

/// 无参 `kimi-switch swap`：列出编号 + 用法。**故意不查 quota**，保持「manual swap 不依赖网络」。
fn print_listing(ctx: &AppContext) -> Result<()> {
    let ordered = ctx.list_ordered()?;
    if ordered.is_empty() {
        println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
        return Ok(());
    }
    println!("Usage: kimi-switch swap <N | id | kimi/id>");
    println!();
    for (idx, acc) in ordered.iter().enumerate() {
        let n = idx + 1;
        let star = if acc.active { "*" } else { " " };
        println!("  {star} {n:>2}  kimi/{}", acc.id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

async fn rm(ctx: &AppContext, id_input: &str) -> Result<()> {
    let acc = resolve_account(ctx, id_input)?;
    let paths = AppPaths::resolve()?;
    kimi_switch_core::remove_account(
        ctx.registry.as_ref(),
        ctx.store.as_ref(),
        ctx.kimi.store_field(),
        &paths.removed_file(),
        &paths.quota_cache_file(),
        &ctx.audit,
        "kimi",
        &acc.id,
    )?;
    println!("removed kimi/{}", acc.id);
    Ok(())
}

// ---------------------------------------------------------------------------
// bind-web
// ---------------------------------------------------------------------------

/// 绑定网页登录（issue #50，ADR-0004）。token 只从 stdin 读（TTY 隐藏回显），
/// **没有 `--token` 旗标**（命令行参数会进 shell 历史与进程列表）。
/// 管道在 kimi 适配器：身份门（JWT `sub` 回退 `user_id` 比对账号 id）失败时
/// 英文报错、库存不动；成功后账号 extra 带会员到期，默认列表可见「有效期至」。
async fn bind_web(ctx: &AppContext, id_input: &str) -> Result<()> {
    let acc = resolve_account(ctx, id_input)?;
    let web_token = read_web_token_from_stdin()?;

    let outcome = kimi_switch_kimi::web_membership::bind_web(
        ctx.registry.as_ref(),
        ctx.store.as_ref(),
        &acc.id,
        &web_token,
    )
    .await;

    match outcome {
        Ok(binding) => {
            ctx.audit
                .append(AuditEvent::ok("bind-web", "kimi", Some(acc.id.0.as_str())));
            match binding.expiry_text.as_deref() {
                // 到期日与默认列表同一口径（#49 format_membership_expiry，东八区日历日）。
                Some(expiry) => println!(
                    "bind-web → kimi/{} (membership valid until {expiry})",
                    acc.id
                ),
                None => println!("bind-web → kimi/{} (no membership end time found)", acc.id),
            }
            Ok(())
        }
        Err(e) => {
            // 管道保证错误文案英文且不含 token。
            ctx.audit.append(AuditEvent::err(
                "bind-web",
                "kimi",
                Some(acc.id.0.as_str()),
                &e.to_string(),
            ));
            Err(anyhow::Error::from(e).context(format!("bind-web kimi/{} failed", acc.id)))
        }
    }
}

/// 从 stdin 读网页 token：TTY 下用 [`dialoguer::Password`] 隐藏回显；
/// 非 TTY（管道 / 重定向）按行读取（脚本与测试走这条路径）。
/// 空输入视为失败——没有 token 就没有绑定。
fn read_web_token_from_stdin() -> Result<String> {
    use std::io::IsTerminal;

    let raw = if std::io::stdin().is_terminal() {
        dialoguer::Password::new()
            .with_prompt("Paste web token (hidden; from www.kimi.com)")
            .interact()
            .context("read web token from terminal")?
    } else {
        use std::io::BufRead as _;
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("read web token from stdin")?;
        line
    };
    normalize_web_token(raw)
}

/// 规整 stdin 读到的 token：去首尾空白（含换行）；空输入视为失败。
fn normalize_web_token(raw: String) -> Result<String> {
    let token = raw.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("no web token provided on stdin");
    }
    Ok(token)
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// 打印生效配置（只读）。GUI 设置页与本命令读写同一 `config.toml`。
fn config() -> Result<()> {
    print!("{}", render_config()?);
    Ok(())
}

/// 构造生效配置的 TOML 文本（与打印分离，便于测试断言内容）。
fn render_config() -> Result<String> {
    let current = settings::current();
    toml::to_string_pretty(current.as_ref()).context("render effective settings")
}

// ---------------------------------------------------------------------------
// on-quota-failure（配额失败被动切换，issue #162 / spec #152 决策 14）
// ---------------------------------------------------------------------------

/// Kimi Code `StopFailure` hook 观察型入口：stdin 读事件 JSON → 分类（见
/// [`kimi_switch_core::auto_swap::classify_stop_failure`]）→ 命中额度耗尽时按耗尽路径
/// 语义复用既有自动切换判定与 activate；每个事件在审计日志留一行痕（见 [`passive_trace`]）。
/// 零新增网络查询；任何错误只写 stderr 日志，退出码由调用方恒置 0；stdin 畸形只留痕放行。
async fn on_quota_failure(ctx: &AppContext) {
    use std::io::Read;

    let mut raw = String::new();
    if let Err(err) = std::io::stdin().lock().read_to_string(&mut raw) {
        tracing::warn!(err = %err, "on-quota-failure: read stdin failed");
        return;
    }
    if let Err(err) = run_on_quota_failure_with(ctx, &raw, &settings::current().auto_swap).await {
        tracing::warn!(err = %err, "on-quota-failure handler failed");
    }
}

/// `on-quota-failure` 的可测内核：事件原文 → 被动切换编排 → 留痕。测试直接喂伪造 hook
/// 事件与显式 AutoSwap 设置（tempdir 双隔离），与 `apply_status_auto_swap` 同一
/// 测试形态。观察型约定：内部任何失败都不向上抛给退出码之外的主流程语义。
async fn run_on_quota_failure_with(
    ctx: &AppContext,
    raw: &str,
    auto_swap: &kimi_switch_core::settings::AutoSwap,
) -> Result<()> {
    use kimi_switch_core::auto_swap::{classify_stop_failure_verdict, StopFailureVerdict};

    // 1. 分类（命中条件见 classify_stop_failure）：未命中与畸形 stdin 不切换。
    let verdict = classify_stop_failure_verdict(raw);
    let decision = match verdict {
        StopFailureVerdict::Exhausted { window, .. } => passive_swap(ctx, window, auto_swap).await,
        StopFailureVerdict::Miss(_) => Ok(PassiveDecision::Stay("not_exhausted")),
    };
    // 每个事件恰好一行留痕，写在最终决定之后；写失败只 warn（AuditLog::append），不影响 hook。
    ctx.audit.append(passive_trace(raw, verdict, &decision));
    decision.map(drop)
}

// 测试 seam（ADR-0020）：被动路径决定之后、带前置条件切换之前运行一次性探针，供端到端用例
// 在这一刻插入一次手动切换（决定与交换锁之间的竞态窗口）。探针登记在调用线程上，用例须用
// 默认的 current-thread runtime。
#[cfg(test)]
thread_local! {
    static BEFORE_PASSIVE_ACTIVATE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn before_next_passive_activate(probe: impl FnOnce() + 'static) {
    BEFORE_PASSIVE_ACTIVATE.with(|slot| *slot.borrow_mut() = Some(Box::new(probe)));
}

#[cfg(test)]
fn run_passive_activate_probe() {
    if let Some(probe) = BEFORE_PASSIVE_ACTIVATE.with(|slot| slot.borrow_mut().take()) {
        probe();
    }
}

/// 被动触发的最终决定（留痕用）。
enum PassiveDecision {
    /// 不切换及原因：`not_exhausted` / `disabled` / `busy`（3 秒内没拿到自动切换串行化锁）/
    /// `no_active_account` / `superseded`（决定之后已有别的切换抢先：交换锁内的期望当前账号
    /// 比对不符，或锁内重读到冷却），或判定的跳过原因（[`skip_reason_json`]）。
    Stay(&'static str),
    /// 已切换。
    Swap { from: AccountId, to: AccountId },
    /// 判定要切，activate 失败（错误原文另记 auto_swap 审计）。
    SwapFailed { from: AccountId, to: AccountId },
}

/// 命中额度耗尽之后的被动切换编排，返回最终决定。
async fn passive_swap(
    ctx: &AppContext,
    window: Option<QuotaWindow>,
    auto_swap: &kimi_switch_core::settings::AutoSwap,
) -> Result<PassiveDecision> {
    use kimi_switch_core::auto_swap::{
        decide_auto_swap_passive_from_state, overlay_exhausted_snapshot, record_exhaustion,
    };

    // 2. 总开关沿用既有语义：自动切换关闭时被动路径同闸，切换侧零副作用。
    if !auto_swap.enabled {
        return Ok(PassiveDecision::Stay("disabled"));
    }

    let paths = AppPaths::resolve()?;

    // 3. 自动切换串行化锁（ADR-0020；锁顺序：本锁 → 交换锁）：GUI 快照编排、CLI status 自动
    //    切换与被动触发共用，把「记耗尽账本 → 读状态 → 判定 → activate → 写冷却」整段排成
    //    队列。后到者在锁内重读状态，看到冷却或新当前账号健康即自然落空（issue #162 锁 + 冷却
    //    收敛；冷却写在 activate 返回之后，没有这把锁时 [提交, 写冷却] 间隙里迟到的触发会把
    //    耗尽证据错按在新当前账号上再切一跳）。3 秒等不到视为另一自动触发处理中：观察型放弃，
    //    留痕 busy，耗尽账本也不记。
    let Some(_serialized) = AutoSwapLock::acquire_async(paths.auto_swap_lock_file()).await else {
        return Ok(PassiveDecision::Stay("busy"));
    };

    // 4. 以 live 为准一致性自检（ADR-0002 / issue #155），再取账号列表与缓存快照
    //    （零网络；无缓存账号无快照，候选资格线照旧不通过——判定只用已收集的额度快照）。
    //    观察型上下文：结果只记 debug；live 身份不在库时清除标记后无当前账号，
    //    下方 find(active) 自然落空返回。
    tracing::debug!(outcome = ?startup_check(ctx), "startup check (passive)");
    let accounts = ctx.list_ordered()?;
    let Some(current) = accounts.iter().find(|account| account.active) else {
        return Ok(PassiveDecision::Stay("no_active_account"));
    };
    let state_path = paths.auto_swap_state_file();
    let cache = QuotaCache::load(&paths.quota_cache_file());
    let mut snapshots: Vec<QuotaSnapshot> = accounts
        .iter()
        .filter_map(|account| cached_quota_snapshot(&cache, &account.id))
        .collect();
    // 缓存行不带档位（/usages 已移除 user 对象）：同样用 extra 的 /me 缓存补章，
    // 保住被动判定的档位排序与回驻资格。
    kimi_switch_core::apply_cached_membership(&mut snapshots, &accounts);

    // 5. 耗尽原文 = 当前账号耗尽的权威证据：先记进耗尽账本（判定之前，无论最终切没切；
    //    恢复前该账号不作任何自动路径的候选），再合成/覆盖当前账号快照后进既有判定。
    //    耗尽语义（冷却、合格线、防抖边际、摘出、回驻确认期、往返记忆、候选本地
    //    凭证快检、耗尽账本）全部由 core 判定单点继承；安顿宽限与手动保持期不阻挡。账本、
    //    overlay 与判定共用同一 `now`：过点重置按同一时刻划线，抵消不了覆写的耗尽窗口
    //    （ADR-0020）。
    //
    //    例外：上次自动切换距此刻不到安顿宽限时证据归属不明，不记账本（ADR-0020 自动切换
    //    串行化锁决策 9）。hook 事件不带账号身份，这时到达的耗尽多半是刚被切走的账号的会话
    //    在切换前发出的请求，记在新的当前账号头上会让它被切走后不作停泊候选。overlay 与判定
    //    照常，冷却仍让判定落空。只看上次自动切换：刚被手动选中的账号撞上耗尽照记，安顿宽限
    //    对被动路径的豁免不变（spec #152 决策 14）。
    let now = Utc::now();
    let last_auto_swap_at = read_last_auto_swap_at(&state_path);
    let cached_current = snapshots.iter().find(|item| item.account_id == current.id);
    let just_auto_swapped = last_auto_swap_at
        .is_some_and(|at| now < at + chrono::Duration::milliseconds(auto_swap.settle_grace_ms));
    if !just_auto_swapped {
        record_exhaustion(&state_path, &current.id, cached_current, window, now);
    }
    let overlay = overlay_exhausted_snapshot(&current.id, cached_current, window, now);
    match snapshots
        .iter_mut()
        .find(|item| item.account_id == current.id)
    {
        Some(existing) => *existing = overlay,
        None => snapshots.push(overlay),
    }

    let decision = decide_auto_swap_passive_from_state(
        &state_path,
        &accounts,
        &snapshots,
        auto_swap,
        now,
        last_auto_swap_at,
    );

    match decision {
        // 与轮询路径同口径：本应触发但无合格候选时记审计，便于事后核查。
        AutoSwapDecision::Stay {
            reason: AutoSwapSkipReason::NoEligibleCandidate,
        } => {
            ctx.audit.append(AuditEvent::err(
                "auto_swap",
                "kimi",
                Some(current.id.0.as_str()),
                "no eligible candidate",
            ));
            Ok(PassiveDecision::Stay(skip_reason_json(
                AutoSwapSkipReason::NoEligibleCandidate,
            )))
        }
        // 冷却等其余 Stay：会话侧静默落空（观察型，不打扰会话），只留痕。
        AutoSwapDecision::Stay { reason } => Ok(PassiveDecision::Stay(skip_reason_json(reason))),
        AutoSwapDecision::Swap(target) => {
            // 决定之后、真正 activate 之前在锁内重读冷却：别的自动切换刚写下冷却时放弃，不重复
            // activate、不审计。同版本的 GUI 与 CLI status 已在同一把锁里排队，这道重读留给不拿
            // 这把锁的旧版本进程（GUI 与 hook 所调 CLI 版本可能不一致）。当前账号不在这里重读：
            // 切换带着决定时的当前账号作前置条件，在交换锁内比对（期望当前账号，ADR-0020），
            // 决定之后已有别的切换抢先就返回「已被取代」，同样不审计、不写冷却。
            let cooldown_held = read_last_auto_swap_at(&state_path).is_some_and(|at| {
                Utc::now() < at + chrono::Duration::milliseconds(auto_swap.cooldown_ms)
            });
            if cooldown_held {
                return Ok(PassiveDecision::Stay("superseded"));
            }
            #[cfg(test)]
            run_passive_activate_probe();
            // activate 段计时（issue #153 续跑）：被动路径与轮询 / 手动切换同口径，
            // 耗时写入审计 duration_ms。
            let activate_started = std::time::Instant::now();
            match ctx.kimi.activate_if_current(&target.to, &target.from).await {
                Ok(ActivateOutcome::Superseded) => Ok(PassiveDecision::Stay("superseded")),
                Ok(ActivateOutcome::Activated) => {
                    record_auto_swap_success(&state_path, Utc::now(), &target, &snapshots);
                    ctx.audit.append(
                        AuditEvent::ok("auto_swap", "kimi", Some(target.to.0.as_str()))
                            .with_detail(format!(
                                "{} trigger=passive",
                                auto_swap_audit_detail(&target)
                            ))
                            .with_duration_ms(activate_started.elapsed().as_millis() as u64),
                    );
                    Ok(PassiveDecision::Swap {
                        from: target.from,
                        to: target.to,
                    })
                }
                Err(err) => {
                    // activate 失败不写冷却（与既有路径一致）；观察型不影响退出码。
                    ctx.audit.append(AuditEvent::err(
                        "auto_swap",
                        "kimi",
                        Some(target.to.0.as_str()),
                        &err.to_string(),
                    ));
                    Ok(PassiveDecision::SwapFailed {
                        from: target.from,
                        to: target.to,
                    })
                }
            }
        }
    }
}

/// 被动触发留痕（审计 action `on_quota_failure`，每个 StopFailure 恰好一行）：事件名、
/// error_type、分类判词、辨认出的窗口与最终决定，空格分隔的 key=value。事件只读这两个
/// 字段（见 [`trace_field`]），会话标题、工作目录与错误原文一概不写。
fn passive_trace(
    raw: &str,
    verdict: kimi_switch_core::auto_swap::StopFailureVerdict,
    decision: &Result<PassiveDecision>,
) -> AuditEvent {
    use kimi_switch_core::auto_swap::{ExhaustionRule, StopFailureMiss, StopFailureVerdict};

    let event = serde_json::from_str::<serde_json::Value>(raw).ok();
    let (signal, window) = match verdict {
        StopFailureVerdict::Exhausted { rule, window } => (
            match rule {
                ExhaustionRule::Official429 => "quota_429",
                ExhaustionRule::Legacy403 => "quota_403",
            },
            window
                .map(|window| format!(" window={}", quota_window_short(window)))
                .unwrap_or_default(),
        ),
        StopFailureVerdict::Miss(miss) => (
            match miss {
                StopFailureMiss::MalformedStdin => "malformed_stdin",
                StopFailureMiss::NoUsageLimit => "no_usage_limit",
                StopFailureMiss::NoStatusSignal => "no_status_signal",
                StopFailureMiss::NoOfficialWording => "no_official_wording",
            },
            String::new(),
        ),
    };
    let outcome = match decision {
        Ok(PassiveDecision::Stay(reason)) => format!("decision=stay reason={reason}"),
        Ok(PassiveDecision::Swap { from, to }) => format!("decision=swap from={from} to={to}"),
        Ok(PassiveDecision::SwapFailed { from, to }) => {
            format!("decision=swap_failed from={from} to={to}")
        }
        Err(_) => "decision=error".into(),
    };
    let detail = format!(
        "hook_event={} error_type={} signal={signal}{window} {outcome}",
        trace_field(event.as_ref(), "hook_event_name"),
        trace_field(event.as_ref(), "error_type"),
    );
    match decision {
        Ok(PassiveDecision::Stay(_) | PassiveDecision::Swap { .. }) => {
            AuditEvent::ok("on_quota_failure", "kimi", None).with_detail(detail)
        }
        Ok(PassiveDecision::SwapFailed { .. }) | Err(_) => {
            AuditEvent::err("on_quota_failure", "kimi", None, &detail)
        }
    }
}

/// 留痕里事件字段的写法：只留 `[A-Za-z0-9_.:-]`（其余字符换成 `_`），至多 64 字符，
/// detail 因此仍是空格分隔的 key=value；字段缺失、不是字符串或 stdin 畸形记 `-`。
fn trace_field(event: Option<&serde_json::Value>, key: &str) -> String {
    let token: String = event
        .and_then(|value| value.get(key))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.:-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if token.is_empty() {
        "-".into()
    } else {
        token
    }
}

/// 只读额度缓存构造快照（被动路径零网络）。档位取窗口一字段（与 core
/// `membership_of` 同口径）。无缓存账号返回 `None`（无快照 → 候选资格线不通过）。
/// `reset_at` 已过的窗口原样保留，交给判定按过点重置处理（ADR-0020）：GUI 久未开时，
/// 窗口全部过点的账号不会因缓存层丢窗口而整条失去快照。
fn cached_quota_snapshot(cache: &QuotaCache, id: &AccountId) -> Option<QuotaSnapshot> {
    let entry = cache.get_keeping_past_resets("kimi", &id.0)?;
    let membership = entry.quotas.iter().find_map(|q| q.membership.clone());
    Some(QuotaSnapshot {
        account_id: id.clone(),
        fetched_at: entry.cached_at,
        stale: false,
        membership,
        state: QuotaState::Cached(entry.quotas),
    })
}

// ---------------------------------------------------------------------------
// on-session-start（跟随启动拉起器，issue #197 / ADR-0021）
// ---------------------------------------------------------------------------

/// 跟随启动拉起的 GUI 程序，取 CLI 同目录（ADR-0021 决策 9）。
const GUI_PROGRAM: &str = "kimi-switch.exe";

/// GUI 的后台启动参数（#196）：首次启动最小化到任务栏、不抢焦点；GUI 已在运行时不动已有窗口。
const GUI_BACKGROUND_ARG: &str = "--background";

/// 拉起计划：拉起器交给执行器的全部参数，也是拉起器内核的可观察结果。
#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchPlan {
    program: PathBuf,
    args: Vec<&'static str>,
    /// 程序所在目录。hook 在会话的项目目录里执行，GUI 继承它就会一直占住那个目录。
    working_dir: PathBuf,
    /// 脱离 hook 的进程树：不挂控制台、自成进程组，并尽力脱离 hook 所在的 Job，
    /// 不随 hook 进程或 Kimi Code 退出而结束。
    detach: bool,
    /// 是否继承本进程的句柄。拉起器恒为 false：GUI 一旦持有 hook 执行方交给本进程的
    /// stdout / stderr 管道，执行方就会一直等管道关闭，直到 hook 超时。
    inherit_handles: bool,
}

/// 拉起器所见的启动环境。入口装配真实探针与执行器，用例注入假的。
trait LaunchEnv {
    /// 本平台是否支持跟随启动：只有 Windows（ADR-0021 决策 6）。
    fn platform_supported(&self) -> bool;
    /// 当前进程是否在桌面交互会话里。SSH、服务等非交互会话为 false。
    fn interactive_session(&self) -> bool;
    /// 本 Windows 会话里是否已有本工具的 GUI 进程。
    fn gui_running(&self) -> bool;
    /// 本 CLI 可执行文件的路径，GUI 取它的同目录。
    fn cli_exe(&self) -> std::io::Result<PathBuf>;
    /// 按计划创建 GUI 进程。创建成功即返回，不等 GUI。
    fn launch(&self, plan: &LaunchPlan) -> std::io::Result<()>;
}

/// 拉起器对一个 SessionStart 事件的决定，即留痕里 `decision` 的取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStartDecision {
    /// 自动切换总开关关闭（ADR-0021 决策 3）。
    Disabled,
    /// 非 Windows 平台（ADR-0021 决策 6）。
    UnsupportedPlatform,
    /// 当前进程不在桌面交互会话里（SSH、服务等）。
    NonInteractive,
    /// 本 Windows 会话里已有 GUI 进程。
    AlreadyRunning,
    /// 已按拉起计划创建 GUI 进程。
    Launched,
    /// GUI 程序缺失、解析不出 CLI 路径或创建进程失败；错误原文只进 stderr 日志。
    LaunchFailed,
}

impl SessionStartDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::NonInteractive => "non_interactive",
            Self::AlreadyRunning => "already_running",
            Self::Launched => "launched",
            Self::LaunchFailed => "launch_failed",
        }
    }
}

/// 入口装配的真实启动环境：探针与执行器见 [`follow_launch_sys`]。
struct SystemLaunchEnv;

impl LaunchEnv for SystemLaunchEnv {
    fn platform_supported(&self) -> bool {
        cfg!(windows)
    }

    fn interactive_session(&self) -> bool {
        follow_launch_sys::interactive_session()
    }

    fn gui_running(&self) -> bool {
        follow_launch_sys::gui_running(GUI_PROGRAM)
    }

    fn cli_exe(&self) -> std::io::Result<PathBuf> {
        std::env::current_exe()
    }

    fn launch(&self, plan: &LaunchPlan) -> std::io::Result<()> {
        let outside_job = follow_launch_sys::launch(plan)?;
        if plan.detach && !outside_job {
            tracing::warn!(
                "on-session-start: the hook's job forbids breakaway; the GUI may exit with Kimi Code"
            );
        }
        Ok(())
    }
}

/// 跟随启动的系统探针与执行器，Windows 实现见 `follow_launch_windows.rs`。
#[cfg(windows)]
#[path = "follow_launch_windows.rs"]
mod follow_launch_sys;

/// 非 Windows 平台在平台检查处就不拉起（ADR-0021 决策 6），这些桩不会被调用。
#[cfg(not(windows))]
mod follow_launch_sys {
    pub(crate) fn interactive_session() -> bool {
        false
    }

    pub(crate) fn gui_running(_image: &str) -> bool {
        false
    }

    pub(crate) fn launch(_plan: &super::LaunchPlan) -> std::io::Result<bool> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

/// Kimi Code `SessionStart` hook 观察型入口：stdin 读事件 JSON（只用于留痕），按判定顺序决定
/// 是否拉起 GUI，每个事件在审计日志留一行痕（见 [`session_start_trace`]）。任何错误只写 stderr
/// 日志，退出码由调用方恒置 0；stdin 读失败或不是 UTF-8 时照常判定，字段记 `-`。
fn on_session_start() {
    use std::io::Read;

    let mut bytes = Vec::new();
    if let Err(err) = std::io::stdin().lock().read_to_end(&mut bytes) {
        tracing::warn!(err = %err, "on-session-start: read stdin failed");
    }
    let raw = String::from_utf8_lossy(&bytes);
    let audit = match AuditLog::from_default_paths() {
        Ok(audit) => audit,
        Err(err) => {
            // 数据目录解析失败时设置同样只能取默认值（总开关关闭），本就不会拉起，留痕也无处可写。
            tracing::warn!(err = %err, "on-session-start: audit log unavailable");
            return;
        }
    };
    run_on_session_start_with(
        &audit,
        &raw,
        &settings::current().auto_swap,
        &SystemLaunchEnv,
    );
}

/// `on-session-start` 的可测内核：判定是否拉起 GUI，拉起时把计划交给执行器，最后在审计日志
/// 留一行痕。用例注入启动环境与 tempdir 里的审计日志。只读设置、只写审计：不碰账号库、凭证、
/// 额度缓存与自动切换状态，不发网络请求。
fn run_on_session_start_with(
    audit: &AuditLog,
    raw: &str,
    auto_swap: &kimi_switch_core::settings::AutoSwap,
    env: &dyn LaunchEnv,
) {
    let decision = decide_session_start(auto_swap, env);
    // 每个事件恰好一行留痕，写在最终决定之后；写失败只 warn（AuditLog::append），不影响 hook。
    audit.append(session_start_trace(raw, decision));
}

/// 判定顺序（issue #197）：总开关 → 平台 → 交互会话 → 本会话已有 GUI → 拉起。总开关关闭时
/// 不调用任何探针。
fn decide_session_start(
    auto_swap: &kimi_switch_core::settings::AutoSwap,
    env: &dyn LaunchEnv,
) -> SessionStartDecision {
    if !auto_swap.enabled {
        return SessionStartDecision::Disabled;
    }
    if !env.platform_supported() {
        return SessionStartDecision::UnsupportedPlatform;
    }
    if !env.interactive_session() {
        return SessionStartDecision::NonInteractive;
    }
    if env.gui_running() {
        return SessionStartDecision::AlreadyRunning;
    }
    let cli_exe = match env.cli_exe() {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!(err = %err, "on-session-start: resolve CLI path failed");
            return SessionStartDecision::LaunchFailed;
        }
    };
    let Some(plan) = gui_launch_plan(&cli_exe) else {
        tracing::warn!(cli = %cli_exe.display(), "on-session-start: CLI path has no parent directory");
        return SessionStartDecision::LaunchFailed;
    };
    // 被 `cargo clean` 删掉或正在重建时 GUI 不在：只记一行拉起失败，不调用执行器。
    if !plan.program.is_file() {
        tracing::warn!(gui = %plan.program.display(), "on-session-start: GUI program not found");
        return SessionStartDecision::LaunchFailed;
    }
    match env.launch(&plan) {
        Ok(()) => SessionStartDecision::Launched,
        Err(err) => {
            tracing::warn!(err = %err, gui = %plan.program.display(), "on-session-start: launch GUI failed");
            SessionStartDecision::LaunchFailed
        }
    }
}

/// 拉起计划：CLI 同目录的 GUI，带后台启动参数，工作目录为程序目录，脱离 hook 进程树，
/// 不继承任何句柄。
fn gui_launch_plan(cli_exe: &Path) -> Option<LaunchPlan> {
    let dir = cli_exe.parent()?;
    Some(LaunchPlan {
        program: dir.join(GUI_PROGRAM),
        args: vec![GUI_BACKGROUND_ARG],
        working_dir: dir.to_path_buf(),
        detach: true,
        inherit_handles: false,
    })
}

/// 跟随启动留痕（审计 action `on_session_start`，每个 SessionStart 恰好一行）：事件名、来源、
/// 客户端类型与决定，空格分隔的 key=value。事件字段的清洗与被动留痕同口径（见 [`trace_field`]），
/// 会话标题、工作目录与 stdin 原文一概不写。
fn session_start_trace(raw: &str, decision: SessionStartDecision) -> AuditEvent {
    let event = serde_json::from_str::<serde_json::Value>(raw).ok();
    let detail = format!(
        "hook_event={} source={} client_type={} decision={}",
        trace_field(event.as_ref(), "hook_event_name"),
        trace_field(event.as_ref(), "source"),
        trace_field(event.as_ref(), "client_type"),
        decision.as_str(),
    );
    match decision {
        SessionStartDecision::LaunchFailed => {
            AuditEvent::err("on_session_start", "kimi", None, &detail)
        }
        _ => AuditEvent::ok("on_session_start", "kimi", None).with_detail(detail),
    }
}

// ---------------------------------------------------------------------------
// install-hook（issue #162 / spec #152 决策 16）
// ---------------------------------------------------------------------------

/// 我们写入 Kimi Code config.toml 的 StopFailure 条目识别标记：`command` 含
/// `on-quota-failure` 子命令即视为本工具条目。重复安装只刷新（不重复追加），
/// 卸载只删自己的，不动用户其它 hooks 条目。
const HOOK_MARKER: &str = "on-quota-failure";

/// Kimi Code 工作目录（只读镜像 `crates/providers/kimi/src/paths.rs::kimi_home`，
/// owner 在那边；布局调整时需同步）。`KIMI_CODE_HOME` > `~/.kimi-code` > `.kimi-code`。
fn kimi_code_home() -> PathBuf {
    if let Some(value) = std::env::var_os("KIMI_CODE_HOME") {
        return PathBuf::from(value);
    }
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().join(".kimi-code"))
        .unwrap_or_else(|| PathBuf::from(".kimi-code"))
}

/// `install-hook` 命令体：幂等写入 / 卸载 Kimi Code `config.toml` 的 `[[hooks]]`
/// StopFailure 条目，command 指向当前可执行文件的 `on-quota-failure` 子命令。
fn install_hook(uninstall: bool) -> Result<()> {
    let config_path = kimi_code_home().join("config.toml");
    let exe = std::env::current_exe().context("resolve current executable path")?;
    let message = install_hook_into(&config_path, &exe, uninstall)?;
    println!("{message}");
    Ok(())
}

/// 安装 / 卸载的可测内核（tempdir 假 config）：先 toml_edit 解析后写回，保留注释、
/// 顺序与既有条目；解析失败报错退出、绝不覆写。无变化时不重写文件（字节不变）。
fn install_hook_into(config_path: &Path, exe: &Path, uninstall: bool) -> Result<String> {
    let raw = std::fs::read_to_string(config_path).unwrap_or_default();
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parse {}", config_path.display()))?;

    if doc.get("hooks").is_none() {
        doc["hooks"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let Some(hooks) = doc["hooks"].as_array_of_tables_mut() else {
        anyhow::bail!(
            "`hooks` in {} is not an array of tables ([[hooks]]); refusing to touch it",
            config_path.display()
        );
    };
    let ours: Vec<usize> = hooks
        .iter()
        .enumerate()
        .filter(|(_, table)| hook_entry_is_ours(table))
        .map(|(idx, _)| idx)
        .collect();

    if uninstall {
        if ours.is_empty() {
            return Ok("no kimi-switch StopFailure hook installed; nothing to remove".into());
        }
        for idx in ours.into_iter().rev() {
            hooks.remove(idx);
        }
        // 数组被清空时连 `hooks` 键一起移除，还原到没有 hooks 段的原始形态。
        if hooks.is_empty() {
            doc.remove("hooks");
        }
    } else {
        let command = format!("\"{}\" {HOOK_MARKER}", exe.display());
        match ours.first() {
            // 已安装：刷新 command（可执行文件可能移动过）与 timeout，多余重复条目收敛成一个。
            Some(&first) => {
                let table = hooks
                    .get_mut(first)
                    .expect("ours 索引来自同一 hooks 数组，必然存在");
                table.insert("command", toml_edit::value(command));
                table.insert("timeout", toml_edit::value(HOOK_TIMEOUT_SECS));
                for idx in ours.into_iter().skip(1).rev() {
                    hooks.remove(idx);
                }
            }
            None => {
                let mut table = toml_edit::Table::new();
                table.insert("event", toml_edit::value("StopFailure"));
                table.insert("command", toml_edit::value(command));
                table.insert("timeout", toml_edit::value(HOOK_TIMEOUT_SECS));
                hooks.push(table);
            }
        }
    }

    let body = doc.to_string();
    if body == raw {
        return Ok(format!(
            "kimi-switch StopFailure hook already up to date in {}",
            config_path.display()
        ));
    }
    write_config_atomically(config_path, &body)?;
    Ok(if uninstall {
        format!(
            "removed kimi-switch StopFailure hook from {}",
            config_path.display()
        )
    } else {
        format!(
            "installed kimi-switch StopFailure hook in {} (command runs {})",
            config_path.display(),
            HOOK_MARKER
        )
    })
}

/// hook 条目超时（秒）：被动切换为纯本地文件操作（毫秒级），10s 上限兜底即可，
/// 不让 hook 拖慢会话（官方默认 30s）。
const HOOK_TIMEOUT_SECS: i64 = 10;

/// 判定 [[hooks]] 条目是否为本工具写入的 StopFailure 条目。
fn hook_entry_is_ours(table: &toml_edit::Table) -> bool {
    table.get("event").and_then(|value| value.as_str()) == Some("StopFailure")
        && table
            .get("command")
            .and_then(|value| value.as_str())
            .is_some_and(|command| command.contains(HOOK_MARKER))
}

/// 同目录临时文件 + rename 写回（config.toml 非机密，默认权限即可）。
fn write_config_atomically(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.kimi-switch.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// `config` 输出必须含主动续期段与开关键（issue #42 设置壳的 CLI 展示面）。
    #[test]
    fn config_output_contains_proactive_renewal_section() {
        let text = super::render_config().unwrap();
        assert!(text.contains("proactive_renewal"), "{text}");
        assert!(text.contains("enabled"), "{text}");
    }

    /// 会员档位段（issue #49）：extra 有到期瞬间 → 档位名 + `有效期至 <实抓日期>`；
    /// 档位在但无到期 → 绑定弱提示（不是错误）；档位缺失 → None 保持现状。
    #[test]
    fn membership_status_text_binds_expiry_or_hint() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "membership_current_end_time".into(),
            serde_json::Value::String("2027-08-28T00:00:00Z".into()),
        );
        extra.insert(
            "membership_next_billing_time".into(),
            serde_json::Value::String("2027-08-27T01:25:37.930341Z".into()),
        );
        assert_eq!(
            super::membership_status_text(Some("LEVEL_INTERMEDIATE"), &extra).as_deref(),
            Some("Allegretto 有效期至 2027-08-27")
        );

        // 档位在但没绑定网页登录：弱提示。
        assert_eq!(
            super::membership_status_text(Some("LEVEL_ADVANCED"), &serde_json::Map::new())
                .as_deref(),
            Some("Allegro bind web login to show expiry")
        );

        // 档位缺失（额度查询失败）：保持现状。
        assert_eq!(super::membership_status_text(None, &extra), None);
    }

    /// bind-web（issue #50）：**没有 `--token` 旗标**（命令行参数会进 shell
    /// 历史与进程列表）；token 只从 stdin 读；位置参数仍是账号引用。
    #[test]
    fn bind_web_has_no_token_flag() {
        use super::{Cli, Cmd};
        use clap::Parser as _;

        let ok = Cli::try_parse_from(["kimi-switch", "bind-web", "1"]);
        assert!(matches!(
            ok.unwrap().cmd,
            Some(Cmd::BindWeb { ref id }) if id == "1"
        ));
        assert!(
            Cli::try_parse_from(["kimi-switch", "bind-web", "1", "--token", "x"]).is_err(),
            "--token 旗标不得存在"
        );
        assert!(
            Cli::try_parse_from(["kimi-switch", "bind-web", "1", "--token=x"]).is_err(),
            "--token=... 形式同样不得存在"
        );
    }

    /// issue #92：`login` 的 clap 帮助须同时写清 `kimi` 与公司名别名 `moonshot`。
    #[test]
    fn login_help_documents_kimi_and_moonshot_alias() {
        use clap::CommandFactory as _;

        let help = super::Cli::command()
            .find_subcommand("login")
            .expect("login")
            .clone()
            .render_long_help()
            .to_string();
        assert!(
            help.contains("`kimi` is supported; `moonshot` is an alias"),
            "{help}"
        );
        assert!(
            !help.contains("only `kimi`"),
            "不得再写成只支持 kimi：{help}"
        );
    }

    /// issue #92：未知 provider 错误与 clap 帮助同一套说法。
    #[tokio::test]
    async fn login_unknown_provider_mentions_kimi_and_moonshot_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = cli_ctx(&tmp);
        let err = super::login(&ctx, "openai").await.unwrap_err().to_string();
        assert!(
            err.contains("`kimi` is supported; `moonshot` is an alias"),
            "{err}"
        );
        assert!(!err.contains("only `kimi`"), "不得再写成只支持 kimi：{err}");
    }

    fn cli_ctx(tmp: &tempfile::TempDir) -> super::AppContext {
        use kimi_switch_core::{AccountRegistry, CredentialStore, FileStore};
        use std::sync::Arc;

        let store: Arc<dyn CredentialStore> =
            Arc::new(FileStore::new(tmp.path().join("creds.json")));
        let registry = Arc::new(AccountRegistry::new(tmp.path().join("registry.toml")));
        let kimi = Arc::new(kimi_switch_kimi::new(store.clone(), registry.clone()));
        let audit = super::AuditLog::new(tmp.path().join("audit.log"));
        super::AppContext {
            store,
            registry,
            kimi,
            audit,
        }
    }

    /// issue #92：`0` 非法（编号从 1 起），即使存在 label/id 为 `"0"` 的账号也不走别名。
    #[test]
    fn resolve_account_rejects_index_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = cli_ctx(&tmp);
        let mut labeled_zero = acc("alice", true);
        labeled_zero.label = "0".into();
        ctx.registry.upsert(labeled_zero).unwrap();

        let err = super::resolve_account(&ctx, "0").unwrap_err().to_string();
        assert!(err.contains("index 0"), "{err}");
        assert!(err.contains("starts at 1"), "{err}");
    }

    /// issue #92：越界编号报错，不回退到 id/label。
    #[test]
    fn resolve_account_rejects_out_of_range_index() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = cli_ctx(&tmp);
        ctx.registry.upsert(acc("alice", true)).unwrap();
        ctx.registry.upsert(acc("bob", false)).unwrap();

        let err = super::resolve_account(&ctx, "3").unwrap_err().to_string();
        assert!(err.contains("index 3"), "{err}");
    }

    /// issue #92：纯数字优先按 1 起编号；与别名冲突时编号命中则不走别名。
    #[test]
    fn resolve_account_numeric_alias_still_uses_1_based_index() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = cli_ctx(&tmp);
        let mut first = acc("alice", true);
        first.label = "2".into();
        ctx.registry.upsert(first).unwrap();
        ctx.registry.upsert(acc("bob", false)).unwrap();

        let got = super::resolve_account(&ctx, "2").unwrap();
        assert_eq!(
            got.id.0, "bob",
            "输入 2 必须命中编号第 2 个，而不是 label 为 2 的第 1 个"
        );
    }

    /// stdin 读到的 token：去首尾空白（管道输入带换行）；空输入失败。
    #[test]
    fn normalize_web_token_trims_and_rejects_empty() {
        assert_eq!(
            super::normalize_web_token("  header.payload.sig\n".into()).unwrap(),
            "header.payload.sig"
        );
        assert_eq!(
            super::normalize_web_token("header.payload.sig".into()).unwrap(),
            "header.payload.sig"
        );
        for empty in ["", "\n", "   \t"] {
            assert!(
                super::normalize_web_token(empty.into()).is_err(),
                "空输入必须失败：{empty:?}"
            );
        }
    }

    /// 闭环（issue #50）：绑定管道写侧（`write_membership_extra`）产出的 extra
    /// → 默认列表行出现「有效期至」。写入与渲染隔着两层 crate，这里用与
    /// 管道测试同一实抓样例钉住两端契约衔接。
    #[test]
    fn membership_status_text_renders_extra_written_by_bind_pipeline() {
        let current_end = "2027-08-28T00:00:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        let next_billing = "2027-08-27T01:25:37.930341Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        let mut extra = serde_json::Map::new();
        kimi_switch_core::write_membership_extra(
            &mut extra,
            None,
            Some(current_end),
            Some(next_billing),
        );
        assert_eq!(
            super::membership_status_text(Some("LEVEL_ADVANCED"), &extra).as_deref(),
            Some("Allegro 有效期至 2027-08-27")
        );
    }

    use super::{
        Account, AccountId, ActivateOutcome, AuditEvent, AuditLog, Quota, QuotaSnapshot,
        QuotaState, QuotaWindow,
    };
    use chrono::{DateTime, Utc};

    fn now() -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 9, 3, 12, 0, 0).unwrap()
    }

    fn auto_swap_on() -> kimi_switch_core::settings::AutoSwap {
        kimi_switch_core::settings::AutoSwap {
            enabled: true,
            ..kimi_switch_core::settings::AutoSwap::default()
        }
    }

    fn acc(id: &str, active: bool) -> Account {
        Account {
            provider: "kimi".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: now(),
            last_used_at: Some(now() - chrono::Duration::seconds(120)),
            extra: Default::default(),
        }
    }

    fn ready(id: &str, used: u64, limit: u64) -> QuotaSnapshot {
        ready_reset(id, used, limit, None)
    }

    fn ready_reset(
        id: &str,
        used: u64,
        limit: u64,
        reset_at: Option<DateTime<Utc>>,
    ) -> QuotaSnapshot {
        QuotaSnapshot {
            account_id: AccountId(id.into()),
            fetched_at: now(),
            stale: false,
            membership: None,
            state: QuotaState::Ready(vec![Quota {
                provider: "kimi".into(),
                account_id: AccountId(id.into()),
                window: QuotaWindow::FiveHour,
                used,
                limit,
                reset_at,
                status: kimi_switch_core::QuotaStatus::Ok,
                note: None,
                membership: None,
            }]),
        }
    }

    fn last_audit(path: &std::path::Path) -> AuditEvent {
        let raw = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(raw.trim()).unwrap()
    }

    /// 自动切换串行化锁的替身：立即拿到锁。
    fn unlocked() -> std::future::Ready<Option<()>> {
        std::future::ready(Some(()))
    }

    /// 开关关闭：不 activate、不写审计、不碰冷却文件。
    #[tokio::test]
    async fn status_auto_swap_disabled_has_no_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let mut settings = auto_swap_on();
        settings.enabled = false;
        let effect = super::apply_status_auto_swap(
            &settings,
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 100, 100), ready("bob", 10, 100)],
            now(),
            unlocked,
            |_to, _expected| async { panic!("disabled 不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert!(effect.line.is_none());
        assert!(!effect.swapped);
        assert!(!state.exists());
        assert!(!audit.path().exists());
    }

    /// 触发成功：走 activate、写审计、持久化冷却、输出成功行。
    #[tokio::test]
    async fn status_auto_swap_success_activates_audits_and_persists() {
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let activated = Arc::new(Mutex::new(None));
        let activated_cb = activated.clone();
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 99, 100), ready("bob", 10, 100)],
            now(),
            unlocked,
            move |id, _expected| {
                let activated = activated_cb.clone();
                async move {
                    *activated.lock().unwrap() = Some(id);
                    Ok(ActivateOutcome::Activated)
                }
            },
            &audit,
            &state,
        )
        .await;
        assert_eq!(
            effect.line.as_deref(),
            Some("auto_swap kimi/alice → kimi/bob (5h 99%)")
        );
        assert!(effect.swapped);
        assert_eq!(
            activated.lock().unwrap().as_ref().map(|id| id.0.as_str()),
            Some("bob")
        );
        assert!(kimi_switch_core::read_last_auto_swap_at(&state).is_some());
        let event = last_audit(audit.path());
        assert_eq!(event.action, "auto_swap");
        assert_eq!(event.provider, "kimi");
        assert_eq!(event.result, "ok");
        assert_eq!(event.account.as_deref(), Some("bob"));
        // issue #153 续跑：自动切换成功审计同样携带 activate 段耗时。
        assert!(
            event.duration_ms.is_some(),
            "自动切换成功审计应含 duration_ms（activate 段耗时）"
        );
        let detail = event.detail.expect("success audit has detail");
        assert!(detail.contains("from=alice"), "{detail}");
        assert!(detail.contains("window=5h"), "{detail}");
        assert!(detail.contains("used=99%"), "{detail}");
        assert!(
            !detail.contains("kind="),
            "耗尽路径保持无 kind= 的既有 detail: {detail}"
        );
    }

    /// issue #114：pace 预切换成功 → 审计 kind=pace，人类可读行带 pace，退出码 0。
    #[tokio::test]
    async fn status_auto_swap_pace_activates_audits_and_json() {
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let mut settings = auto_swap_on();
        settings.pace_pre_swap = true;
        let half = now() + chrono::Duration::minutes(150);
        let activated = Arc::new(Mutex::new(None));
        let activated_cb = activated.clone();
        let effect = super::apply_status_auto_swap(
            &settings,
            &[acc("alice", true), acc("bob", false)],
            &[
                ready_reset("alice", 85, 100, Some(half)),
                ready("bob", 10, 100),
            ],
            now(),
            unlocked,
            move |id, _expected| {
                let activated = activated_cb.clone();
                async move {
                    *activated.lock().unwrap() = Some(id);
                    Ok(ActivateOutcome::Activated)
                }
            },
            &audit,
            &state,
        )
        .await;
        assert_eq!(
            effect.line.as_deref(),
            Some("auto_swap kimi/alice → kimi/bob (pace 5h 85%)")
        );
        assert!(effect.swapped);
        assert_eq!(effect.exit_code(), 0);
        assert_eq!(
            activated.lock().unwrap().as_ref().map(|id| id.0.as_str()),
            Some("bob")
        );
        assert!(kimi_switch_core::read_last_auto_swap_at(&state).is_some());
        let event = last_audit(audit.path());
        assert_eq!(event.action, "auto_swap");
        assert_eq!(event.result, "ok");
        assert_eq!(event.account.as_deref(), Some("bob"));
        let detail = event.detail.expect("pace audit has detail");
        assert!(detail.contains("kind=pace"), "{detail}");
        assert!(detail.contains("from=alice"), "{detail}");
        assert!(detail.contains("window=5h"), "{detail}");
        assert!(detail.contains("used=85%"), "{detail}");
        let json = parse_status_event(&effect);
        assert_eq!(json["schemaVersion"], 1);
        assert_eq!(json["type"], "swap");
        assert_eq!(json["from"], "alice");
        assert_eq!(json["to"], "bob");
        assert_eq!(json["kind"], "pace");
        assert_eq!(json["window"], "5h");
        assert_eq!(json["usagePct"], 85);
    }

    /// issue #147：重置前抢用成功 → 审计 kind=burn，人类可读行带 burn，退出码 0。
    #[tokio::test]
    async fn status_auto_swap_burn_activates_audits_and_json() {
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let mut settings = auto_swap_on();
        settings.pre_reset_burn = true;
        let near = now() + chrono::Duration::hours(1);
        let activated = Arc::new(Mutex::new(None));
        let activated_cb = activated.clone();
        let effect = super::apply_status_auto_swap(
            &settings,
            &[acc("alice", true), acc("bob", false)],
            &[
                ready("alice", 10, 100),
                ready_reset("bob", 80, 100, Some(near)),
            ],
            now(),
            unlocked,
            move |id, _expected| {
                let activated = activated_cb.clone();
                async move {
                    *activated.lock().unwrap() = Some(id);
                    Ok(ActivateOutcome::Activated)
                }
            },
            &audit,
            &state,
        )
        .await;
        assert_eq!(
            effect.line.as_deref(),
            Some("auto_swap kimi/alice → kimi/bob (burn 5h 80%)")
        );
        assert!(effect.swapped);
        assert_eq!(effect.exit_code(), 0);
        assert_eq!(
            activated.lock().unwrap().as_ref().map(|id| id.0.as_str()),
            Some("bob")
        );
        assert!(kimi_switch_core::read_last_auto_swap_at(&state).is_some());
        let event = last_audit(audit.path());
        assert_eq!(event.action, "auto_swap");
        assert_eq!(event.result, "ok");
        assert_eq!(event.account.as_deref(), Some("bob"));
        let detail = event.detail.expect("burn audit has detail");
        assert!(detail.contains("kind=burn"), "{detail}");
        assert!(detail.contains("from=alice"), "{detail}");
        assert!(detail.contains("window=5h"), "{detail}");
        assert!(detail.contains("used=80%"), "{detail}");
        let json = parse_status_event(&effect);
        assert_eq!(json["schemaVersion"], 1);
        assert_eq!(json["type"], "swap");
        assert_eq!(json["from"], "alice");
        assert_eq!(json["to"], "bob");
        assert_eq!(json["kind"], "burn");
        assert_eq!(json["window"], "5h");
        assert_eq!(json["usagePct"], 80);
        let trip = kimi_switch_core::read_burn_trip(&state).expect("抢用成功应写入往返记忆");
        assert_eq!(trip.origin.0, "alice");
        assert_eq!(trip.target.0, "bob");
        assert_eq!(trip.window, QuotaWindow::FiveHour);
        assert_eq!(trip.reset_at, near);
    }

    /// issue #148：reset_at 翻过 → 回程原账号，审计 kind=burn_return，JSON kind burn_return。
    #[tokio::test]
    async fn status_auto_swap_burn_return_on_reset_flip_activates_audits_and_json() {
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let recorded = now() + chrono::Duration::hours(1);
        let flipped = recorded + chrono::Duration::hours(5);
        kimi_switch_core::write_burn_trip_and_last_auto_swap_at(
            &state,
            now() - chrono::Duration::hours(1),
            &kimi_switch_core::BurnTrip {
                origin: AccountId("alice".into()),
                target: AccountId("bob".into()),
                window: QuotaWindow::FiveHour,
                reset_at: recorded,
            },
        );
        let activated = Arc::new(Mutex::new(None));
        let activated_cb = activated.clone();
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", false), acc("bob", true)],
            &[
                ready("alice", 10, 100),
                ready_reset("bob", 10, 100, Some(flipped)),
            ],
            now(),
            unlocked,
            move |id, _expected| {
                let activated = activated_cb.clone();
                async move {
                    *activated.lock().unwrap() = Some(id);
                    Ok(ActivateOutcome::Activated)
                }
            },
            &audit,
            &state,
        )
        .await;
        assert_eq!(
            effect.line.as_deref(),
            Some("auto_swap kimi/bob → kimi/alice (burn_return 5h 10%)")
        );
        assert!(effect.swapped);
        assert_eq!(effect.exit_code(), 0);
        assert_eq!(
            activated.lock().unwrap().as_ref().map(|id| id.0.as_str()),
            Some("alice")
        );
        assert!(kimi_switch_core::read_burn_trip(&state).is_none());
        let event = last_audit(audit.path());
        assert_eq!(event.action, "auto_swap");
        assert_eq!(event.result, "ok");
        assert_eq!(event.account.as_deref(), Some("alice"));
        let detail = event.detail.expect("burn_return audit has detail");
        assert!(detail.contains("kind=burn_return"), "{detail}");
        assert!(detail.contains("from=bob"), "{detail}");
        assert!(
            !detail.contains("window="),
            "回程审计不强制 window: {detail}"
        );
        let json = parse_status_event(&effect);
        assert_eq!(json["schemaVersion"], 1);
        assert_eq!(json["type"], "swap");
        assert_eq!(json["from"], "bob");
        assert_eq!(json["to"], "alice");
        assert_eq!(json["kind"], "burn_return");
    }

    /// issue #148：目标耗尽回程锁定原账号，不重新排序。
    #[tokio::test]
    async fn status_auto_swap_burn_return_on_exhaust_locks_origin() {
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let recorded = now() + chrono::Duration::hours(1);
        kimi_switch_core::write_burn_trip_and_last_auto_swap_at(
            &state,
            now() - chrono::Duration::hours(1),
            &kimi_switch_core::BurnTrip {
                origin: AccountId("alice".into()),
                target: AccountId("bob".into()),
                window: QuotaWindow::FiveHour,
                reset_at: recorded,
            },
        );
        let activated = Arc::new(Mutex::new(None));
        let activated_cb = activated.clone();
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", false), acc("bob", true), acc("carol", false)],
            &[
                ready("alice", 10, 100),
                ready("bob", 99, 100),
                ready("carol", 1, 100),
            ],
            now(),
            unlocked,
            move |id, _expected| {
                let activated = activated_cb.clone();
                async move {
                    *activated.lock().unwrap() = Some(id);
                    Ok(ActivateOutcome::Activated)
                }
            },
            &audit,
            &state,
        )
        .await;
        assert!(effect.swapped);
        assert_eq!(effect.exit_code(), 0);
        assert_eq!(
            activated.lock().unwrap().as_ref().map(|id| id.0.as_str()),
            Some("alice"),
            "耗尽回程须锁定原账号，不得改选 carol"
        );
        let json = parse_status_event(&effect);
        assert_eq!(json["kind"], "burn_return");
        assert_eq!(json["to"], "alice");
        assert!(kimi_switch_core::read_burn_trip(&state).is_none());
        let detail = last_audit(audit.path()).detail.expect("detail");
        assert!(detail.contains("kind=burn_return"), "{detail}");
    }

    /// issue #69 故事 12：CLI 编排把整份 AutoSwap 交给决策，prefer_lower_tier 生效。
    #[tokio::test]
    async fn status_auto_swap_prefer_lower_tier_picks_lower_membership() {
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let mut settings = auto_swap_on();
        settings.prefer_lower_tier = true;
        let mut bob = ready("bob", 50, 100);
        bob.membership = Some("LEVEL_INTERMEDIATE".into());
        let mut carol = ready("carol", 10, 100);
        carol.membership = Some("LEVEL_ADVANCED".into());
        let activated = Arc::new(Mutex::new(None));
        let activated_cb = activated.clone();
        let effect = super::apply_status_auto_swap(
            &settings,
            &[acc("alice", true), acc("bob", false), acc("carol", false)],
            &[ready("alice", 99, 100), bob, carol],
            now(),
            unlocked,
            move |id, _expected| {
                let activated = activated_cb.clone();
                async move {
                    *activated.lock().unwrap() = Some(id);
                    Ok(ActivateOutcome::Activated)
                }
            },
            &audit,
            &state,
        )
        .await;
        assert!(effect.swapped);
        assert_eq!(
            activated.lock().unwrap().as_ref().map(|id| id.0.as_str()),
            Some("bob"),
            "低档高用量应胜过高级低用量"
        );
        assert!(kimi_switch_core::read_last_auto_swap_at(&state).is_some());
        let event = last_audit(audit.path());
        assert_eq!(event.action, "auto_swap");
        assert_eq!(event.provider, "kimi");
        assert_eq!(event.result, "ok");
        assert_eq!(event.account.as_deref(), Some("bob"));
        let detail = event.detail.expect("success audit has detail");
        assert!(detail.contains("from=alice"), "{detail}");
        assert!(detail.contains("window=5h"), "{detail}");
        assert!(detail.contains("used=99%"), "{detail}");
    }

    /// issue #97 / #111 回驻：默认确认期 N=2，首个合格周期不切；第 2 个周期才
    /// activate 低档账号、写冷却、审计 detail 带 kind=return、输出成功行。
    #[tokio::test]
    async fn status_auto_swap_resident_return_swaps_to_lower_tier() {
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let mut settings = auto_swap_on();
        settings.resident_lower_tier = true;
        let mut alice = ready("alice", 10, 100);
        alice.membership = Some("LEVEL_ADVANCED".into());
        let mut bob = ready("bob", 20, 100);
        bob.membership = Some("LEVEL_INTERMEDIATE".into());
        let first = super::apply_status_auto_swap(
            &settings,
            &[acc("alice", true), acc("bob", false)],
            &[alice.clone(), bob.clone()],
            now(),
            unlocked,
            |_to, _expected| async { panic!("确认期未满不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert!(!first.swapped);
        assert!(first.line.is_none());
        assert!(!audit.path().exists());
        assert_eq!(
            kimi_switch_core::read_resident_confirm_counts(&state)
                .get(&AccountId("bob".into()))
                .copied(),
            Some(1)
        );

        let activated = Arc::new(Mutex::new(None));
        let activated_cb = activated.clone();
        let effect = super::apply_status_auto_swap(
            &settings,
            &[acc("alice", true), acc("bob", false)],
            &[alice, bob],
            now(),
            unlocked,
            move |id, _expected| {
                let activated = activated_cb.clone();
                async move {
                    *activated.lock().unwrap() = Some(id);
                    Ok(ActivateOutcome::Activated)
                }
            },
            &audit,
            &state,
        )
        .await;
        assert!(effect.swapped);
        assert!(
            effect.line.as_deref().is_some_and(|line| !line.is_empty()),
            "回驻成功必须输出结果行"
        );
        assert_eq!(
            activated.lock().unwrap().as_ref().map(|id| id.0.as_str()),
            Some("bob"),
            "必须切回档位更低的 bob"
        );
        assert!(kimi_switch_core::read_last_auto_swap_at(&state).is_some());
        let event = last_audit(audit.path());
        assert_eq!(event.action, "auto_swap");
        assert_eq!(event.result, "ok");
        assert_eq!(event.account.as_deref(), Some("bob"));
        let detail = event.detail.expect("success audit has detail");
        assert!(detail.contains("kind=return"), "{detail}");
        assert!(detail.contains("from=alice"), "{detail}");
    }

    /// 手动保持期内（manual_hold_until = now + 1h）不回驻：不 activate、
    /// 无输出行、不落冷却、无审计。
    #[tokio::test]
    async fn status_auto_swap_resident_return_held_within_manual_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let mut settings = auto_swap_on();
        settings.resident_lower_tier = true;
        let mut alice = ready("alice", 10, 100);
        alice.membership = Some("LEVEL_ADVANCED".into());
        let mut bob = ready("bob", 20, 100);
        bob.membership = Some("LEVEL_INTERMEDIATE".into());
        kimi_switch_core::set_manual_hold_until(&state, now() + chrono::Duration::hours(1));
        let state_before = std::fs::read(&state).unwrap();
        let effect = super::apply_status_auto_swap(
            &settings,
            &[acc("alice", true), acc("bob", false)],
            &[alice, bob],
            now(),
            unlocked,
            |_to, _expected| async { panic!("保持期内不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert!(effect.line.is_none());
        assert!(!effect.swapped);
        assert_eq!(
            std::fs::read(&state).unwrap(),
            state_before,
            "保持期内不落冷却、不碰状态文件"
        );
        assert!(!audit.path().exists());
    }

    /// 回驻子开关默认关：同样的档位差完全无副作用（静默语义，无提示行）。
    #[tokio::test]
    async fn status_auto_swap_resident_return_off_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let mut alice = ready("alice", 10, 100);
        alice.membership = Some("LEVEL_ADVANCED".into());
        let mut bob = ready("bob", 20, 100);
        bob.membership = Some("LEVEL_INTERMEDIATE".into());
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[alice, bob],
            now(),
            unlocked,
            |_to, _expected| async { panic!("回驻关闭不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert!(effect.line.is_none());
        assert!(!effect.swapped);
        assert!(!state.exists());
        assert!(!audit.path().exists());
    }

    /// activate 失败：输出失败行、不写冷却、当前账号路径不视为已切换。
    #[tokio::test]
    async fn status_auto_swap_activate_failure_outputs_line_without_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 99, 100), ready("bob", 10, 100)],
            now(),
            unlocked,
            |_to, _expected| async { Err(kimi_switch_core::Error::provider("disk full")) },
            &audit,
            &state,
        )
        .await;
        assert_eq!(
            effect.line.as_deref(),
            Some("auto_swap kimi/bob failed: provider: disk full")
        );
        assert!(!effect.swapped);
        assert!(!state.exists());
        let event = last_audit(audit.path());
        assert_eq!(event.action, "auto_swap");
        assert_eq!(event.result, "error");
        assert_eq!(event.account.as_deref(), Some("bob"));
        assert!(
            event.detail.as_deref().unwrap_or("").contains("disk full"),
            "{:?}",
            event.detail
        );
    }

    /// 无合格候选：不切换、输出说明行、记审计。
    #[tokio::test]
    async fn status_auto_swap_no_candidate_prints_line_and_audits() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 99, 100), ready("bob", 99, 100)],
            now(),
            unlocked,
            |_to, _expected| async { panic!("无候选不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert_eq!(
            effect.line.as_deref(),
            Some("auto_swap: no eligible candidate")
        );
        assert!(!effect.swapped);
        assert!(!state.exists());
        let event = last_audit(audit.path());
        assert_eq!(event.action, "auto_swap");
        assert_eq!(event.result, "error");
        assert_eq!(event.account.as_deref(), Some("alice"));
        assert_eq!(event.detail.as_deref(), Some("no eligible candidate"));
    }

    #[test]
    fn auto_swap_no_candidate_line_appends_rfc3339_earliest_reset() {
        let early = chrono::DateTime::parse_from_rfc3339("2026-09-03T16:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            super::auto_swap_no_candidate_line(Some(early)),
            "auto_swap: no eligible candidate (earliest reset 2026-09-03T16:00:00Z)"
        );
    }

    #[test]
    fn auto_swap_no_candidate_line_unchanged_without_reset() {
        assert_eq!(
            super::auto_swap_no_candidate_line(None),
            "auto_swap: no eligible candidate"
        );
    }

    /// 无合格候选且有 reset_at：说明行附最早恢复时刻，仍不切换、不写冷却。
    #[tokio::test]
    async fn status_auto_swap_no_candidate_appends_earliest_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let early = chrono::DateTime::parse_from_rfc3339("2026-09-03T16:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let late = chrono::DateTime::parse_from_rfc3339("2026-09-04T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[
                ready_reset("alice", 99, 100, Some(late)),
                ready_reset("bob", 99, 100, Some(early)),
            ],
            now(),
            unlocked,
            |_to, _expected| async { panic!("无候选不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert_eq!(
            effect.line.as_deref(),
            Some("auto_swap: no eligible candidate (earliest reset 2026-09-03T16:00:00Z)")
        );
        assert!(!effect.swapped);
        assert!(!state.exists());
        let event = last_audit(audit.path());
        assert_eq!(event.action, "auto_swap");
        assert_eq!(event.result, "error");
        assert_eq!(event.detail.as_deref(), Some("no eligible candidate"));
    }

    fn parse_status_event(effect: &super::AutoSwapStatusEffect) -> serde_json::Value {
        serde_json::from_str(&effect.json_line()).expect("status json line")
    }

    /// issue #113：耗尽切换成功 → 退出码 0，JSON `swap` / `kind=exhausted`。
    #[tokio::test]
    async fn status_auto_swap_success_exit_0_json_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 99, 100), ready("bob", 10, 100)],
            now(),
            unlocked,
            |_to, _expected| async { Ok(ActivateOutcome::Activated) },
            &audit,
            &state,
        )
        .await;
        assert_eq!(effect.exit_code(), 0);
        let event = parse_status_event(&effect);
        assert_eq!(event["schemaVersion"], 1);
        assert_eq!(event["type"], "swap");
        assert_eq!(event["from"], "alice");
        assert_eq!(event["to"], "bob");
        assert_eq!(event["kind"], "exhausted");
        assert_eq!(event["window"], "5h");
        assert_eq!(event["usagePct"], 99);
    }

    /// issue #113：回驻成功也是已切换 → 退出码 0，JSON `kind=resident`。
    #[tokio::test]
    async fn status_resident_return_exit_0_json_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let mut settings = auto_swap_on();
        settings.resident_lower_tier = true;
        settings.resident_confirm_n = 1;
        let mut alice = ready("alice", 10, 100);
        alice.membership = Some("LEVEL_ADVANCED".into());
        let mut bob = ready("bob", 20, 100);
        bob.membership = Some("LEVEL_INTERMEDIATE".into());
        let effect = super::apply_status_auto_swap(
            &settings,
            &[acc("alice", true), acc("bob", false)],
            &[alice, bob],
            now(),
            unlocked,
            |_to, _expected| async { Ok(ActivateOutcome::Activated) },
            &audit,
            &state,
        )
        .await;
        assert!(effect.swapped);
        assert_eq!(effect.exit_code(), 0);
        let event = parse_status_event(&effect);
        assert_eq!(event["schemaVersion"], 1);
        assert_eq!(event["type"], "swap");
        assert_eq!(event["from"], "alice");
        assert_eq!(event["to"], "bob");
        assert_eq!(event["kind"], "resident");
        assert!(event.get("window").is_none());
        assert!(event.get("usagePct").is_none());
    }

    /// issue #113：开关关闭 / 低于阈值等 Stay → 退出码 2，JSON `stay`。
    #[tokio::test]
    async fn status_auto_swap_disabled_exit_2_json_stay() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let mut settings = auto_swap_on();
        settings.enabled = false;
        let effect = super::apply_status_auto_swap(
            &settings,
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 100, 100), ready("bob", 10, 100)],
            now(),
            unlocked,
            |_to, _expected| async { panic!("disabled 不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert_eq!(effect.exit_code(), 2);
        let event = parse_status_event(&effect);
        assert_eq!(event["schemaVersion"], 1);
        assert_eq!(event["type"], "stay");
        assert_eq!(event["reason"], "disabled");
    }

    /// issue #113：未达阈值 Stay → 退出码 2，reason=below_threshold。
    #[tokio::test]
    async fn status_auto_swap_below_threshold_exit_2_json_stay() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 10, 100), ready("bob", 20, 100)],
            now(),
            unlocked,
            |_to, _expected| async { panic!("低于阈值不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert_eq!(effect.exit_code(), 2);
        let event = parse_status_event(&effect);
        assert_eq!(event["schemaVersion"], 1);
        assert_eq!(event["type"], "stay");
        assert_eq!(event["reason"], "below_threshold");
    }

    /// 期望当前账号（ADR-0020）：切换带着决定时的当前账号 alice 作前置条件；交换锁内发现
    /// 当前账号已不是 alice（用户手动抢先切换了），返回「已被取代」：无输出行、不留切换审计、
    /// 不写冷却；退出码 2，JSON `stay` / reason=superseded。
    #[tokio::test]
    async fn status_auto_swap_superseded_is_stay_without_audit_or_cooldown() {
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let requested = Arc::new(Mutex::new(None));
        let requested_cb = requested.clone();
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 99, 100), ready("bob", 10, 100)],
            now(),
            unlocked,
            move |to: AccountId, expected: AccountId| {
                *requested_cb.lock().unwrap() = Some((to.0, expected.0));
                async { Ok(ActivateOutcome::Superseded) }
            },
            &audit,
            &state,
        )
        .await;
        assert_eq!(
            requested.lock().unwrap().clone(),
            Some(("bob".to_string(), "alice".to_string())),
            "期望当前账号取决定时的当前账号"
        );
        assert!(effect.line.is_none());
        assert!(!effect.swapped);
        assert_eq!(effect.exit_code(), 2);
        let event = parse_status_event(&effect);
        assert_eq!(event["type"], "stay");
        assert_eq!(event["reason"], "superseded");
        assert!(!state.exists(), "已被取代不写冷却");
        assert!(!audit.path().exists(), "已被取代不留切换审计");
    }

    /// 回驻同口径（ADR-0020）：已被取代 → 退出码 2、reason=superseded，不审计、不写冷却。
    #[tokio::test]
    async fn status_resident_return_superseded_is_stay_without_audit_or_cooldown() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let mut settings = auto_swap_on();
        settings.resident_lower_tier = true;
        settings.resident_confirm_n = 1;
        let mut alice = ready("alice", 10, 100);
        alice.membership = Some("LEVEL_ADVANCED".into());
        let mut bob = ready("bob", 20, 100);
        bob.membership = Some("LEVEL_INTERMEDIATE".into());
        let effect = super::apply_status_auto_swap(
            &settings,
            &[acc("alice", true), acc("bob", false)],
            &[alice, bob],
            now(),
            unlocked,
            |to, expected| async move {
                assert_eq!((to.0.as_str(), expected.0.as_str()), ("bob", "alice"));
                Ok(ActivateOutcome::Superseded)
            },
            &audit,
            &state,
        )
        .await;
        assert!(effect.line.is_none());
        assert!(!effect.swapped);
        assert_eq!(effect.exit_code(), 2);
        assert_eq!(parse_status_event(&effect)["reason"], "superseded");
        assert!(!audit.path().exists(), "已被取代不留切换审计");
        assert!(
            kimi_switch_core::read_last_auto_swap_at(&state).is_none(),
            "已被取代不写冷却"
        );
    }

    /// 自动切换串行化锁（issue #194 / ADR-0020）：3 秒拿不到锁（另一自动触发正在切换）时与
    /// superseded 同为 Stay：退出码 2，JSON `stay` / reason=busy；不判定、不 activate、无输出行、
    /// 不留审计、不写状态。
    #[tokio::test]
    async fn status_auto_swap_lock_busy_is_stay_without_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 99, 100), ready("bob", 10, 100)],
            now(),
            || std::future::ready(None::<()>),
            |_to, _expected| async { panic!("拿不到锁不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert!(effect.line.is_none());
        assert!(!effect.swapped);
        assert_eq!(effect.exit_code(), 2);
        let event = parse_status_event(&effect);
        assert_eq!(event["type"], "stay");
        assert_eq!(event["reason"], "busy");
        assert!(!state.exists(), "拿不到锁不写状态");
        assert!(!audit.path().exists(), "拿不到锁不留审计");
    }

    /// 锁内重读状态（ADR-0020）：等锁期间，前一个持锁者刚完成一次自动切换并写下冷却。拿到锁
    /// 之后的判定读到这份冷却，不再切换（退出码 2，reason=cooldown），不留审计。
    #[tokio::test]
    async fn status_auto_swap_rereads_cooldown_after_taking_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 99, 100), ready("bob", 10, 100)],
            now(),
            || {
                // 前一个持锁者释放锁之前写下的冷却。
                kimi_switch_core::write_last_auto_swap_at(
                    &state,
                    now() - chrono::Duration::seconds(1),
                );
                std::future::ready(Some(()))
            },
            |_to, _expected| async { panic!("冷却中不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert!(!effect.swapped);
        assert_eq!(effect.exit_code(), 2);
        assert_eq!(parse_status_event(&effect)["reason"], "cooldown");
        assert!(!audit.path().exists(), "冷却中不留切换审计");
    }

    /// issue #113：已达阈值无合格候选 → 退出码 3，JSON `no_candidate`。
    #[tokio::test]
    async fn status_auto_swap_no_candidate_exit_3_json_event() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 99, 100), ready("bob", 99, 100)],
            now(),
            unlocked,
            |_to, _expected| async { panic!("无候选不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert_eq!(effect.exit_code(), 3);
        let event = parse_status_event(&effect);
        assert_eq!(event["schemaVersion"], 1);
        assert_eq!(event["type"], "no_candidate");
        assert!(event.get("earliestReset").is_none());
    }

    /// issue #113：无候选且有 reset_at → JSON 带 earliestReset（RFC3339 UTC）。
    #[tokio::test]
    async fn status_auto_swap_no_candidate_json_earliest_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let early = chrono::DateTime::parse_from_rfc3339("2026-09-03T16:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let late = chrono::DateTime::parse_from_rfc3339("2026-09-04T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[
                ready_reset("alice", 99, 100, Some(late)),
                ready_reset("bob", 99, 100, Some(early)),
            ],
            now(),
            unlocked,
            |_to, _expected| async { panic!("无候选不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert_eq!(effect.exit_code(), 3);
        let event = parse_status_event(&effect);
        assert_eq!(event["type"], "no_candidate");
        assert_eq!(event["earliestReset"], "2026-09-03T16:00:00Z");
    }

    /// 「最早恢复」只给仍在未来的时刻（issue #191 / ADR-0020）：说明行与 `--json` 的
    /// earliestReset 同口径，摘出的 bob 缓存里已过点的 reset_at 都不给出。
    #[tokio::test]
    async fn status_auto_swap_no_candidate_skips_passed_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let passed = now() - chrono::Duration::hours(1);
        let future = chrono::DateTime::parse_from_rfc3339("2026-09-03T16:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut bob = acc("bob", false);
        kimi_switch_core::write_auto_swap_opt_out_extra(&mut bob.extra, true);
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), bob],
            &[
                ready_reset("alice", 99, 100, Some(future)),
                ready_reset("bob", 100, 100, Some(passed)),
            ],
            now(),
            unlocked,
            |_to, _expected| async { panic!("无候选不得 activate") },
            &audit,
            &state,
        )
        .await;
        assert_eq!(
            effect.line.as_deref(),
            Some("auto_swap: no eligible candidate (earliest reset 2026-09-03T16:00:00Z)")
        );
        let event = parse_status_event(&effect);
        assert_eq!(event["type"], "no_candidate");
        assert_eq!(event["earliestReset"], "2026-09-03T16:00:00Z");
    }

    /// issue #113：activate 失败 → 退出码 1，JSON `error`，失败行语义不变。
    #[tokio::test]
    async fn status_auto_swap_activate_failure_exit_1_json_error() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let state = tmp.path().join("auto_swap.json");
        let effect = super::apply_status_auto_swap(
            &auto_swap_on(),
            &[acc("alice", true), acc("bob", false)],
            &[ready("alice", 99, 100), ready("bob", 10, 100)],
            now(),
            unlocked,
            |_to, _expected| async { Err(kimi_switch_core::Error::provider("disk full")) },
            &audit,
            &state,
        )
        .await;
        assert_eq!(
            effect.line.as_deref(),
            Some("auto_swap kimi/bob failed: provider: disk full")
        );
        assert_eq!(effect.exit_code(), 1);
        let event = parse_status_event(&effect);
        assert_eq!(event["schemaVersion"], 1);
        assert_eq!(event["type"], "error");
        assert_eq!(event["to"], "bob");
        assert_eq!(event["message"], "provider: disk full");
    }

    /// issue #113：`--json` 是默认 status 路径旗标（也可写在子命令前）。
    #[test]
    fn json_flag_parses_on_default_status() {
        use super::{Cli, Cmd};
        use clap::Parser as _;

        let cli = Cli::try_parse_from(["kimi-switch", "--json"]).unwrap();
        assert!(cli.json);
        assert!(cli.cmd.is_none());

        let cli = Cli::try_parse_from(["kimi-switch"]).unwrap();
        assert!(!cli.json);
        assert!(cli.cmd.is_none());

        let cli = Cli::try_parse_from(["kimi-switch", "--json", "swap", "1"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.cmd, Some(Cmd::Swap { ref id }) if id.as_deref() == Some("1")));
    }

    /// issue #113：帮助文本写清退出码、事件类型、additive 契约。
    #[test]
    fn status_help_documents_exit_codes_and_json_contract() {
        use clap::CommandFactory as _;

        let help = super::Cli::command().render_long_help().to_string();
        assert!(help.contains("--json"), "{help}");
        assert!(help.contains("schemaVersion"), "{help}");
        assert!(help.contains("additive"), "{help}");
        assert!(help.contains("no_candidate"), "{help}");
        assert!(
            help.contains("ignore unknown fields"),
            "须写明脚本忽略未知字段：{help}"
        );
        assert!(help.contains("stderr"), "{help}");
        for code in ["  0  ", "  1  ", "  2  ", "  3  "] {
            assert!(help.contains(code), "缺少退出码说明 {code:?}：{help}");
        }
        assert!(
            help.contains("default status") || help.contains("no subcommand"),
            "须限定默认 status 路径：{help}"
        );
    }

    /// issue #113：`--json` 时事件走 stdout、人类可读走 stderr；默认 stdout 不变。
    #[test]
    fn json_mode_routes_event_to_stdout_and_human_to_stderr() {
        let event = super::StatusJsonEvent::stay("disabled");
        let human = [
            "kimi".to_string(),
            "  *  1  alice                    quota: n/a".to_string(),
        ];

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        super::emit_status_output_to(false, &mut stdout, &mut stderr, &human, &event).unwrap();
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "kimi\n  *  1  alice                    quota: n/a\n"
        );
        assert!(stderr.is_empty());

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        super::emit_status_output_to(true, &mut stdout, &mut stderr, &human, &event).unwrap();
        let out = String::from_utf8(stdout).unwrap();
        assert!(out.ends_with('\n'), "{out:?}");
        let parsed: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(parsed["schemaVersion"], 1);
        assert_eq!(parsed["type"], "stay");
        assert_eq!(parsed["reason"], "disabled");
        assert!(
            !out.contains("kimi\n"),
            "json 模式账号列表不得进 stdout：{out}"
        );
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "kimi\n  *  1  alice                    quota: n/a\n"
        );
    }

    /// issue #113：空账号列表视为无事（退出码 2 / stay）。
    #[test]
    fn empty_account_list_is_stay_exit_2() {
        let effect = super::AutoSwapStatusEffect::stay("no_accounts");
        assert_eq!(effect.exit_code(), 2);
        let event = parse_status_event(&effect);
        assert_eq!(event["type"], "stay");
        assert_eq!(event["reason"], "no_accounts");
    }

    /// issue #153 P50 实测（手动执行；spec #152 决策 4 明确不做 CI 时延断言）：
    /// `cargo test -p kimi-switch-cli swap_duration_p50 -- --ignored --nocapture`
    ///
    /// 在 tempdir 内双隔离（`KIMI_SWITCH_HOME` 收口 registry/审计/凭证/快照，
    /// `KIMI_CODE_HOME` 收口 live 登录文件），经真实 CLI `swap` 命令交替切换 21 次，
    /// 从审计记录读出每条 activate 的 `duration_ms`，打印 P50 对照 200ms 预算。
    /// 账号为合成 blob（无任何真实 token），切换全程零网络。
    #[tokio::test]
    #[ignore = "时延实测专用：本地手动执行，只打印数值不做时延断言"]
    async fn swap_duration_p50_measure() {
        use kimi_switch_core::paths::AppPaths;
        use kimi_switch_core::{
            Account, AccountId, AccountRegistry, AuditEvent, AuditLog, CredentialStore, FileStore,
        };
        use kimi_switch_kimi::KimiProvider;
        use std::sync::{Arc, Mutex as TestMutex};

        static ENV_LOCK: TestMutex<()> = TestMutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let switch_home = tempfile::tempdir().unwrap();
        let kimi_home = tempfile::tempdir().unwrap();

        struct RestoreEnv(Option<std::ffi::OsString>, Option<std::ffi::OsString>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("KIMI_SWITCH_HOME", value),
                    None => std::env::remove_var("KIMI_SWITCH_HOME"),
                }
                match self.1.take() {
                    Some(value) => std::env::set_var("KIMI_CODE_HOME", value),
                    None => std::env::remove_var("KIMI_CODE_HOME"),
                }
            }
        }
        let _restore = RestoreEnv(
            std::env::var_os("KIMI_SWITCH_HOME"),
            std::env::var_os("KIMI_CODE_HOME"),
        );
        std::env::set_var("KIMI_SWITCH_HOME", switch_home.path());
        std::env::set_var("KIMI_CODE_HOME", kimi_home.path());

        let paths = AppPaths::resolve().unwrap();
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(paths.credentials_file()));
        let registry = Arc::new(AccountRegistry::new(paths.registry_file()));
        let kimi: Arc<KimiProvider> =
            Arc::new(kimi_switch_kimi::new(store.clone(), registry.clone()));
        let audit = AuditLog::new(paths.audit_log());
        let ctx = super::AppContext {
            store,
            registry,
            kimi,
            audit,
        };

        let now = Utc::now();
        for (uid, active) in [("p50-u1", true), ("p50-u2", false)] {
            ctx.registry
                .upsert(Account {
                    provider: "kimi".into(),
                    id: AccountId(uid.into()),
                    label: uid.into(),
                    active,
                    created_at: now,
                    last_used_at: None,
                    extra: serde_json::Map::new(),
                })
                .unwrap();
            ctx.store
                .set(
                    "kimi",
                    uid,
                    "blob",
                    &format!(r#"{{"access_token":"{uid}"}}"#),
                )
                .unwrap();
        }
        let live_dir = kimi_home.path().join("credentials");
        std::fs::create_dir_all(&live_dir).unwrap();
        std::fs::write(
            live_dir.join("kimi-code.json"),
            r#"{"access_token":"p50-u1"}"#,
        )
        .unwrap();

        const RUNS: usize = 21;
        for i in 0..RUNS {
            let target = if i % 2 == 0 { "p50-u2" } else { "p50-u1" };
            super::swap(&ctx, Some(target)).await.unwrap();
        }

        let mut durations: Vec<u64> = std::fs::read_to_string(paths.audit_log())
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<AuditEvent>(line).ok())
            .filter(|event| event.action == "activate")
            .map(|event| event.duration_ms.expect("activate 记录应含 duration_ms"))
            .collect();
        durations.sort_unstable();
        assert_eq!(
            durations.len(),
            RUNS,
            "每次切换都应写入一条含 duration_ms 的 activate 记录"
        );
        let p50 = durations[RUNS / 2];
        println!(
            "activate duration_ms samples={:?}\nP50={p50}ms（设计预算 200ms，spec #152 决策 4）",
            durations
        );
    }

    // -----------------------------------------------------------------------
    // on-quota-failure / install-hook（issue #162）
    // -----------------------------------------------------------------------

    mod passive_env {
        //! 端到端被动切换测试的双隔离夹具：`KIMI_SWITCH_HOME` 收口 registry / 凭证 /
        //! 审计 / 快照 / 状态文件，`KIMI_CODE_HOME` 收口 live 登录文件。进程级 env
        //! 变量在并行测试间共享一把锁；Drop 先恢复 env 再放锁。

        use std::ffi::OsString;
        use std::path::PathBuf;
        use std::sync::{Mutex, MutexGuard, OnceLock};

        fn env_lock() -> &'static Mutex<()> {
            static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
            LOCK.get_or_init(|| Mutex::new(()))
        }

        pub struct IsolatedHomes {
            pub kimi_home: PathBuf,
            previous: (Option<OsString>, Option<OsString>),
            _guard: MutexGuard<'static, ()>,
        }

        impl Drop for IsolatedHomes {
            fn drop(&mut self) {
                let (switch_prev, kimi_prev) = self.previous.clone();
                match kimi_prev {
                    Some(value) => std::env::set_var("KIMI_CODE_HOME", value),
                    None => std::env::remove_var("KIMI_CODE_HOME"),
                }
                match switch_prev {
                    Some(value) => std::env::set_var("KIMI_SWITCH_HOME", value),
                    None => std::env::remove_var("KIMI_SWITCH_HOME"),
                }
            }
        }

        pub fn isolate() -> IsolatedHomes {
            let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
            // `.keep()` 让 tempdir 目录在 TempDir drop 后仍留在盘上供整个测试使用；
            // 交换侧路径都经 `AppPaths::resolve()`（读 KIMI_SWITCH_HOME env）取得，
            // 因此结构体只保留需要直接读的 kimi_home。
            let switch_home = tempfile::tempdir().unwrap().keep();
            let kimi_home = tempfile::tempdir().unwrap().keep();
            let previous = (
                std::env::var_os("KIMI_SWITCH_HOME"),
                std::env::var_os("KIMI_CODE_HOME"),
            );
            std::env::set_var("KIMI_SWITCH_HOME", &switch_home);
            std::env::set_var("KIMI_CODE_HOME", &kimi_home);
            IsolatedHomes {
                kimi_home,
                previous,
                _guard: guard,
            }
        }
    }

    /// 被动切换端到端夹具：registry 账号 + store blob + live 登录文件。
    /// blob 带 `label` 供 live 身份辨认（official_identity 回退 label），全零网络。
    fn seed_passive_fixture(
        env: &passive_env::IsolatedHomes,
        accounts: &[(&str, bool)],
    ) -> super::AppContext {
        use kimi_switch_core::paths::AppPaths;
        use kimi_switch_core::{Account, AccountId, AccountRegistry, CredentialStore, FileStore};

        let paths = AppPaths::resolve().unwrap();
        let store: std::sync::Arc<dyn CredentialStore> =
            std::sync::Arc::new(FileStore::new(paths.credentials_file()));
        let registry = std::sync::Arc::new(AccountRegistry::new(paths.registry_file()));
        let kimi = std::sync::Arc::new(kimi_switch_kimi::new(store.clone(), registry.clone()));
        let audit = super::AuditLog::new(paths.audit_log());

        let now = Utc::now();
        for (id, active) in accounts {
            registry
                .upsert(Account {
                    provider: "kimi".into(),
                    id: AccountId((*id).into()),
                    label: (*id).into(),
                    active: *active,
                    created_at: now,
                    // 当前账号锚在当下：默认 60s 安顿宽限必命中，断言被动路径不被阻挡。
                    last_used_at: if *active { Some(now) } else { None },
                    extra: Default::default(),
                })
                .unwrap();
            store
                .set(
                    "kimi",
                    id,
                    "blob",
                    &format!(r#"{{"label":"{id}","access_token":"tok-{id}"}}"#),
                )
                .unwrap();
        }
        let live_dir = env.kimi_home.join("credentials");
        std::fs::create_dir_all(&live_dir).unwrap();
        let current = accounts
            .iter()
            .find(|(_, active)| *active)
            .map(|(id, _)| *id)
            .unwrap();
        std::fs::write(
            live_dir.join("kimi-code.json"),
            format!(r#"{{"label":"{current}","access_token":"tok-{current}"}}"#),
        )
        .unwrap();

        super::AppContext {
            store,
            registry,
            kimi,
            audit,
        }
    }

    /// 种额度缓存（被动路径的唯一快照来源，零网络）。
    fn seed_quota_cache(entries: &[(&str, u64, u64)]) {
        let entries: Vec<_> = entries
            .iter()
            .map(|(id, used, limit)| (*id, *used, *limit, None))
            .collect();
        seed_quota_cache_with_reset(&entries);
    }

    /// 同 [`seed_quota_cache`]，每个账号的 5 小时窗另带 `reset_at`。
    fn seed_quota_cache_with_reset(entries: &[(&str, u64, u64, Option<DateTime<Utc>>)]) {
        use kimi_switch_core::paths::AppPaths;
        use kimi_switch_core::{Quota, QuotaCache};

        let cache_path = AppPaths::resolve().unwrap().quota_cache_file();
        let mut cache = QuotaCache::load(&cache_path);
        for (id, used, limit, reset_at) in entries {
            cache.set(
                "kimi",
                id,
                vec![Quota {
                    provider: "kimi".into(),
                    account_id: kimi_switch_core::AccountId((*id).into()),
                    window: QuotaWindow::FiveHour,
                    used: *used,
                    limit: *limit,
                    reset_at: *reset_at,
                    status: kimi_switch_core::QuotaStatus::Ok,
                    note: None,
                    membership: None,
                }],
            );
        }
        cache.save(&cache_path);
    }

    fn passive_settings() -> kimi_switch_core::settings::AutoSwap {
        kimi_switch_core::settings::AutoSwap {
            enabled: true,
            ..kimi_switch_core::settings::AutoSwap::default()
        }
    }

    fn event_403_5h() -> String {
        r#"{"hook_event_name":"StopFailure","session_id":"s-1","client_type":"kimi_code_cli","error":{"status":403,"message":"You have reached your 5-hour usage limit"}}"#.into()
    }

    /// Kimi Code CLI 2.3.0 的 StopFailure 载荷形态：平铺对象，`session_title` 是首句用户
    /// 提示的前 200 字（用户内容）。形态出自 #182 评审对本机 kimi.exe 内嵌源码的阅读，
    /// 未抓过真机事件。
    fn stop_failure_event(session_title: &str, error_type: &str, error_message: &str) -> String {
        serde_json::json!({
            "hook_event_name": "StopFailure",
            "session_id": "0b7c2f9e-5d1a-4c3b-9e8f-1a2b3c4d5e6f",
            "cwd": r"D:\agents\projects\kimi-switch",
            "session_title": session_title,
            "error_type": error_type,
            "error_message": error_message,
        })
        .to_string()
    }

    /// 官方现行额度耗尽事件（#184）：真实平铺载荷，error_message 取 2026-10-24 真机
    /// 会话日志原文，429 只出现在文案前缀里。
    fn event_429_5h() -> String {
        stop_failure_event(
            "修一下 README",
            "APIProviderQuotaExhaustedError",
            "429 You've reached your 5-hour usage limit",
        )
    }

    fn live_blob(env: &passive_env::IsolatedHomes) -> String {
        std::fs::read_to_string(env.kimi_home.join("credentials").join("kimi-code.json")).unwrap()
    }

    fn audit_log_text() -> String {
        use kimi_switch_core::paths::AppPaths;
        std::fs::read_to_string(AppPaths::resolve().unwrap().audit_log()).unwrap_or_default()
    }

    fn audits(action: &str) -> Vec<AuditEvent> {
        audit_log_text()
            .lines()
            .filter_map(|line| serde_json::from_str::<AuditEvent>(line).ok())
            .filter(|event| event.action == action)
            .collect()
    }

    fn auto_swap_audits() -> Vec<AuditEvent> {
        audits("auto_swap")
    }

    /// 被动触发留痕：恰好一行 on_quota_failure 记录，detail 与期望逐字一致。
    fn assert_one_trace(raw: &str, expected: &str) {
        let traces = audits("on_quota_failure");
        assert_eq!(
            traces.len(),
            1,
            "每个 StopFailure 恰好一行留痕：{raw}\n{traces:?}"
        );
        assert_eq!(traces[0].detail.as_deref(), Some(expected), "{raw}");
    }

    /// 不切换且切换侧零副作用：live 仍是 alice、无 auto_swap 审计、无自动切换状态文件
    /// （留痕另由 `assert_one_trace` 断言）。
    fn assert_passive_silent(env: &passive_env::IsolatedHomes, raw: &str) {
        use kimi_switch_core::paths::AppPaths;
        assert_eq!(
            live_blob(env),
            r#"{"label":"alice","access_token":"tok-alice"}"#,
            "不得切换：{raw}"
        );
        assert!(auto_swap_audits().is_empty(), "不得写审计：{raw}");
        assert!(
            !AppPaths::resolve().unwrap().auto_swap_state_file().exists(),
            "不得写状态文件：{raw}"
        );
    }

    /// 被动切换经真实 activate 切到 bob：live 变为 bob、registry active 翻转、冷却落盘、
    /// 恰好一条 ok 审计（带 activate 段耗时，detail 含 from=alice 与 trigger=passive）。
    fn assert_passive_swapped_to_bob(env: &passive_env::IsolatedHomes, ctx: &super::AppContext) {
        use kimi_switch_core::paths::AppPaths;
        assert_eq!(
            live_blob(env),
            r#"{"label":"bob","access_token":"tok-bob"}"#
        );
        assert!(
            ctx.registry
                .list_by_provider("kimi")
                .unwrap()
                .iter()
                .any(|a| a.id.0 == "bob" && a.active),
            "registry active 应翻转为 bob"
        );
        let state = AppPaths::resolve().unwrap().auto_swap_state_file();
        assert!(
            kimi_switch_core::read_last_auto_swap_at(&state).is_some(),
            "冷却应落盘"
        );
        let audits = auto_swap_audits();
        assert_eq!(audits.len(), 1, "恰好一条 auto_swap 审计：{audits:?}");
        assert_eq!(audits[0].result, "ok");
        assert_eq!(audits[0].account.as_deref(), Some("bob"));
        // issue #153 续跑：被动路径审计同样携带 activate 段耗时。
        assert!(
            audits[0].duration_ms.is_some(),
            "被动切换成功审计应含 duration_ms（activate 段耗时）"
        );
        let detail = audits[0].detail.as_deref().unwrap();
        assert!(detail.contains("from=alice"), "{detail}");
        assert!(detail.contains("trigger=passive"), "{detail}");
    }

    /// 无合格候选：live 仍是 alice，恰好一条记在 alice 名下的 no eligible candidate 审计。
    fn assert_no_candidate_kept_alice(env: &passive_env::IsolatedHomes) {
        assert_eq!(
            live_blob(env),
            r#"{"label":"alice","access_token":"tok-alice"}"#,
            "无候选不得切换"
        );
        let audits = auto_swap_audits();
        assert_eq!(audits.len(), 1, "{audits:?}");
        assert_eq!(audits[0].result, "error");
        assert_eq!(audits[0].account.as_deref(), Some("alice"));
        assert_eq!(audits[0].detail.as_deref(), Some("no eligible candidate"));
    }

    /// 端到端：403 usage limit 事件 → 与耗尽路径语义一致的自动切换。当前账号缓存
    /// 明明未达阈值（80/100）也被权威证据覆写触发；安顿宽限与手动保持期不阻挡；
    /// 切换走真实 activate（live 变为 bob、registry active 翻转、冷却落盘、审计
    /// detail 带 trigger=passive）。
    #[tokio::test]
    async fn passive_403_swaps_via_real_activate_despite_grace_and_hold() {
        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        // 手动保持期写入（只约束回驻与新开抢用，不该挡被动耗尽切换）。
        {
            use kimi_switch_core::paths::AppPaths;
            let state = AppPaths::resolve().unwrap().auto_swap_state_file();
            kimi_switch_core::set_manual_hold_until(
                &state,
                Utc::now() + chrono::Duration::hours(1),
            );
        }
        seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);

        super::run_on_quota_failure_with(&ctx, &event_403_5h(), &passive_settings())
            .await
            .unwrap();

        assert_passive_swapped_to_bob(&env, &ctx);
    }

    /// 无合格候选：唯一停泊账号已达阈值 → 不切换、live 不动、记 no-candidate 审计。
    #[tokio::test]
    async fn passive_no_eligible_candidate_keeps_live() {
        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        seed_quota_cache(&[("alice", 80, 100), ("bob", 99, 100)]);

        super::run_on_quota_failure_with(&ctx, &event_403_5h(), &passive_settings())
            .await
            .unwrap();

        assert_no_candidate_kept_alice(&env);
    }

    /// 端到端（#184）：官方 429 五小时额度耗尽事件（真实平铺载荷）与 403 同走耗尽路径。
    /// 当前账号缓存未达阈值（80/100）也被权威证据覆写触发；夹具当前账号刚被选中，
    /// 安顿宽限不阻挡；切换走真实 activate（live 变为 bob、registry active 翻转、冷却
    /// 落盘、恰好一条 ok 审计且 detail 带 trigger=passive）。
    #[tokio::test]
    async fn passive_429_usage_limit_swaps_via_real_activate() {
        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);

        super::run_on_quota_failure_with(&ctx, &event_429_5h(), &passive_settings())
            .await
            .unwrap();

        assert_passive_swapped_to_bob(&env, &ctx);
    }

    /// 429 额度耗尽但无合格候选（#184）：唯一停泊账号已达阈值 → live 不动，
    /// 记一条 no eligible candidate 审计。
    #[tokio::test]
    async fn passive_429_no_eligible_candidate_keeps_live() {
        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        seed_quota_cache(&[("alice", 80, 100), ("bob", 99, 100)]);

        super::run_on_quota_failure_with(&ctx, &event_429_5h(), &passive_settings())
            .await
            .unwrap();

        assert_no_candidate_kept_alice(&env);
    }

    /// 被动触发对额度缓存只读（reviewer 补钉）：403 与官方 429 事件各跑命中（发生切换）
    /// 与落空（无候选）两种路径，quota_cache.json 文件字节都保持不变——耗尽原文这一
    /// 权威证据只在内存经 overlay 合成进判定，不回写缓存，缓存写权仍只属于真实额度查询路径。
    #[tokio::test]
    async fn passive_trigger_leaves_quota_cache_bytes_untouched() {
        use kimi_switch_core::paths::AppPaths;

        for event in [event_403_5h(), event_429_5h()] {
            // 命中路径：真实切换发生。
            {
                let env = passive_env::isolate();
                let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
                seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);
                let cache_path = AppPaths::resolve().unwrap().quota_cache_file();
                let before = std::fs::read(&cache_path).unwrap();

                super::run_on_quota_failure_with(&ctx, &event, &passive_settings())
                    .await
                    .unwrap();

                assert_eq!(
                    live_blob(&env),
                    r#"{"label":"bob","access_token":"tok-bob"}"#,
                    "先证明命中路径确实发生了切换，断言才有意义：{event}"
                );
                assert_eq!(
                    std::fs::read(&cache_path).unwrap(),
                    before,
                    "命中路径不得回写额度缓存：{event}"
                );
            }

            // 落空路径：无合格候选，只记 no-candidate 审计。
            {
                let env = passive_env::isolate();
                let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
                seed_quota_cache(&[("alice", 80, 100), ("bob", 99, 100)]);
                let cache_path = AppPaths::resolve().unwrap().quota_cache_file();
                let before = std::fs::read(&cache_path).unwrap();

                super::run_on_quota_failure_with(&ctx, &event, &passive_settings())
                    .await
                    .unwrap();

                assert_eq!(
                    auto_swap_audits().len(),
                    1,
                    "先证明落空路径确实走到了无候选分支：{event}"
                );
                assert_eq!(
                    std::fs::read(&cache_path).unwrap(),
                    before,
                    "落空路径不得回写额度缓存：{event}"
                );
            }
        }
    }

    /// 过点重置（issue #191 / ADR-0020）：GUI 没开、额度缓存陈旧，bob 的缓存仍显示耗尽
    /// 但 `reset_at` 已过 → 被动触发照样经真实 activate 切到 bob；额度缓存字节不变，内核
    /// 不向上报错（入口恒退出码 0）。
    #[tokio::test]
    async fn passive_swaps_to_account_whose_cached_exhaustion_already_reset() {
        use kimi_switch_core::paths::AppPaths;

        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        let passed = Utc::now() - chrono::Duration::hours(1);
        seed_quota_cache_with_reset(&[("alice", 80, 100, None), ("bob", 100, 100, Some(passed))]);
        let cache_path = AppPaths::resolve().unwrap().quota_cache_file();
        let before = std::fs::read(&cache_path).unwrap();

        super::run_on_quota_failure_with(&ctx, &event_429_5h(), &passive_settings())
            .await
            .expect("观察型内核不得向上报错");

        assert_passive_swapped_to_bob(&env, &ctx);
        assert_eq!(
            std::fs::read(&cache_path).unwrap(),
            before,
            "被动路径不得回写额度缓存"
        );
    }

    /// 耗尽账本（issue #192 / ADR-0020）：GUI 没开，额度缓存停在第一次触发之前。alice 撞上
    /// 额度耗尽、被动切到 bob；冷却过后 bob 也撞上耗尽。alice 的缓存仍显示有余量，但它刚因
    /// 耗尽被切走、之后没有更新的额度快照，不得被切回：不切换，留痕 no_eligible_candidate。
    /// 额度缓存字节不变，内核不向上报错（入口恒退出码 0）；手动切换到有耗尽记录的 alice
    /// 照常成功。
    #[tokio::test]
    async fn passive_does_not_swap_back_to_just_exhausted_account() {
        use kimi_switch_core::paths::AppPaths;

        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);
        let paths = AppPaths::resolve().unwrap();
        let cache_before = std::fs::read(paths.quota_cache_file()).unwrap();
        let settings = passive_settings();
        let raw = event_429_5h();

        super::run_on_quota_failure_with(&ctx, &raw, &settings)
            .await
            .expect("观察型内核不得向上报错");
        assert_eq!(
            live_blob(&env),
            r#"{"label":"bob","access_token":"tok-bob"}"#,
            "alice 耗尽先切到 bob"
        );

        // 冷却过后：把上次自动切换时刻拨到冷却之前，bob 接着撞上额度耗尽。
        kimi_switch_core::write_last_auto_swap_at(
            &paths.auto_swap_state_file(),
            Utc::now()
                - chrono::Duration::milliseconds(settings.cooldown_ms)
                - chrono::Duration::minutes(1),
        );
        super::run_on_quota_failure_with(&ctx, &raw, &settings)
            .await
            .expect("观察型内核不得向上报错");

        assert_eq!(
            live_blob(&env),
            r#"{"label":"bob","access_token":"tok-bob"}"#,
            "不得切回刚耗尽的 alice"
        );
        let traces = audits("on_quota_failure");
        assert_eq!(traces.len(), 2, "{traces:?}");
        assert_eq!(
            traces[1].detail.as_deref(),
            Some("hook_event=StopFailure error_type=APIProviderQuotaExhaustedError signal=quota_429 window=5h decision=stay reason=no_eligible_candidate")
        );
        assert_eq!(
            std::fs::read(paths.quota_cache_file()).unwrap(),
            cache_before,
            "被动路径不得回写额度缓存"
        );

        // 手动切换不看耗尽账本。
        super::swap(&ctx, Some("alice"))
            .await
            .expect("手动切换到有耗尽记录的账号照常成功");
        assert_eq!(
            live_blob(&env),
            r#"{"label":"alice","access_token":"tok-alice"}"#
        );
    }

    /// 耗尽账本 × 过点重置（spec #185 测试决策「A 的缓存 `reset_at` 已过 → 允许切回 A」）：
    /// alice 撞上额度耗尽时缓存里 5 小时窗的 `reset_at` 还在未来，账本按它记下 alice 的恢复
    /// 时刻，被动切到 bob。到了那个时刻、冷却也过了，bob 撞上耗尽：alice 的缓存仍显示 100%，
    /// 但窗口已过点重置、账本记录已到恢复时刻，被动切回 alice。额度缓存字节不变。
    #[tokio::test]
    async fn passive_swaps_back_once_exhausted_account_reset_has_passed() {
        use kimi_switch_core::paths::AppPaths;

        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        let alice_reset = Utc::now() + chrono::Duration::milliseconds(1_500);
        seed_quota_cache_with_reset(&[
            ("alice", 100, 100, Some(alice_reset)),
            ("bob", 10, 100, None),
        ]);
        let paths = AppPaths::resolve().unwrap();
        let cache_before = std::fs::read(paths.quota_cache_file()).unwrap();
        let settings = passive_settings();
        let raw = event_429_5h();

        super::run_on_quota_failure_with(&ctx, &raw, &settings)
            .await
            .expect("观察型内核不得向上报错");
        assert_eq!(
            live_blob(&env),
            r#"{"label":"bob","access_token":"tok-bob"}"#,
            "alice 耗尽先切到 bob"
        );

        // 等到 alice 的 reset_at（即账本记下的恢复时刻）过去，再把冷却拨到过期。
        let wait = (alice_reset - Utc::now()).to_std().unwrap_or_default();
        tokio::time::sleep(wait + std::time::Duration::from_millis(50)).await;
        kimi_switch_core::write_last_auto_swap_at(
            &paths.auto_swap_state_file(),
            Utc::now()
                - chrono::Duration::milliseconds(settings.cooldown_ms)
                - chrono::Duration::minutes(1),
        );
        super::run_on_quota_failure_with(&ctx, &raw, &settings)
            .await
            .expect("观察型内核不得向上报错");

        assert_eq!(
            live_blob(&env),
            r#"{"label":"alice","access_token":"tok-alice"}"#,
            "alice 的窗口已过点重置、账本记录已到恢复时刻：切回 alice"
        );
        let traces = audits("on_quota_failure");
        assert_eq!(traces.len(), 2, "{traces:?}");
        assert_eq!(
            traces[1].detail.as_deref(),
            Some("hook_event=StopFailure error_type=APIProviderQuotaExhaustedError signal=quota_429 window=5h decision=swap from=bob to=alice")
        );
        assert_eq!(
            std::fs::read(paths.quota_cache_file()).unwrap(),
            cache_before,
            "被动路径不得回写额度缓存"
        );
    }

    /// 迟到的被动触发（ADR-0020 自动切换串行化锁决策 9）：alice 撞上额度耗尽、被动切到 bob；
    /// alice 会话在切换前发出的请求随即也失败，hook 在安顿宽限内到达，此刻当前账号已是 bob。
    /// 这次耗尽归属不明，不记进耗尽账本，冷却照常挡住切换。之后用户手动切回 alice、冷却过后
    /// alice 再次撞上耗尽：bob 没有被错记成刚耗尽，仍是合格的停泊候选，被动切到 bob。
    #[tokio::test]
    async fn passive_late_trigger_within_settle_grace_does_not_ledger_new_current_account() {
        use kimi_switch_core::paths::AppPaths;

        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);
        let paths = AppPaths::resolve().unwrap();
        let settings = passive_settings();
        let raw = event_429_5h();
        let bob_live = r#"{"label":"bob","access_token":"tok-bob"}"#;

        super::run_on_quota_failure_with(&ctx, &raw, &settings)
            .await
            .expect("观察型内核不得向上报错");
        assert_eq!(live_blob(&env), bob_live, "alice 耗尽先切到 bob");

        // 安顿宽限内迟到的同一会话失败：当前账号已是 bob，冷却挡住切换。
        super::run_on_quota_failure_with(&ctx, &raw, &settings)
            .await
            .expect("观察型内核不得向上报错");
        assert_eq!(live_blob(&env), bob_live, "冷却中不切换");

        // 用户手动切回 alice；冷却过后 alice 再次撞上额度耗尽。
        super::swap(&ctx, Some("alice"))
            .await
            .expect("手动切换照常成功");
        kimi_switch_core::write_last_auto_swap_at(
            &paths.auto_swap_state_file(),
            Utc::now()
                - chrono::Duration::milliseconds(settings.cooldown_ms)
                - chrono::Duration::minutes(1),
        );
        super::run_on_quota_failure_with(&ctx, &raw, &settings)
            .await
            .expect("观察型内核不得向上报错");

        assert_eq!(
            live_blob(&env),
            bob_live,
            "bob 没有被错记成刚耗尽，仍是合格候选"
        );
        let traces = audits("on_quota_failure");
        assert_eq!(traces.len(), 3, "{traces:?}");
        assert_eq!(
            traces[2].detail.as_deref(),
            Some("hook_event=StopFailure error_type=APIProviderQuotaExhaustedError signal=quota_429 window=5h decision=swap from=alice to=bob")
        );
    }

    /// 总开关关闭（issue #192）：撞上额度耗尽也不写耗尽账本——已有的状态文件与额度缓存
    /// 字节都不变，内核不向上报错（入口恒退出码 0）。
    #[tokio::test]
    async fn passive_disabled_writes_no_exhaustion_ledger() {
        use kimi_switch_core::paths::AppPaths;

        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);
        let paths = AppPaths::resolve().unwrap();
        kimi_switch_core::set_manual_hold_until(
            &paths.auto_swap_state_file(),
            Utc::now() + chrono::Duration::hours(1),
        );
        let state_before = std::fs::read(paths.auto_swap_state_file()).unwrap();
        let cache_before = std::fs::read(paths.quota_cache_file()).unwrap();
        let mut off = passive_settings();
        off.enabled = false;

        for raw in [event_429_5h(), event_403_5h()] {
            super::run_on_quota_failure_with(&ctx, &raw, &off)
                .await
                .expect("观察型内核不得向上报错");
        }

        assert_eq!(
            std::fs::read(paths.auto_swap_state_file()).unwrap(),
            state_before,
            "总开关关闭不写耗尽账本"
        );
        assert_eq!(
            std::fs::read(paths.quota_cache_file()).unwrap(),
            cache_before
        );
        assert_eq!(
            live_blob(&env),
            r#"{"label":"alice","access_token":"tok-alice"}"#
        );
    }

    /// 冷却收敛：第一次切换完成后，后续触发（新当前账号健康 + 冷却未过）自然落空，
    /// 不再产生切换或切换审计（只留痕）。
    #[tokio::test]
    async fn passive_second_trigger_after_swap_falls_through() {
        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);

        super::run_on_quota_failure_with(&ctx, &event_403_5h(), &passive_settings())
            .await
            .unwrap();
        assert_eq!(auto_swap_audits().len(), 1);

        // 同一会话紧接着又撞 403：bob 才切过来（冷却 5 分钟内），不得再切。
        super::run_on_quota_failure_with(&ctx, &event_403_5h(), &passive_settings())
            .await
            .unwrap();
        assert_eq!(
            live_blob(&env),
            r#"{"label":"bob","access_token":"tok-bob"}"#
        );
        assert_eq!(auto_swap_audits().len(), 1, "冷却期内不得再次切换");
    }

    /// 期望当前账号（ADR-0020）：被动判定决定从 alice 切到 bob 之后、交换锁之前，用户手动切到
    /// 了 carol。带前置条件的切换返回「已被取代」：手动选择保住（当前登录文件与 registry 都是
    /// carol），不留 auto_swap 审计、不写冷却，留痕 reason=superseded，观察型内核不报错。
    #[tokio::test]
    async fn passive_superseded_by_manual_switch_keeps_manual_choice() {
        use kimi_switch_core::paths::AppPaths;
        use kimi_switch_core::Provider;

        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false), ("carol", false)]);
        seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);
        let kimi = ctx.kimi.clone();
        super::before_next_passive_activate(move || {
            // 手动切换走无前置条件的 activate；跑在另一线程自己的 runtime 上，探针本身是同步的。
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(kimi.activate(&AccountId("carol".into())))
            })
            .join()
            .unwrap()
            .unwrap();
        });

        let raw = event_429_5h();
        super::run_on_quota_failure_with(&ctx, &raw, &passive_settings())
            .await
            .expect("观察型内核不得向上报错");

        assert_eq!(
            live_blob(&env),
            r#"{"label":"carol","access_token":"tok-carol"}"#,
            "手动选择不得被自动切换覆盖"
        );
        assert!(
            ctx.registry
                .list_by_provider("kimi")
                .unwrap()
                .iter()
                .any(|a| a.id.0 == "carol" && a.active),
            "registry 当前标记保持手动切换的 carol"
        );
        assert!(auto_swap_audits().is_empty(), "已被取代不留切换审计");
        let state = AppPaths::resolve().unwrap().auto_swap_state_file();
        assert!(
            kimi_switch_core::read_last_auto_swap_at(&state).is_none(),
            "已被取代不写冷却"
        );
        assert_one_trace(
            &raw,
            "hook_event=StopFailure error_type=APIProviderQuotaExhaustedError signal=quota_429 window=5h decision=stay reason=superseded",
        );
    }

    /// 畸形 stdin / 总开关关闭：切换侧零副作用（无切换审计、无状态文件、live 不动，
    /// 只留一行痕）。不是额度耗尽的 429 见 `passive_non_exhaustion_429_is_silent`。
    #[tokio::test]
    async fn passive_malformed_and_disabled_are_silent() {
        for (raw, settings) in [
            ("not json at all", passive_settings()),
            ("", passive_settings()),
            (&event_403_5h(), {
                let mut off = passive_settings();
                off.enabled = false;
                off
            }),
        ] {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);

            super::run_on_quota_failure_with(&ctx, raw, &settings)
                .await
                .unwrap();

            assert_passive_silent(&env, raw);
        }
    }

    /// 不是额度耗尽的 429（#184）：引擎过载、请求过多、并发上限一律不切换——
    /// 无切换审计、无状态文件、live 不动（停泊账号 bob 明明合格也不切）。
    #[tokio::test]
    async fn passive_non_exhaustion_429_is_silent() {
        for raw in [
            r#"{"hook_event_name":"StopFailure","error":{"status":429,"message":"The engine is currently overloaded, please try again later"}}"#,
            r#"{"hook_event_name":"StopFailure","error":{"status":429,"message":"Too Many Requests"}}"#,
            r#"{"hook_event_name":"StopFailure","error":{"status":429,"message":"You've reached your concurrent request limit."}}"#,
        ] {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);

            super::run_on_quota_failure_with(&ctx, raw, &passive_settings())
                .await
                .unwrap();

            assert_passive_silent(&env, raw);
        }
    }

    /// 会话标题是用户内容（#182 评审）：用户在首句提示里贴额度耗尽报错排查之后，这个
    /// 会话撞上过载 429 或网络错误都不得切换——无切换审计、无状态文件、live 不动。
    #[tokio::test]
    async fn passive_session_title_quoting_exhaustion_is_silent() {
        for raw in [
            stop_failure_event(
                "You've reached your 5-hour usage limit 之后自动切换没切，帮我查",
                "APIProviderOverloadedError",
                "429 The engine is currently overloaded, please try again later",
            ),
            stop_failure_event(
                "429 You've reached your 5-hour usage limit 是什么意思",
                "APIConnectionError",
                "Connection error.",
            ),
        ] {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);

            super::run_on_quota_failure_with(&ctx, &raw, &passive_settings())
                .await
                .unwrap();

            assert_passive_silent(&env, &raw);
        }
    }

    /// 并发多触发收敛（锁 + 冷却）：403 与官方 429 事件各有 5 个触发同时命中，自动切换
    /// 串行化锁把「读状态 → 判定 → activate → 写冷却」排队；第一触发完成后，后续触发
    /// 在锁内看到冷却即自然落空——全程恰好一次切换、稳定在 bob，不来回抖动。
    #[tokio::test]
    async fn passive_concurrent_triggers_converge_without_oscillation() {
        for event in [event_403_5h(), event_429_5h()] {
            let env = passive_env::isolate();
            let ctx = std::sync::Arc::new(seed_passive_fixture(
                &env,
                &[("alice", true), ("bob", false), ("carol", false)],
            ));
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100), ("carol", 20, 100)]);

            const TRIGGERS: usize = 5;
            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(TRIGGERS));
            let mut handles = Vec::with_capacity(TRIGGERS);
            for _ in 0..TRIGGERS {
                let ctx = ctx.clone();
                let barrier = barrier.clone();
                let event = event.clone();
                handles.push(tokio::spawn(async move {
                    barrier.wait().await;
                    super::run_on_quota_failure_with(&ctx, &event, &passive_settings())
                        .await
                        .unwrap();
                }));
            }
            for handle in handles {
                handle.await.unwrap();
            }

            assert_eq!(
                live_blob(&env),
                r#"{"label":"bob","access_token":"tok-bob"}"#,
                "并发触发后应稳定在排序最优的 bob（已用最低）：{event}"
            );
            let audits = auto_swap_audits();
            assert_eq!(audits.len(), 1, "只产生必要次数的切换：{audits:?}");
            assert_eq!(audits[0].result, "ok");
            assert_eq!(audits[0].account.as_deref(), Some("bob"));
            assert!(audits[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("trigger=passive"));

            // 全部完成后：冷却期内再触发，落空。
            super::run_on_quota_failure_with(&ctx, &event, &passive_settings())
                .await
                .unwrap();
            assert_eq!(
                auto_swap_audits().len(),
                1,
                "冷却收敛后的触发不得再切换：{event}"
            );
        }
    }

    /// 自动切换串行化锁（issue #194 / ADR-0020）：CLI status 自动切换刚把已耗尽的 alice 切到
    /// bob、冷却还没写下，alice 会话的配额失败 hook 恰在此刻到达。被动路径重读账号库时当前账号
    /// 已是 bob：status 不拿同一把锁时，耗尽证据被错按在 bob 身上再切一跳（bob → carol），留下
    /// 两条切换审计。全部自动触发共用自动切换串行化锁后，被动触发等不到 status 释放锁，3 秒后
    /// 以 busy 放弃：恰好切一次、只留一条切换审计，留痕 reason=busy，观察型内核不向上报错
    /// （入口恒退出码 0）。
    #[tokio::test]
    async fn status_and_passive_concurrent_triggers_swap_exactly_once() {
        use kimi_switch_core::paths::AppPaths;
        use kimi_switch_core::Provider;

        let env = passive_env::isolate();
        let ctx = std::sync::Arc::new(seed_passive_fixture(
            &env,
            &[("alice", true), ("bob", false), ("carol", false)],
        ));
        // 被动路径只读额度缓存（零网络）；status 用自己刚查到的快照判定。
        seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100), ("carol", 20, 100)]);
        let paths = AppPaths::resolve().unwrap();
        let state = paths.auto_swap_state_file();
        let lock_path = paths.auto_swap_lock_file();
        // 夹具把当前账号锚在当下：status 走轮询口径，安顿宽限置 0 才不挡这次耗尽切换。
        let mut settings = passive_settings();
        settings.settle_grace_ms = 0;
        let accounts = ctx.list_ordered().unwrap();
        let snapshots = [
            ready("alice", 99, 100),
            ready("bob", 10, 100),
            ready("carol", 20, 100),
        ];

        let raw = event_429_5h();
        let kimi = ctx.kimi.clone();
        let passive_ctx = ctx.clone();
        let passive_raw = raw.clone();
        let passive_settings = settings.clone();
        let effect = super::apply_status_auto_swap(
            &settings,
            &accounts,
            &snapshots,
            Utc::now(),
            move || kimi_switch_core::AutoSwapLock::acquire_async(lock_path),
            move |to: AccountId, expected: AccountId| async move {
                let outcome = kimi.activate_if_current(&to, &expected).await;
                // status 的切换已提交、冷却尚未写下：alice 会话的配额失败 hook 恰在此刻到达。
                super::run_on_quota_failure_with(&passive_ctx, &passive_raw, &passive_settings)
                    .await
                    .expect("观察型内核不得向上报错");
                outcome
            },
            &ctx.audit,
            &state,
        )
        .await;

        assert!(effect.swapped, "status 自动切换照常完成");
        assert_eq!(
            live_blob(&env),
            r#"{"label":"bob","access_token":"tok-bob"}"#,
            "耗尽证据不得被错按在 bob 身上再切一跳"
        );
        let audits = auto_swap_audits();
        assert_eq!(audits.len(), 1, "恰好一条切换审计：{audits:?}");
        assert_eq!(audits[0].account.as_deref(), Some("bob"));
        assert!(
            !audits[0]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("trigger=passive"),
            "这条审计来自 status 自动切换：{audits:?}"
        );
        assert_one_trace(
            &raw,
            "hook_event=StopFailure error_type=APIProviderQuotaExhaustedError signal=quota_429 window=5h decision=stay reason=busy",
        );
        // 没拿到锁的被动触发也不记耗尽账本：账本写在锁内（#192），bob 没被记成已耗尽。
        assert_eq!(
            kimi_switch_core::earliest_recovery_at(&state, &snapshots, Utc::now()),
            None
        );
    }

    /// 手动切换不经自动切换串行化锁（ADR-0020）：自动触发正持锁时，`swap` 照常立刻完成，
    /// 不等锁、不报 busy。
    #[tokio::test]
    async fn manual_swap_ignores_auto_swap_lock() {
        use kimi_switch_core::paths::AppPaths;

        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        let _held = kimi_switch_core::AutoSwapLock::acquire(
            &AppPaths::resolve().unwrap().auto_swap_lock_file(),
        )
        .expect("先占住自动切换串行化锁");

        super::swap(&ctx, Some("bob"))
            .await
            .expect("手动切换不受自动切换串行化锁影响");
        assert_eq!(
            live_blob(&env),
            r#"{"label":"bob","access_token":"tok-bob"}"#
        );
    }

    /// 留痕（未命中的 429）：不是额度耗尽的 429 也追加恰好一行 on_quota_failure 记录，
    /// 写明事件名、error_type、未命中原因与不切换的决定；切换侧照旧静默。
    #[tokio::test]
    async fn passive_trace_records_unmatched_429() {
        for (error_type, message, expected) in [
            (
                "APIProviderOverloadedError",
                "429 The engine is currently overloaded, please try again later",
                "hook_event=StopFailure error_type=APIProviderOverloadedError signal=no_usage_limit decision=stay reason=not_exhausted",
            ),
            (
                "APIProviderRateLimitError",
                "429 You've reached your concurrent request limit.",
                "hook_event=StopFailure error_type=APIProviderRateLimitError signal=no_usage_limit decision=stay reason=not_exhausted",
            ),
        ] {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);
            let raw = stop_failure_event("修一下 README", error_type, message);

            super::run_on_quota_failure_with(&ctx, &raw, &passive_settings())
                .await
                .unwrap();

            assert_passive_silent(&env, &raw);
            assert_one_trace(&raw, expected);
        }
    }

    /// 留痕（stdin 畸形）：不是合法 JSON 也留一行，未命中原因记 malformed_stdin，
    /// 事件字段读不到记 `-`；切换侧照旧静默。
    #[tokio::test]
    async fn passive_trace_records_malformed_stdin() {
        for raw in ["not json at all", ""] {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);

            super::run_on_quota_failure_with(&ctx, raw, &passive_settings())
                .await
                .unwrap();

            assert_passive_silent(&env, raw);
            assert_one_trace(
                raw,
                "hook_event=- error_type=- signal=malformed_stdin decision=stay reason=not_exhausted",
            );
        }
    }

    /// 留痕（有 usage limit 文案仍未命中）：没带本地 429 / 403 信号记 no_status_signal
    /// （如 SDK 去掉了 "429 " 前缀）；带 429 却缺官方措辞 "reached your" 记
    /// no_official_wording（如官方改了文案）。两种都不切换。
    #[tokio::test]
    async fn passive_trace_names_usage_limit_miss() {
        for (message, expected) in [
            (
                "You've reached your 5-hour usage limit",
                "hook_event=StopFailure error_type=APIProviderQuotaExhaustedError signal=no_status_signal decision=stay reason=not_exhausted",
            ),
            (
                "429 You have hit your 5-hour usage limit",
                "hook_event=StopFailure error_type=APIProviderQuotaExhaustedError signal=no_official_wording decision=stay reason=not_exhausted",
            ),
        ] {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);
            let raw = stop_failure_event("修一下 README", "APIProviderQuotaExhaustedError", message);

            super::run_on_quota_failure_with(&ctx, &raw, &passive_settings())
                .await
                .unwrap();

            assert_passive_silent(&env, &raw);
            assert_one_trace(&raw, expected);
        }
    }

    /// 留痕（命中并切换）：写明命中的口径（官方 429 / 旧口径 403）、辨认出的窗口与切换的
    /// 起止账号；标题引用别的窗口的耗尽文案时，记录里同样找不到它。
    #[tokio::test]
    async fn passive_trace_records_swap() {
        for (raw, expected) in [
            (
                stop_failure_event(
                    "You've reached your monthly usage limit 是什么意思",
                    "APIProviderQuotaExhaustedError",
                    "429 You've reached your 5-hour usage limit",
                ),
                "hook_event=StopFailure error_type=APIProviderQuotaExhaustedError signal=quota_429 window=5h decision=swap from=alice to=bob",
            ),
            (
                event_403_5h(),
                "hook_event=StopFailure error_type=- signal=quota_403 window=5h decision=swap from=alice to=bob",
            ),
        ] {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);

            super::run_on_quota_failure_with(&ctx, &raw, &passive_settings())
                .await
                .unwrap();

            assert_passive_swapped_to_bob(&env, &ctx);
            assert_one_trace(&raw, expected);
            let log = audit_log_text();
            for fragment in ["monthly", "是什么意思", "reached your", "projects"] {
                assert!(!log.contains(fragment), "留痕不得含用户内容「{fragment}」：{log}");
            }
        }
    }

    /// 留痕（命中但不切换）：总开关关闭记 reason=disabled；无合格候选记
    /// reason=no_eligible_candidate；冷却期内的第二次触发记 reason=cooldown。
    #[tokio::test]
    async fn passive_trace_records_stay_reason() {
        // 总开关关闭：判定之前放行，切换侧零副作用。
        {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);
            let raw = event_403_5h();
            let mut off = passive_settings();
            off.enabled = false;

            super::run_on_quota_failure_with(&ctx, &raw, &off)
                .await
                .unwrap();

            assert_passive_silent(&env, &raw);
            assert_one_trace(
                &raw,
                "hook_event=StopFailure error_type=- signal=quota_403 window=5h decision=stay reason=disabled",
            );
        }
        // 无合格候选：与 auto_swap 的无候选审计并存。
        {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 99, 100)]);
            let raw = event_429_5h();

            super::run_on_quota_failure_with(&ctx, &raw, &passive_settings())
                .await
                .unwrap();

            assert_no_candidate_kept_alice(&env);
            assert_one_trace(
                &raw,
                "hook_event=StopFailure error_type=APIProviderQuotaExhaustedError signal=quota_429 window=5h decision=stay reason=no_eligible_candidate",
            );
        }
        // 冷却期内的第二次触发：同一会话切到 bob 后紧接着又撞耗尽。
        {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);
            let raw = event_429_5h();

            for _ in 0..2 {
                super::run_on_quota_failure_with(&ctx, &raw, &passive_settings())
                    .await
                    .unwrap();
            }

            let traces = audits("on_quota_failure");
            assert_eq!(traces.len(), 2, "{traces:?}");
            assert_eq!(
                traces[1].detail.as_deref(),
                Some("hook_event=StopFailure error_type=APIProviderQuotaExhaustedError signal=quota_429 window=5h decision=stay reason=cooldown")
            );
        }
    }

    /// 留痕不含用户内容：会话标题引用额度耗尽文案时照常留一行，但审计日志里找不到
    /// 标题与工作目录的任何片段。
    #[tokio::test]
    async fn passive_trace_omits_user_content() {
        for (title, error_type, message, expected) in [
            (
                "You've reached your 5-hour usage limit 之后自动切换没切，帮我查",
                "APIProviderOverloadedError",
                "429 The engine is currently overloaded, please try again later",
                "hook_event=StopFailure error_type=APIProviderOverloadedError signal=no_usage_limit decision=stay reason=not_exhausted",
            ),
            (
                "429 You've reached your 5-hour usage limit 是什么意思",
                "APIConnectionError",
                "Connection error.",
                "hook_event=StopFailure error_type=APIConnectionError signal=no_usage_limit decision=stay reason=not_exhausted",
            ),
        ] {
            let env = passive_env::isolate();
            let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
            seed_quota_cache(&[("alice", 80, 100), ("bob", 10, 100)]);
            let raw = stop_failure_event(title, error_type, message);

            super::run_on_quota_failure_with(&ctx, &raw, &passive_settings())
                .await
                .unwrap();

            assert_one_trace(&raw, expected);
            let log = audit_log_text();
            for fragment in ["reached your", "usage limit", "帮我查", "是什么意思", "projects"] {
                assert!(!log.contains(fragment), "留痕不得含用户内容「{fragment}」：{log}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // install-hook（tempdir 假 config，绝不碰真机 Kimi Code 配置）
    // -----------------------------------------------------------------------

    fn hook_config_seed() -> &'static str {
        // 含用户注释、普通键、既有 [[hooks]] 条目：断言全部保留。
        "# user comment line\nmodel = \"kimi-k2\"\n\n# keep my hooks here\n[[hooks]]\nevent = \"PreToolUse\"\ncommand = \"echo pre\"\n"
    }

    fn count_our_hook_entries(raw: &str) -> usize {
        let value: toml::Value = toml::from_str(raw).unwrap();
        value
            .get("hooks")
            .and_then(|hooks| hooks.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| {
                        entry.get("event").and_then(|v| v.as_str()) == Some("StopFailure")
                            && entry
                                .get("command")
                                .and_then(|v| v.as_str())
                                .is_some_and(|c| c.contains("on-quota-failure"))
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// 幂等安装：重复执行无重复条目；注释与用户既有条目原样保留。
    #[test]
    fn install_hook_is_idempotent_and_preserves_user_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config.toml");
        std::fs::write(&config, hook_config_seed()).unwrap();
        let exe = tmp.path().join("kimi-switch-cli.exe");

        super::install_hook_into(&config, &exe, false).unwrap();
        let once = std::fs::read_to_string(&config).unwrap();
        assert_eq!(count_our_hook_entries(&once), 1, "{once}");
        assert!(once.contains("# user comment line"), "{once}");
        assert!(once.contains("model = \"kimi-k2\""), "{once}");
        assert!(once.contains("\"PreToolUse\""), "{once}");
        assert!(once.contains("echo pre"), "{once}");

        super::install_hook_into(&config, &exe, false).unwrap();
        let twice = std::fs::read_to_string(&config).unwrap();
        assert_eq!(
            count_our_hook_entries(&twice),
            1,
            "重复安装不得追加重复条目：{twice}"
        );

        // 条目内容核对：事件、command 指向本工具、timeout 写入。
        let value: toml::Value = toml::from_str(&twice).unwrap();
        let ours = value["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("StopFailure"))
            .unwrap();
        let command = ours.get("command").unwrap().as_str().unwrap();
        assert!(command.contains("on-quota-failure"), "{command}");
        assert!(command.contains("kimi-switch-cli.exe"), "{command}");
        assert!(
            command.starts_with('"'),
            "可执行路径须加引号防空格：{command}"
        );
    }

    /// 可执行文件移动后重装：条目刷新到新路径，仍只一条。
    #[test]
    fn install_hook_refreshes_moved_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config.toml");
        std::fs::write(&config, hook_config_seed()).unwrap();
        let exe_a = tmp.path().join("bin_a").join("kimi-switch-cli.exe");
        let exe_b = tmp.path().join("bin b").join("kimi-switch-cli.exe");

        super::install_hook_into(&config, &exe_a, false).unwrap();
        super::install_hook_into(&config, &exe_b, false).unwrap();
        let raw = std::fs::read_to_string(&config).unwrap();
        assert_eq!(count_our_hook_entries(&raw), 1, "{raw}");
        let value: toml::Value = toml::from_str(&raw).unwrap();
        let command = value["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("StopFailure"))
            .unwrap()
            .get("command")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            command.contains("bin b"),
            "command 应刷新到新路径：{command}"
        );
        assert!(!command.contains("bin_a"), "{command}");
    }

    /// 卸载：只删自己的条目；用户自己的 StopFailure hook（不含我们的标记）与注释保留；
    /// 无条目可删时不重写文件（字节不变）。
    #[test]
    fn uninstall_hook_removes_only_ours_and_leaves_file_untouched_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config.toml");
        let seed = "# user comment line\n\n[[hooks]]\nevent = \"StopFailure\"\ncommand = \"node my-failure-handler.mjs\"\n";
        std::fs::write(&config, seed).unwrap();
        let exe = tmp.path().join("kimi-switch-cli.exe");

        // 未安装过 → 无可删除，文件不动。
        super::install_hook_into(&config, &exe, true).unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), seed);

        // 安装 → 卸载：我们的条目消失，用户的 StopFailure hook 保留。
        super::install_hook_into(&config, &exe, false).unwrap();
        super::install_hook_into(&config, &exe, true).unwrap();
        let raw = std::fs::read_to_string(&config).unwrap();
        assert_eq!(count_our_hook_entries(&raw), 0, "{raw}");
        assert!(raw.contains("my-failure-handler.mjs"), "{raw}");
        assert!(raw.contains("# user comment line"), "{raw}");
        assert!(!raw.contains("kimi-switch-cli.exe"), "{raw}");

        // 再卸载一次：无条目可删，文件保持字节不变。
        let before = std::fs::read_to_string(&config).unwrap();
        super::install_hook_into(&config, &exe, true).unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    }

    /// 无 hooks 段的干净 config：安装 → 卸载一轮后还原到无 hooks 段的形态。
    #[test]
    fn uninstall_hook_restores_config_without_hooks_section() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config.toml");
        let clean = "# just a comment\nmodel = \"kimi-k2\"\n";
        std::fs::write(&config, clean).unwrap();
        let exe = tmp.path().join("kimi-switch-cli.exe");

        super::install_hook_into(&config, &exe, false).unwrap();
        let installed = std::fs::read_to_string(&config).unwrap();
        assert_eq!(count_our_hook_entries(&installed), 1, "{installed}");

        super::install_hook_into(&config, &exe, true).unwrap();
        let raw = std::fs::read_to_string(&config).unwrap();
        assert_eq!(count_our_hook_entries(&raw), 0, "{raw}");
        assert!(!raw.contains("hooks"), "空 hooks 段应一并移除：{raw}");
        assert!(raw.contains("# just a comment"), "{raw}");
        assert!(raw.contains("model = \"kimi-k2\""), "{raw}");
    }

    /// 损坏的 config.toml：报错退出，绝不覆写。
    #[test]
    fn install_hook_refuses_corrupt_config() {
        use std::path::Path;

        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config.toml");
        let corrupt = "not [ valid toml =\n";
        std::fs::write(&config, corrupt).unwrap();

        assert!(super::install_hook_into(&config, &Path::new("x.exe"), false).is_err());
        assert!(super::install_hook_into(&config, &Path::new("x.exe"), true).is_err());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), corrupt);
    }

    /// `hooks` 存在但不是 [[hooks]] 数组（如被写成普通表）：拒绝触碰，报错不改写。
    #[test]
    fn install_hook_refuses_non_array_hooks_table() {
        use std::path::Path;

        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config.toml");
        let bad = "[hooks]\nevent = \"StopFailure\"\n";
        std::fs::write(&config, bad).unwrap();

        assert!(super::install_hook_into(&config, &Path::new("x.exe"), false).is_err());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), bad);
    }

    /// clap 解析：子命令命名与 --uninstall 旗标。
    #[test]
    fn hook_subcommands_parse() {
        use super::{Cli, Cmd};
        use clap::Parser as _;

        let cli = Cli::try_parse_from(["kimi-switch", "on-quota-failure"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::OnQuotaFailure)));

        let cli = Cli::try_parse_from(["kimi-switch", "on-session-start"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::OnSessionStart)));

        let cli = Cli::try_parse_from(["kimi-switch", "install-hook"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Some(Cmd::InstallHook { uninstall: false })
        ));

        let cli = Cli::try_parse_from(["kimi-switch", "install-hook", "--uninstall"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Some(Cmd::InstallHook { uninstall: true })
        ));
    }

    /// config.toml 尚不存在（Kimi Code 未配置过 hooks）：安装时新建文件与目录。
    #[test]
    fn install_hook_creates_missing_config_file() {
        use std::path::Path;

        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("nested").join("config.toml");

        super::install_hook_into(&config, &Path::new("C:/tools/kimi-switch-cli.exe"), false)
            .unwrap();
        let raw = std::fs::read_to_string(&config).unwrap();
        assert_eq!(count_our_hook_entries(&raw), 1, "{raw}");
        let value: toml::Value = toml::from_str(&raw).unwrap();
        let command = value["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("StopFailure"))
            .unwrap()
            .get("command")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(
            command.contains("C:/tools/kimi-switch-cli.exe"),
            "{command}"
        );
    }

    /// 帮助文本把 hook 子命令写进命令面（用户要手动执行 install-hook）。
    #[test]
    fn help_documents_hook_subcommands() {
        use clap::CommandFactory as _;

        let help = super::Cli::command().render_long_help().to_string();
        assert!(help.contains("on-quota-failure"), "{help}");
        assert!(help.contains("install-hook"), "{help}");
        assert!(help.contains("Always exits 0"), "{help}");
        assert!(help.contains("--uninstall"), "{help}");

        // 拉起器（issue #197）：由 hook 唤起、退出码恒 0、不要手动执行。
        assert!(help.contains("on-session-start"), "{help}");
        let launcher = super::Cli::command()
            .find_subcommand("on-session-start")
            .expect("on-session-start")
            .clone()
            .render_long_help()
            .to_string();
        assert!(launcher.contains("SessionStart hook"), "{launcher}");
        assert!(launcher.contains("Always exits 0"), "{launcher}");
        assert!(launcher.contains("do not run it manually"), "{launcher}");
    }

    // -----------------------------------------------------------------------
    // on-session-start（跟随启动拉起器，issue #197 / ADR-0021）
    // -----------------------------------------------------------------------

    /// 拉起器用例的启动环境：探针取值由用例给定，记下被调用的探针与交给执行器的拉起计划。
    /// 执行器是假的，用例绝不真的拉起 GUI。
    struct FakeLaunchEnv {
        platform_supported: bool,
        interactive: bool,
        gui_running: bool,
        cli_exe: std::path::PathBuf,
        launch_error: Option<std::io::ErrorKind>,
        probes: std::cell::RefCell<Vec<&'static str>>,
        plans: std::cell::RefCell<Vec<super::LaunchPlan>>,
    }

    impl FakeLaunchEnv {
        /// Windows 桌面交互会话、GUI 没在运行、执行器成功；CLI 位于 `bin`。
        fn desktop(bin: &std::path::Path) -> Self {
            Self {
                platform_supported: true,
                interactive: true,
                gui_running: false,
                cli_exe: bin.join("kimi-switch-cli.exe"),
                launch_error: None,
                probes: Default::default(),
                plans: Default::default(),
            }
        }
    }

    impl super::LaunchEnv for FakeLaunchEnv {
        fn platform_supported(&self) -> bool {
            self.probes.borrow_mut().push("platform_supported");
            self.platform_supported
        }

        fn interactive_session(&self) -> bool {
            self.probes.borrow_mut().push("interactive_session");
            self.interactive
        }

        fn gui_running(&self) -> bool {
            self.probes.borrow_mut().push("gui_running");
            self.gui_running
        }

        fn cli_exe(&self) -> std::io::Result<std::path::PathBuf> {
            self.probes.borrow_mut().push("cli_exe");
            Ok(self.cli_exe.clone())
        }

        fn launch(&self, plan: &super::LaunchPlan) -> std::io::Result<()> {
            self.plans.borrow_mut().push(plan.clone());
            match self.launch_error {
                Some(kind) => Err(kind.into()),
                None => Ok(()),
            }
        }
    }

    fn auto_swap_enabled(enabled: bool) -> kimi_switch_core::settings::AutoSwap {
        kimi_switch_core::settings::AutoSwap {
            enabled,
            ..kimi_switch_core::settings::AutoSwap::default()
        }
    }

    /// SessionStart 载荷的真实形态：官方 hooks 文档的公共字段（hook_event_name、session_id、
    /// session_title、client_type、cwd）加 SessionStart 专有的 source、model、profile。
    /// 会话标题与工作目录是用户内容。
    fn session_start_event(source: &str) -> String {
        serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "7f3c9a2e-1b4d-4e5f-8a6b-0c1d2e3f4a5b",
            "session_title": "修一下 README 里的安装步骤",
            "client_type": "kimi_code_cli",
            "cwd": r"D:\work\secret-project",
            "source": source,
            "model": "kimi-k2",
            "profile": "default",
        })
        .to_string()
    }

    /// 审计日志里的 on_session_start 留痕。
    fn session_traces(audit: &super::AuditLog) -> Vec<AuditEvent> {
        std::fs::read_to_string(audit.path())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<AuditEvent>(line).ok())
            .filter(|event| event.action == "on_session_start")
            .collect()
    }

    /// 拉起：桌面交互会话里 GUI 没在运行时，拉起 CLI 同目录的 GUI，带后台启动参数；工作目录
    /// 是程序目录（不占住会话的项目目录），脱离 hook 的进程树，不继承 hook 的标准输入输出；
    /// 审计恰好一行 decision=launched。
    #[test]
    fn session_start_launches_gui_in_background() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin dir");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("kimi-switch.exe"), "").unwrap();
        let env = FakeLaunchEnv::desktop(&bin);
        let audit = super::AuditLog::new(tmp.path().join("audit.log"));

        super::run_on_session_start_with(
            &audit,
            &session_start_event("startup"),
            &auto_swap_enabled(true),
            &env,
        );

        assert_eq!(
            *env.plans.borrow(),
            [super::LaunchPlan {
                program: bin.join("kimi-switch.exe"),
                args: vec!["--background"],
                working_dir: bin.clone(),
                detach: true,
                inherit_handles: false,
            }]
        );
        let traces = session_traces(&audit);
        assert_eq!(
            traces.len(),
            1,
            "每个 SessionStart 恰好一行留痕：{traces:?}"
        );
        assert_eq!(traces[0].result, "ok");
        assert_eq!(
            traces[0].detail.as_deref(),
            Some("hook_event=SessionStart source=startup client_type=kimi_code_cli decision=launched")
        );
    }

    /// 判定顺序：总开关关闭 → 非 Windows 平台 → 非交互会话 → 本会话已有 GUI → 拉起；GUI 程序
    /// 缺失或执行器失败记 launch_failed（result=error）。每个分支审计恰好一行，只有走到拉起的
    /// 分支才把计划交给执行器。
    #[test]
    fn session_start_decision_order_traces_each_branch() {
        struct Case {
            name: &'static str,
            enabled: bool,
            env: fn(&mut FakeLaunchEnv),
            gui_exists: bool,
            decision: &'static str,
            result: &'static str,
            launches: usize,
        }
        let cases = [
            Case {
                name: "总开关关闭",
                enabled: false,
                env: |_| {},
                gui_exists: true,
                decision: "disabled",
                result: "ok",
                launches: 0,
            },
            Case {
                name: "非 Windows 平台",
                enabled: true,
                env: |env| env.platform_supported = false,
                gui_exists: true,
                decision: "unsupported_platform",
                result: "ok",
                launches: 0,
            },
            Case {
                name: "非交互会话（SSH、服务）",
                enabled: true,
                env: |env| env.interactive = false,
                gui_exists: true,
                decision: "non_interactive",
                result: "ok",
                launches: 0,
            },
            Case {
                name: "本会话已有 GUI",
                enabled: true,
                env: |env| env.gui_running = true,
                gui_exists: true,
                decision: "already_running",
                result: "ok",
                launches: 0,
            },
            Case {
                name: "GUI 程序不在 CLI 同目录",
                enabled: true,
                env: |_| {},
                gui_exists: false,
                decision: "launch_failed",
                result: "error",
                launches: 0,
            },
            Case {
                name: "执行器失败",
                enabled: true,
                env: |env| env.launch_error = Some(std::io::ErrorKind::PermissionDenied),
                gui_exists: true,
                decision: "launch_failed",
                result: "error",
                launches: 1,
            },
            Case {
                name: "拉起",
                enabled: true,
                env: |_| {},
                gui_exists: true,
                decision: "launched",
                result: "ok",
                launches: 1,
            },
        ];
        for case in cases {
            let tmp = tempfile::tempdir().unwrap();
            let bin = tmp.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            if case.gui_exists {
                std::fs::write(bin.join("kimi-switch.exe"), "").unwrap();
            }
            let mut env = FakeLaunchEnv::desktop(&bin);
            (case.env)(&mut env);
            let audit = super::AuditLog::new(tmp.path().join("audit.log"));

            super::run_on_session_start_with(
                &audit,
                &session_start_event("resume"),
                &auto_swap_enabled(case.enabled),
                &env,
            );

            let traces = session_traces(&audit);
            assert_eq!(traces.len(), 1, "{}：{traces:?}", case.name);
            assert_eq!(traces[0].result, case.result, "{}", case.name);
            let expected = format!(
                "hook_event=SessionStart source=resume client_type=kimi_code_cli decision={}",
                case.decision
            );
            assert_eq!(
                traces[0].detail.as_deref(),
                Some(expected.as_str()),
                "{}",
                case.name
            );
            assert_eq!(env.plans.borrow().len(), case.launches, "{}", case.name);
        }
    }

    /// 总开关关闭时不调用任何探针，也不调用执行器，只留一行 decision=disabled。
    #[test]
    fn session_start_disabled_probes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let env = FakeLaunchEnv::desktop(tmp.path());
        let audit = super::AuditLog::new(tmp.path().join("audit.log"));

        super::run_on_session_start_with(
            &audit,
            &session_start_event("startup"),
            &auto_swap_enabled(false),
            &env,
        );

        assert!(env.probes.borrow().is_empty(), "{:?}", env.probes.borrow());
        assert!(env.plans.borrow().is_empty());
        assert_eq!(session_traces(&audit).len(), 1);
    }

    /// stdin 畸形或缺字段时照常按判定顺序拉起，读不到的字段记 `-`；字段清洗与被动留痕的
    /// error_type 同口径（只留 `[A-Za-z0-9_.:-]`，至多 64 字符）。
    #[test]
    fn session_start_malformed_stdin_still_launches() {
        let long = "x".repeat(80);
        let long_payload = format!(r#"{{"source":"{long}"}}"#);
        let cases = [
            (
                "not json at all",
                "hook_event=- source=- client_type=-".to_string(),
            ),
            ("", "hook_event=- source=- client_type=-".to_string()),
            ("[1, 2]", "hook_event=- source=- client_type=-".to_string()),
            (
                r#"{"hook_event_name":42,"source":"resume"}"#,
                "hook_event=- source=resume client_type=-".to_string(),
            ),
            (
                r#"{"hook_event_name":"SessionStart","client_type":"kimi code desktop/1.2.0 (win32)"}"#,
                "hook_event=SessionStart source=- client_type=kimi_code_desktop_1.2.0__win32_"
                    .to_string(),
            ),
            (
                long_payload.as_str(),
                format!("hook_event=- source={} client_type=-", "x".repeat(64)),
            ),
        ];
        for (raw, fields) in cases {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("kimi-switch.exe"), "").unwrap();
            let env = FakeLaunchEnv::desktop(tmp.path());
            let audit = super::AuditLog::new(tmp.path().join("audit.log"));

            super::run_on_session_start_with(&audit, raw, &auto_swap_enabled(true), &env);

            assert_eq!(env.plans.borrow().len(), 1, "stdin 畸形也照常拉起：{raw}");
            let traces = session_traces(&audit);
            assert_eq!(traces.len(), 1, "{raw}");
            let expected = format!("{fields} decision=launched");
            assert_eq!(
                traces[0].detail.as_deref(),
                Some(expected.as_str()),
                "{raw}"
            );
        }
    }

    /// 留痕不含用户内容：真实形态载荷的会话标题、工作目录与 stdin 原文都不进审计日志。
    #[test]
    fn session_start_trace_omits_user_content() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("kimi-switch.exe"), "").unwrap();
        let env = FakeLaunchEnv::desktop(tmp.path());
        let audit = super::AuditLog::new(tmp.path().join("audit.log"));
        let raw = session_start_event("resume");

        super::run_on_session_start_with(&audit, &raw, &auto_swap_enabled(true), &env);

        assert_eq!(
            session_traces(&audit)[0].detail.as_deref(),
            Some(
                "hook_event=SessionStart source=resume client_type=kimi_code_cli decision=launched"
            )
        );
        let log = std::fs::read_to_string(audit.path()).unwrap();
        assert!(!log.contains(&raw), "{log}");
        for fragment in ["secret-project", "修一下", "README", "7f3c9a2e", "kimi-k2"] {
            assert!(
                !log.contains(fragment),
                "留痕不得含载荷内容「{fragment}」：{log}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 启动一致性自检（issue #155，spec #152 决策 8）
    // -----------------------------------------------------------------------

    use kimi_switch_core::StartupCheckOutcome;

    /// 测试用 base64url（无填充），与 `kimi_files::base64url_decode` 互逆。
    fn base64url(bytes: &[u8]) -> String {
        const CHARS: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(CHARS[(n >> 18) as usize & 63] as char);
            out.push(CHARS[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                out.push(CHARS[(n >> 6) as usize & 63] as char);
            }
            if chunk.len() > 2 {
                out.push(CHARS[n as usize & 63] as char);
            }
        }
        out
    }

    /// 带 JWT 的合成 live blob。Kimi 身份辨认只认 access_token JWT 的 `user_id`
    /// claim（引擎 `parse_metadata` → `official_identity`），与 #154 回读校验同一套；
    /// 合成 token 无任何真实凭证，全程零网络。
    fn jwt_user_blob(id: &str) -> String {
        let payload = base64url(format!(r#"{{"user_id":"{id}"}}"#).as_bytes());
        format!(r#"{{"access_token":"eyJhbGciOiJFUzI1NiJ9.{payload}.sig"}}"#)
    }

    /// 一致 / 无法判定：零可见输出（None 即调用方什么都不打印）。
    #[test]
    fn startup_check_notice_consistent_is_none() {
        assert!(super::startup_check_notice(&StartupCheckOutcome::Consistent).is_none());
    }

    /// 纠正与工具外身份两条提示的文案要点（简洁中文；不含任何凭证）。
    #[test]
    fn startup_check_notice_texts_cover_correction_and_import_hint() {
        let corrected = super::startup_check_notice(&StartupCheckOutcome::CorrectedActive {
            account: kimi_switch_core::AccountId("bob".into()),
            previous: Some(kimi_switch_core::AccountId("alice".into())),
        })
        .expect("纠正应有提示");
        assert!(corrected.contains("纠正"), "{corrected}");
        assert!(corrected.contains("kimi/bob"), "{corrected}");
        assert!(corrected.contains("alice"), "{corrected}");

        let untracked = super::startup_check_notice(&StartupCheckOutcome::UntrackedLiveIdentity {
            live: kimi_switch_core::AccountId("carol".into()),
        })
        .expect("工具外身份应有导入提示");
        assert!(untracked.contains("工具外身份"), "{untracked}");
        assert!(untracked.contains("kimi-switch login kimi"), "{untracked}");
        assert!(untracked.contains("导入"), "{untracked}");
    }

    /// 端到端：live 与注册表 active 不一致（用户在工具外重新登回了库内旧账号）
    /// → 真实 live 身份辨认（引擎 parse_metadata → official_identity）+ 自动纠正
    /// + 审计留痕。身份经 blob 的 label 字段辨认（与被动切换测试同一零网络夹具）。
    #[tokio::test]
    async fn startup_check_corrects_stale_marker_via_real_live_identity() {
        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        // 用户在工具外把 Kimi Code 重新登到库内的 bob：live 变了，注册表还标着 alice。
        std::fs::write(
            env.kimi_home.join("credentials").join("kimi-code.json"),
            jwt_user_blob("bob"),
        )
        .unwrap();

        let outcome = super::startup_check(&ctx);

        assert_eq!(
            outcome,
            StartupCheckOutcome::CorrectedActive {
                account: kimi_switch_core::AccountId("bob".into()),
                previous: Some(kimi_switch_core::AccountId("alice".into())),
            }
        );
        let actives: Vec<_> = ctx
            .registry
            .load()
            .unwrap()
            .into_iter()
            .filter(|a| a.active)
            .map(|a| a.id.0)
            .collect();
        assert_eq!(actives, vec!["bob".to_string()]);
        let audits: Vec<AuditEvent> = std::fs::read_to_string(ctx.audit.path())
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<AuditEvent>(line).ok())
            .filter(|event| event.action == "startup_check")
            .collect();
        assert_eq!(audits.len(), 1, "纠正恰留一条审计");
        assert_eq!(audits[0].account.as_deref(), Some("bob"));
    }

    /// 端到端：live 身份不在账号库（工具外登录了新身份）→ 清除 active 标记 +
    /// 审计留痕 + CLI 出现导入提示。
    #[tokio::test]
    async fn startup_check_untracked_live_clears_marker_and_prompts_import() {
        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        std::fs::write(
            env.kimi_home.join("credentials").join("kimi-code.json"),
            jwt_user_blob("carol"),
        )
        .unwrap();

        let outcome = super::startup_check(&ctx);

        assert_eq!(
            outcome,
            StartupCheckOutcome::UntrackedLiveIdentity {
                live: kimi_switch_core::AccountId("carol".into()),
            }
        );
        let actives: Vec<_> = ctx
            .registry
            .load()
            .unwrap()
            .into_iter()
            .filter(|a| a.active)
            .collect();
        assert!(actives.is_empty(), "active 标记应被清除: {actives:?}");
        let audits: Vec<AuditEvent> = std::fs::read_to_string(ctx.audit.path())
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<AuditEvent>(line).ok())
            .filter(|event| event.action == "startup_check")
            .collect();
        assert_eq!(audits.len(), 1);
        assert!(
            audits[0]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("carol"),
            "审计应记录 live 身份: {:?}",
            audits[0].detail
        );
        assert!(
            super::startup_check_notice(&outcome)
                .expect("应有导入提示")
                .contains("导入"),
            "CLI 导入提示必须出现"
        );
    }

    /// 端到端：状态一致 → 零副作用（不新增审计）+ 零可见输出（提示为 None）。
    #[tokio::test]
    async fn startup_check_consistent_is_zero_side_effect_and_zero_output() {
        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        // 用可辨认的 JWT 身份落在 active 账号上，钉住「真一致」而非「无法辨认被跳过」。
        std::fs::write(
            env.kimi_home.join("credentials").join("kimi-code.json"),
            jwt_user_blob("alice"),
        )
        .unwrap();

        let outcome = super::startup_check(&ctx);

        assert_eq!(outcome, StartupCheckOutcome::Consistent);
        assert!(
            !ctx.audit.path().exists(),
            "一致时不得新增审计记录（文件都不应创建）"
        );
        assert!(
            super::startup_check_notice(&outcome).is_none(),
            "一致时不得有任何可见输出"
        );
    }

    // -----------------------------------------------------------------------
    // swap 死卡警告（issue #156，spec #152 决策 9）
    // -----------------------------------------------------------------------

    /// 种拒止指纹文件：写入给定 refresh token（合成占位值）的 SHA-256 指纹。
    fn seed_dead_refresh_fingerprints(env: &passive_env::IsolatedHomes, refresh_tokens: &[&str]) {
        let dir = env.kimi_home.join("credentials");
        std::fs::create_dir_all(&dir).unwrap();
        let body = refresh_tokens
            .iter()
            .map(|token| kimi_switch_core::refresh_fingerprint(token))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join(".kimi-switch-dead-refresh"), format!("{body}\n")).unwrap();
    }

    /// 端到端：切到本地已确认失效（invalid_grant 指纹命中）的卡 → 切换照常成功
    /// （live 翻转、stdout 成功行不变），stderr 补一行中文警告且只此一行。
    #[tokio::test]
    async fn swap_dead_credential_target_still_swaps_and_warns() {
        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);
        // bob 的 blob 带 refresh_token（合成占位值），其指纹进拒止文件。
        ctx.store
            .set(
                "kimi",
                "bob",
                "blob",
                r#"{"label":"bob","access_token":"tok-bob","refresh_token":"refresh-bob"}"#,
            )
            .unwrap();
        seed_dead_refresh_fingerprints(&env, &["refresh-bob"]);

        let bob = super::resolve_account(&ctx, "bob").unwrap();
        assert_eq!(
            super::dead_credential_warning(&bob),
            Some(super::DEAD_CREDENTIAL_WARNING),
            "指纹命中应给出警告行"
        );

        // 切换照常成功：警告机制不得影响切换结果本身。
        super::swap(&ctx, Some("bob")).await.unwrap();
        assert!(
            live_blob(&env).contains("tok-bob"),
            "live 应翻转到 bob（合成占位 token，非真实凭证）"
        );

        // stderr 警告行（writer seam 与 emit_status_output_to 同款）。
        let mut stderr = Vec::new();
        super::write_dead_credential_warning(
            super::dead_credential_warning(&bob).as_deref(),
            &mut stderr,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "该账号凭证已失效，建议重新授权\n"
        );
    }

    /// 端到端负例：切到健康卡（指纹文件缺失 → 信号未知 fail-open）→ 零警告输出。
    #[tokio::test]
    async fn swap_healthy_credential_target_has_no_warning() {
        let env = passive_env::isolate();
        let ctx = seed_passive_fixture(&env, &[("alice", true), ("bob", false)]);

        super::swap(&ctx, Some("bob")).await.unwrap();
        assert!(live_blob(&env).contains("tok-bob"), "切换本身必须照常成功");

        let bob = super::resolve_account(&ctx, "bob").unwrap();
        assert!(
            super::dead_credential_warning(&bob).is_none(),
            "健康卡（含指纹文件缺失的 fail-open）不得有警告"
        );
        let mut stderr = Vec::new();
        super::write_dead_credential_warning(None, &mut stderr).unwrap();
        assert!(stderr.is_empty(), "无警告 → stderr 零输出");
    }
}
