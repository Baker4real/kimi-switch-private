//! kimi-switch（Kimi Code 裁剪版）CLI 入口。
//!
//! 命令面（刻意保持最小）：
//! - `kimi-switch`              — 默认入口：同步本地激活账号，列出 Kimi 账号 + 5h/7d 额度。
//! - `kimi-switch login kimi`   — 导入当前本机 Kimi Code 已登录账号
//!   （`~/.kimi-code/credentials/kimi-code.json`）。
//! - `kimi-switch swap <id|N>`  — 切换激活账号。原子写 + 快照回滚，不依赖网络/quota。
//!   无参数时只打印编号列表，不做切换。
//! - `kimi-switch rm <id|N>`    — 删除账号（registry + 凭证仓库 + 墓碑）。
//!
//! `<id>` 是账号 id（Kimi user_id）、label 或 `kimi/<id>`；`<N>` 是默认入口
//! 显示的编号（1 起）。

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::{
    settings, Account, AccountRegistry, AuditEvent, AuditLog, CredentialStore, FileStore,
    KeyringStore, Provider, Quota, QuotaState, QuotaWindow, RemovedAccounts,
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
        /// Provider to log in: only `kimi` is supported.
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

/// 把用户传入的引用解析到具体账号：纯数字 N 取显示顺序第 N 个；
/// 否则按 id / label / `kimi/<id>` 走 `find_unique`。
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
        Some(Cmd::Login { provider }) => login(&ctx, &provider),
        Some(Cmd::Swap { id }) => swap(&ctx, id.as_deref()).await,
        Some(Cmd::Rm { id }) => rm(&ctx, &id).await,
    }
}

// ---------------------------------------------------------------------------
// 默认入口：账号列表 + 额度
// ---------------------------------------------------------------------------

async fn status(ctx: &AppContext) -> Result<()> {
    // 1. 自动导入/对齐本地激活账号（`rm` 过的账号有墓碑，跳过）。
    sync_local_active(ctx);

    // 2. 列出账号。
    let accounts = ctx.list_ordered()?;
    if accounts.is_empty() {
        println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
        return Ok(());
    }

    // 3. 额度快照（节流 / 退避 / 并发 / 陈旧回退收口在 core）。
    let cache_path = AppPaths::resolve()?.quota_cache_file();
    let ids: Vec<_> = accounts.iter().map(|account| account.id.clone()).collect();
    let snapshots =
        kimi_switch_core::quota_snapshot::collect(ctx.kimi.clone(), "kimi", &ids, &cache_path)
            .await;

    // 4. 渲染。CLI 仍不打印会员等级；节流命中不标 stale，失败回落标 `(stale)`。
    println!("kimi");
    for (idx, (account, snapshot)) in accounts.iter().zip(snapshots.iter()).enumerate() {
        let n = idx + 1;
        let star = if account.active { "*" } else { " " };
        let quota_text = match &snapshot.state {
            QuotaState::Failed(error) => format!("quota: {error}"),
            QuotaState::Ready(quotas) | QuotaState::Cached(quotas) if snapshot.stale => {
                format!("{} (stale)", format_quotas(quotas))
            }
            QuotaState::Ready(quotas) | QuotaState::Cached(quotas) => format_quotas(quotas),
        };
        println!("  {star} {n:>2}  {:<24} {quota_text}", account.id);
    }
    Ok(())
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
// login kimi
// ---------------------------------------------------------------------------

fn login(ctx: &AppContext, provider: &str) -> Result<()> {
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
        other => anyhow::bail!("unknown provider: {other} (only `kimi` is supported)"),
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
