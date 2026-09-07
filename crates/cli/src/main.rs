//! kimi-switch（Kimi Code 裁剪版）CLI 入口。
//!
//! 命令面（刻意保持最小）：
//! - `kimi-switch`              — 默认入口：同步本地激活账号，列出 Kimi 账号 + 5h/7d 额度。
//!   `[auto_swap] enabled = true` 时，额度收集完成后若达阈值则自动切换并追加一行英文结果。
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
//!
//! `<id>` 是账号 id（Kimi user_id）、label 或 `kimi/<id>`；`<N>` 是默认入口
//! 显示的编号（1 起）。

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::{
    decide_auto_swap, membership_expiry_from_extra, prettify_membership, read_last_auto_swap_at,
    settings, write_last_auto_swap_at, Account, AccountId, AccountRegistry, AuditEvent, AuditLog,
    AutoSwapDecision, AutoSwapSkipReason, AutoSwapTarget, CredentialStore, FileStore, KeyringStore,
    Provider, Quota, QuotaSnapshot, QuotaState, QuotaWindow, RemovedAccounts,
};
use kimi_switch_kimi::KimiProvider;

#[derive(Parser)]
#[command(
    name = "kimi-switch",
    version,
    about = "Manage and swap between multiple Kimi Code accounts.",
    long_about = "Run `kimi-switch` with no arguments to list Kimi accounts and their quota. \
                  Use `login kimi` / `swap` / `rm` for explicit actions."
)]
struct Cli {
    /// Log level (equivalent to RUST_LOG).
    #[arg(long, global = true, default_value = "warn")]
    log: String,

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
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cli.log.clone()));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // 启动时加载 config.toml（缺失 / 解析失败时沿用默认值 + warn）。
    if let Err(e) = settings::reload_from_file() {
        tracing::warn!(err = %e, "load config failed; using built-in defaults");
    }

    let ctx = AppContext::build()?;

    match cli.cmd {
        None => status(&ctx).await,
        Some(Cmd::Login { provider }) => login(&ctx, &provider).await,
        Some(Cmd::Swap { id }) => swap(&ctx, id.as_deref()).await,
        Some(Cmd::Rm { id }) => rm(&ctx, &id).await,
        Some(Cmd::BindWeb { id }) => bind_web(&ctx, &id).await,
        Some(Cmd::Config) => config(),
    }
}

// ---------------------------------------------------------------------------
// 默认入口：账号列表 + 额度
// ---------------------------------------------------------------------------

async fn status(ctx: &AppContext) -> Result<()> {
    // 1. 自动导入/对齐本地激活账号（`rm` 过的账号有墓碑，跳过）。
    sync_local_active(ctx);

    // 2. 列出账号。
    let mut accounts = ctx.list_ordered()?;
    if accounts.is_empty() {
        println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
        return Ok(());
    }

    // 3. 额度快照（节流 / 退避 / 并发 / 陈旧回退收口在 core）。
    let cache_path = AppPaths::resolve()?.quota_cache_file();
    let ids: Vec<_> = accounts.iter().map(|account| account.id.clone()).collect();
    let snapshots = kimi_switch_core::quota_snapshot::collect(
        ctx.kimi.clone(),
        "kimi",
        &ids,
        &cache_path,
        kimi_switch_core::QuotaCollectReason::Scheduled,
    )
    .await;

    // 4. 自动切换：开关关闭则短路（零判定、不读冷却文件）。触发时走既有 activate。
    let auto_swap = maybe_run_status_auto_swap(ctx, &accounts, &snapshots).await;
    if auto_swap.swapped {
        sync_local_active(ctx);
        accounts = ctx.list_ordered()?;
    }

    // 5. 渲染。账号 id 旁带档位名与会员到期（extra 有 → `有效期至 <日期>`；
    //    无 → 绑定弱提示）；节流命中不标 stale，失败回落标 `(stale)`。
    println!("kimi");
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
        println!(
            "  {star} {n:>2}  {:<24}{plan_text} {quota_text}",
            account.id
        );
    }
    if let Some(line) = auto_swap.line {
        println!("{line}");
    }
    Ok(())
}

/// CLI status 自动切换的可观察结果：列表后追加的一行，以及是否已成功切换。
struct AutoSwapStatusEffect {
    line: Option<String>,
    swapped: bool,
}

impl AutoSwapStatusEffect {
    fn none() -> Self {
        Self {
            line: None,
            swapped: false,
        }
    }
}

async fn maybe_run_status_auto_swap(
    ctx: &AppContext,
    accounts: &[Account],
    snapshots: &[QuotaSnapshot],
) -> AutoSwapStatusEffect {
    let settings = settings::current();
    if !settings.auto_swap.enabled {
        return AutoSwapStatusEffect::none();
    }
    let Ok(paths) = AppPaths::resolve() else {
        return AutoSwapStatusEffect::none();
    };
    let state_path = paths.auto_swap_state_file();
    let last = read_last_auto_swap_at(&state_path);
    let kimi = ctx.kimi.clone();
    apply_status_auto_swap(
        &settings.auto_swap,
        accounts,
        snapshots,
        Utc::now(),
        last,
        move |id| async move { kimi.activate(&id).await },
        &ctx.audit,
        &state_path,
    )
    .await
}

/// collect 完成后的自动切换编排。开关关闭立即返回；activate 失败不写冷却。
async fn apply_status_auto_swap<F, Fut>(
    auto_swap: &kimi_switch_core::settings::AutoSwap,
    accounts: &[Account],
    snapshots: &[QuotaSnapshot],
    now: DateTime<Utc>,
    last_auto_swap_at: Option<DateTime<Utc>>,
    activate: F,
    audit: &AuditLog,
    state_path: &Path,
) -> AutoSwapStatusEffect
where
    F: FnOnce(AccountId) -> Fut,
    Fut: std::future::Future<Output = kimi_switch_core::Result<()>>,
{
    if !auto_swap.enabled {
        return AutoSwapStatusEffect::none();
    }
    match decide_auto_swap(accounts, snapshots, auto_swap, now, last_auto_swap_at) {
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
            AutoSwapStatusEffect {
                line: Some(auto_swap_no_candidate_line().to_string()),
                swapped: false,
            }
        }
        AutoSwapDecision::Stay { .. } => AutoSwapStatusEffect::none(),
        AutoSwapDecision::Swap(target) => match activate(target.to.clone()).await {
            Ok(()) => {
                write_last_auto_swap_at(state_path, Utc::now());
                audit.append(
                    AuditEvent::ok("auto_swap", "kimi", Some(target.to.0.as_str()))
                        .with_detail(auto_swap_audit_detail(&target)),
                );
                AutoSwapStatusEffect {
                    line: Some(auto_swap_success_line(&target)),
                    swapped: true,
                }
            }
            Err(err) => {
                audit.append(AuditEvent::err(
                    "auto_swap",
                    "kimi",
                    Some(target.to.0.as_str()),
                    &err.to_string(),
                ));
                AutoSwapStatusEffect {
                    line: Some(auto_swap_failed_line(&target.to, &err)),
                    swapped: false,
                }
            }
        },
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
    format!(
        "auto_swap kimi/{} → kimi/{} ({} {}%)",
        target.from,
        target.to,
        quota_window_short(target.trigger_window),
        auto_swap_pct(target.usage_ratio)
    )
}

fn auto_swap_no_candidate_line() -> &'static str {
    "auto_swap: no eligible candidate"
}

fn auto_swap_failed_line(to: &AccountId, err: impl std::fmt::Display) -> String {
    format!("auto_swap kimi/{to} failed: {err}")
}

fn auto_swap_audit_detail(target: &AutoSwapTarget) -> String {
    format!(
        "from={} window={} used={}%",
        target.from,
        quota_window_short(target.trigger_window),
        auto_swap_pct(target.usage_ratio)
    )
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

/// 扫本地 `~/.kimi-code`；当前账号没记录过就入库（已存在则对齐 active）。墓碑命中或未登录则跳过。
fn sync_local_active(ctx: &AppContext) {
    let Ok(paths) = AppPaths::resolve() else {
        return;
    };
    let removed_path = paths.removed_file();
    kimi_switch_core::sync_local_active(
        "kimi",
        &removed_path,
        ctx.registry.as_ref(),
        || ctx.kimi.live_account_id(),
        || ctx.kimi.sync_active_metadata(None),
    );
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
                kimi_switch_kimi::cache_official_nickname_from_blob(
                    ctx.registry.as_ref(),
                    &account.id,
                    &blob,
                )
                .await;
            }
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

/// 显式切换激活账号。手动入口，不依赖网络/quota（原子写 + 快照回滚在 provider 内部）。
async fn swap(ctx: &AppContext, id_input: Option<&str>) -> Result<()> {
    let Some(input) = id_input else {
        print_listing(ctx)?;
        return Ok(());
    };

    let acc = resolve_account(ctx, input)?;
    match ctx.kimi.activate(&acc.id).await {
        Ok(()) => {
            ctx.audit
                .append(AuditEvent::ok("activate", "kimi", Some(acc.id.0.as_str())));
            println!("swap → kimi/{}", acc.id);
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
        Account, AccountId, AuditEvent, AuditLog, Quota, QuotaSnapshot, QuotaState, QuotaWindow,
    };
    use chrono::{DateTime, Utc};

    fn now() -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 9, 3, 12, 0, 0).unwrap()
    }

    fn auto_swap_on() -> kimi_switch_core::settings::AutoSwap {
        kimi_switch_core::settings::AutoSwap {
            enabled: true,
            threshold: 0.99,
            cooldown_ms: 5 * 60 * 1000,
            settle_grace_ms: 60 * 1000,
            prefer_lower_tier: false,
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
                reset_at: None,
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
            None,
            |_id| async { panic!("disabled 不得 activate") },
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
            None,
            move |id| {
                let activated = activated_cb.clone();
                async move {
                    *activated.lock().unwrap() = Some(id);
                    Ok(())
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
        let detail = event.detail.expect("success audit has detail");
        assert!(detail.contains("from=alice"), "{detail}");
        assert!(detail.contains("window=5h"), "{detail}");
        assert!(detail.contains("used=99%"), "{detail}");
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
            None,
            move |id| {
                let activated = activated_cb.clone();
                async move {
                    *activated.lock().unwrap() = Some(id);
                    Ok(())
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
            None,
            |_id| async { Err(kimi_switch_core::Error::provider("disk full")) },
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
            None,
            |_id| async { panic!("无候选不得 activate") },
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
}
