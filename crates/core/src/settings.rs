//! 运行时可热加载的数值调优配置。
//!
//! 设计：
//! - **唯一 source of truth**：`<config_dir>/config.toml`（缺失则用 [`crate::defaults`] 编译期默认值）。
//! - **热更新**：daemon 每轮循环开头 [`reload_from_file`]；CLI 启动时调一次即可（CLI 短命）。
//! - **失败兜底**：文件不存在 → 用默认值；文件存在但解析失败 → 沿用上一次成功加载的值并打 warn，
//!   防止配置 typo 把 daemon 拖挂。
//! - **访问入口**：[`current`] 返回 `Arc<Settings>`，所有调用点都从这里读，避免在长生命周期对象里
//!   缓存可能过期的副本。
//!
//! 字段命名遵循「按 domain 分组」的 TOML 表，方便 `vim ~/.config/kimi-switch/config.toml`。
//! 例：
//! ```toml
//! [auto_swap]
//! enabled = false
//! # threshold = <0.0~1.0>
//!
//! [daemon]
//! poll_interval_ms = 60000
//! idle_threshold_ms = 1800000
//! idle_poll_interval_ms = 900000
//!
//! [quota]
//! fetch_timeout_ms = 20000
//! fetch_retries = 1
//!
//! [proactive_renewal]
//! enabled = false
//! ```

use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::defaults;
use crate::error::{Error, Result};
use crate::paths::AppPaths;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub auto_swap: AutoSwap,
    pub quota: Quota,
    pub token: Token,
    pub daemon: Daemon,
    pub beta: Beta,
    pub proactive_renewal: ProactiveRenewal,
}

/// 自动切换段：CLI 默认 status 在额度收集完成后消费（issue #67）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoSwap {
    /// 自动切换总开关。false 时跳过所有自动切换决策。默认关闭。
    pub enabled: bool,
    /// 自动切换触发阈值，0.0~1.0。任一窗口 used/limit ≥ 此值即触发。
    pub threshold: f64,
    /// 两次自动切换成功之间的最短间隔（毫秒）。
    pub cooldown_ms: i64,
    /// 刚被手动选中的当前账号在此窗口内不会被自动切走（毫秒）。
    pub settle_grace_ms: i64,
    /// 合格停泊账号中优先选档位更低者。默认关闭，与现状比较链一致。
    pub prefer_lower_tier: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Quota {
    /// 展示用 Warn 阈值（百分比 0~100）。不参与切换决策。
    pub warn_pct: f64,
    /// 展示用 Exhausted 阈值（百分比 0~100）。不参与切换决策。
    pub exhausted_pct: f64,
    /// 单次 quota 查询 attempt 的超时（毫秒）。
    pub fetch_timeout_ms: u64,
    /// quota 查询失败后额外重试几次；最多 5 次，401/403 不重试。
    pub fetch_retries: u32,
    /// 首次重试前等待多久（毫秒）；后续指数退避。
    pub fetch_retry_delay_ms: u64,
    /// 同一账号两次真实 quota 查询的最小间隔（毫秒）。缓存比这更新就不打 usage 端点，
    /// 直接复用缓存值。daemon 与 CLI 共用同一缓存文件，借此把每账号请求频率压到
    /// 「每 `min_refresh_interval_ms` 最多 1 次」，避免并发查爆 usage 端点触发 429。
    pub min_refresh_interval_ms: u64,
    /// 连续查询失败时的退避上限（毫秒）。退避从 `min_refresh_interval_ms` 起按失败次数
    /// 翻倍并封顶到这里，避免查不出的账号被每轮重查、把 usage 端点限流桶打空。
    pub failure_backoff_max_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Token {
    /// token 距过期还有多少毫秒视为「需要预刷新」。
    pub refresh_slack_ms: i64,
}

/// daemon 段：serde 接受存量 `config.toml`，运行时零消费。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Daemon {
    /// 活跃时（最近 `idle_threshold_ms` 内 probe 文件被改过）的轮询周期。
    pub poll_interval_ms: u64,
    /// 多久无 probe 文件 mtime 变化判定为空闲。
    pub idle_threshold_ms: i64,
    /// 空闲时的轮询周期。
    pub idle_poll_interval_ms: u64,
}

/// Beta 段：serde 接受存量 `config.toml`，运行时零消费。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Beta {
    /// 旧版 Beta 本地 last_usage 缓存允许使用的最大年龄。仅作字段漂移兜底。
    pub usage_cache_max_age_ms: i64,
}

/// 「当前账号主动续期」段（issue #42 设置壳 + issue #44 运行时消费；ADR-0003 第 2 条）。
///
/// 开启后，Kimi 当前账号额度查询在 access token 临期时主动轮换 live 凭证；
/// 消费方每次直读 config.toml（不依赖全局缓存），GUI 设置页与 CLI `config` 同源。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProactiveRenewal {
    /// 允许在 Windows 等无官方刷新锁的平台上，为当前账号主动续期 live 凭证。
    /// 默认关闭；开启的残余风险是窄窗口内官方会话可能要求重新登录。
    pub enabled: bool,
}

impl Default for AutoSwap {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: defaults::AUTO_SWAP_THRESHOLD,
            cooldown_ms: defaults::AUTO_SWAP_COOLDOWN_MS,
            settle_grace_ms: defaults::AUTO_SWAP_SETTLE_GRACE_MS,
            prefer_lower_tier: false,
        }
    }
}

impl Default for Quota {
    fn default() -> Self {
        Self {
            warn_pct: defaults::QUOTA_WARN_PCT,
            exhausted_pct: defaults::QUOTA_EXHAUSTED_PCT,
            fetch_timeout_ms: defaults::QUOTA_FETCH_TIMEOUT_MS,
            fetch_retries: defaults::QUOTA_FETCH_RETRIES,
            fetch_retry_delay_ms: defaults::QUOTA_FETCH_RETRY_DELAY_MS,
            min_refresh_interval_ms: defaults::QUOTA_MIN_REFRESH_INTERVAL_MS,
            failure_backoff_max_ms: defaults::QUOTA_FAILURE_BACKOFF_MAX_MS,
        }
    }
}

impl Default for Token {
    fn default() -> Self {
        Self {
            refresh_slack_ms: defaults::REFRESH_SLACK_MS,
        }
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self {
            poll_interval_ms: defaults::DAEMON_POLL_INTERVAL_MS,
            idle_threshold_ms: defaults::DAEMON_IDLE_THRESHOLD_MS,
            idle_poll_interval_ms: defaults::DAEMON_IDLE_POLL_INTERVAL_MS,
        }
    }
}

impl Default for Beta {
    fn default() -> Self {
        Self {
            usage_cache_max_age_ms: defaults::BETA_USAGE_CACHE_MAX_AGE_MS,
        }
    }
}

impl Default for ProactiveRenewal {
    fn default() -> Self {
        Self {
            enabled: defaults::PROACTIVE_RENEWAL_ENABLED,
        }
    }
}

// ----- 全局当前值 ------------------------------------------------------------

static CURRENT: OnceLock<RwLock<Arc<Settings>>> = OnceLock::new();

fn cell() -> &'static RwLock<Arc<Settings>> {
    CURRENT.get_or_init(|| RwLock::new(Arc::new(Settings::default())))
}

/// 读取当前生效的配置快照。
///
/// 各模块**每次访问**都调一次，不要把返回值长期缓存——daemon 会在循环开头 [`reload_from_file`]，
/// 长期缓存会拿到过期值。
pub fn current() -> Arc<Settings> {
    cell().read().expect("settings rwlock poisoned").clone()
}

/// 重新从默认路径 `<config_dir>/config.toml` 读取配置。
///
/// 行为：
/// - 文件不存在 → 写入 [`Settings::default`] 并返回。
/// - 解析成功 → 替换全局当前值并返回新快照。
/// - 解析失败 → 保留旧值，返回 `Err`（调用方一般 warn 一行就继续）。
pub fn reload_from_file() -> Result<Arc<Settings>> {
    let path = AppPaths::resolve()?.config_file();
    let parsed = load_from(&path)?;
    install(parsed.clone());
    Ok(parsed)
}

/// 解析指定路径的配置；文件不存在视为默认值。**不**修改全局状态。
pub fn load_from(path: &Path) -> Result<Arc<Settings>> {
    if !path.exists() {
        return Ok(Arc::new(Settings::default()));
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("read {}: {e}", path.display())))?;
    if raw.trim().is_empty() {
        return Ok(Arc::new(Settings::default()));
    }
    let parsed: Settings = toml::from_str(&raw)
        .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))?;
    Ok(Arc::new(parsed))
}

/// 直接替换全局当前值。测试与 CLI doctor 使用。
pub fn install(settings: Arc<Settings>) {
    *cell().write().expect("settings rwlock poisoned") = settings;
}

/// 把「当前账号主动续期」开关写入 `<config_dir>/config.toml`，返回新快照。
///
/// 只落盘，不改全局当前值：CLI 启动时 [`reload_from_file`] 自然读到；
/// GUI 每次快照经 [`load_from`] 直读文件，也不依赖全局缓存。
pub fn set_proactive_renewal_enabled(enabled: bool) -> Result<Arc<Settings>> {
    let path = AppPaths::resolve()?.config_file();
    upsert_proactive_renewal(&path, enabled)
}

/// 读-改-写指定配置文件里的 `proactive_renewal.enabled`。
///
/// - 用 toml_edit 只动一个键，保留用户手写的注释、顺序与其它字段；
/// - 落盘前先确认成品仍是一份合法 [`Settings`]（存量文件有拼错的键时直接报错、不动盘）；
/// - tmp + rename 原子写入，与 registry / 凭证仓库同一写序；
/// - 文件缺失视为空文档。
fn upsert_proactive_renewal(path: &Path, enabled: bool) -> Result<Arc<Settings>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Config(format!("read {}: {e}", path.display()))),
    };
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))?;
    if matches!(doc.get("proactive_renewal"), Some(item) if item.as_table_like().is_none()) {
        // 存量同名键不是表（手误，反正也过不了 Settings 解析）；整体换成新表再落键。
        doc["proactive_renewal"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["proactive_renewal"]["enabled"] = toml_edit::value(enabled);
    let rendered = doc.to_string();
    let parsed: Settings = toml::from_str(&rendered)?;
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, rendered)?;
    std::fs::rename(&tmp, path)?;
    Ok(Arc::new(parsed))
}

/// 把「额度耗尽自动切换」开关写入 `<config_dir>/config.toml`，返回新快照。
///
/// 只落盘，不改全局当前值：CLI 启动时 [`reload_from_file`] 自然读到；
/// GUI 每次快照经 [`load_from`] 直读文件，也不依赖全局缓存。
pub fn set_auto_swap_enabled(enabled: bool) -> Result<Arc<Settings>> {
    let path = AppPaths::resolve()?.config_file();
    upsert_auto_swap(&path, enabled)
}

/// 把「优先选更低档位」写入 `<config_dir>/config.toml`，返回新快照。
///
/// 只落盘，不改全局当前值；写序与 [`set_auto_swap_enabled`] 相同。
pub fn set_auto_swap_prefer_lower_tier(prefer: bool) -> Result<Arc<Settings>> {
    let path = AppPaths::resolve()?.config_file();
    upsert_auto_swap_prefer_lower_tier(&path, prefer)
}

/// 读-改-写指定配置文件里的 `auto_swap.enabled`。
fn upsert_auto_swap(path: &Path, enabled: bool) -> Result<Arc<Settings>> {
    upsert_auto_swap_bool(path, "enabled", enabled)
}

/// 读-改-写指定配置文件里的 `auto_swap.prefer_lower_tier`。
fn upsert_auto_swap_prefer_lower_tier(path: &Path, prefer: bool) -> Result<Arc<Settings>> {
    upsert_auto_swap_bool(path, "prefer_lower_tier", prefer)
}

/// 读-改-写指定配置文件里的一个 `auto_swap` 布尔键。
///
/// - 用 toml_edit 只动一个键，保留用户手写的注释、顺序与其它字段；
/// - 落盘前先确认成品仍是一份合法 [`Settings`]（存量文件有拼错的键时直接报错、不动盘）；
/// - tmp + rename 原子写入，与 registry / 凭证仓库同一写序；
/// - 文件缺失视为空文档。
fn upsert_auto_swap_bool(path: &Path, key: &str, value: bool) -> Result<Arc<Settings>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Config(format!("read {}: {e}", path.display()))),
    };
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))?;
    if matches!(doc.get("auto_swap"), Some(item) if item.as_table_like().is_none()) {
        // 存量同名键不是表（手误，反正也过不了 Settings 解析）；整体换成新表再落键。
        doc["auto_swap"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["auto_swap"][key] = toml_edit::value(value);
    let rendered = doc.to_string();
    let parsed: Settings = toml::from_str(&rendered)?;
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, rendered)?;
    std::fs::rename(&tmp, path)?;
    Ok(Arc::new(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let s = load_from(&path).unwrap();
        assert_eq!(*s, Settings::default());
    }

    #[test]
    fn partial_file_keeps_section_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[auto_swap]
threshold = 0.5
"#,
        )
        .unwrap();
        let s = load_from(&path).unwrap();
        assert_eq!(s.auto_swap.threshold, 0.5);
        // 其他字段保持默认（含开关默认关闭）
        assert!(!s.auto_swap.enabled);
        assert_eq!(s.auto_swap.cooldown_ms, defaults::AUTO_SWAP_COOLDOWN_MS);
        assert_eq!(s.daemon.poll_interval_ms, defaults::DAEMON_POLL_INTERVAL_MS);
    }

    /// issue #67：接入运行时前把遗留脚手架的默认开翻成关。
    #[test]
    fn auto_swap_defaults_off() {
        assert!(!AutoSwap::default().enabled);
        assert!(!Settings::default().auto_swap.enabled);
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert!(!s.auto_swap.enabled);
    }

    /// issue #70：键缺失或 Default 均为 false，存量无此键的 `[auto_swap]` 仍能加载。
    #[test]
    fn prefer_lower_tier_defaults_off() {
        assert!(!AutoSwap::default().prefer_lower_tier);
        assert!(!Settings::default().auto_swap.prefer_lower_tier);
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert!(!s.auto_swap.prefer_lower_tier);

        let path = tmp.path().join("stock.toml");
        std::fs::write(&path, "[auto_swap]\nenabled = true\nthreshold = 0.5\n").unwrap();
        let stock = load_from(&path).unwrap();
        assert!(stock.auto_swap.enabled);
        assert!(!stock.auto_swap.prefer_lower_tier);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[auto_swap]
threshold = 0.5
foo = 1
"#,
        )
        .unwrap();
        let err = load_from(&path).unwrap_err().to_string();
        assert!(err.contains("foo"), "{err}");
    }

    #[test]
    fn stock_auto_swap_daemon_beta_sections_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[auto_swap]
enabled = false
threshold = 0.5
cooldown_ms = 1
settle_grace_ms = 2

[daemon]
poll_interval_ms = 1000
idle_threshold_ms = 2000
idle_poll_interval_ms = 3000

[beta]
usage_cache_max_age_ms = 4000
"#,
        )
        .unwrap();
        let s = load_from(&path).unwrap();
        assert!(!s.auto_swap.enabled);
        assert_eq!(s.auto_swap.threshold, 0.5);
        assert_eq!(s.auto_swap.cooldown_ms, 1);
        assert_eq!(s.auto_swap.settle_grace_ms, 2);
        assert_eq!(s.daemon.poll_interval_ms, 1000);
        assert_eq!(s.daemon.idle_threshold_ms, 2000);
        assert_eq!(s.daemon.idle_poll_interval_ms, 3000);
        assert_eq!(s.beta.usage_cache_max_age_ms, 4000);
        assert_eq!(s.quota.fetch_retries, defaults::QUOTA_FETCH_RETRIES);
    }

    /// 存量手写 `enabled = true` 仍按文件生效，默认翻转不影响显式开启。
    #[test]
    fn stock_auto_swap_enabled_true_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[auto_swap]\nenabled = true\nthreshold = 0.5\n").unwrap();
        let s = load_from(&path).unwrap();
        assert!(s.auto_swap.enabled);
        assert_eq!(s.auto_swap.threshold, 0.5);
        assert_eq!(s.auto_swap.cooldown_ms, defaults::AUTO_SWAP_COOLDOWN_MS);
    }

    /// issue #42 验收 1：开关默认关闭（缺文件与空段都为 false）。
    #[test]
    fn proactive_renewal_defaults_off() {
        assert!(!Settings::default().proactive_renewal.enabled);
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert!(!s.proactive_renewal.enabled);
    }

    #[test]
    fn proactive_renewal_section_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[proactive_renewal]\nenabled = true\n").unwrap();
        let s = load_from(&path).unwrap();
        assert!(s.proactive_renewal.enabled);
        assert_eq!(s.quota.fetch_timeout_ms, defaults::QUOTA_FETCH_TIMEOUT_MS);
    }

    /// issue #42 验收 2：写回后重读得到同一值（持久化）。
    #[test]
    fn upsert_creates_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let s = upsert_proactive_renewal(&path, true).unwrap();
        assert!(s.proactive_renewal.enabled);
        let reread = load_from(&path).unwrap();
        assert!(reread.proactive_renewal.enabled);
        assert_eq!(reread.as_ref(), s.as_ref());
    }

    /// 写回只动一个键：手写注释、键顺序与其它字段原样保留。
    #[test]
    fn upsert_preserves_comments_and_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "# 我的调优\n[auto_swap]\nthreshold = 0.5 # 别动\n").unwrap();
        upsert_proactive_renewal(&path, true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# 我的调优"), "{raw}");
        assert!(raw.contains("threshold = 0.5 # 别动"), "{raw}");
        assert!(raw.contains("enabled = true"), "{raw}");
        let s = load_from(&path).unwrap();
        assert!(s.proactive_renewal.enabled);
        assert_eq!(s.auto_swap.threshold, 0.5);
    }

    #[test]
    fn upsert_flips_existing_key_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[proactive_renewal]\n# 风险自负\nenabled = true\n").unwrap();
        upsert_proactive_renewal(&path, false).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# 风险自负"), "{raw}");
        assert!(raw.contains("enabled = false"), "{raw}");
        assert!(!load_from(&path).unwrap().proactive_renewal.enabled);
    }

    /// 存量同名键不是表：整体换成新表，最终能按 Settings 解析。
    #[test]
    fn upsert_repairs_non_table_section() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "proactive_renewal = 1\n").unwrap();
        upsert_proactive_renewal(&path, true).unwrap();
        assert!(load_from(&path).unwrap().proactive_renewal.enabled);
    }

    /// 成品不再是合法 Settings（存量文件有拼错的键）时报错，且不动盘。
    #[test]
    fn upsert_rejects_invalid_settings_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let original = "[auto_swap]\nfoo = 1\n";
        std::fs::write(&path, original).unwrap();
        let err = upsert_proactive_renewal(&path, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("foo"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// issue #68：写回后重读得到同一值（持久化）。
    #[test]
    fn upsert_auto_swap_creates_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let s = upsert_auto_swap(&path, true).unwrap();
        assert!(s.auto_swap.enabled);
        let reread = load_from(&path).unwrap();
        assert!(reread.auto_swap.enabled);
        assert_eq!(reread.as_ref(), s.as_ref());
        assert!(!reread.proactive_renewal.enabled);
    }

    /// 写回只动 `auto_swap.enabled`：手写注释、阈值与其它段原样保留。
    #[test]
    fn upsert_auto_swap_preserves_comments_and_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "# 我的调优\n[auto_swap]\nthreshold = 0.5 # 别动\n[proactive_renewal]\nenabled = true # 续期别动\n",
        )
        .unwrap();
        upsert_auto_swap(&path, true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# 我的调优"), "{raw}");
        assert!(raw.contains("threshold = 0.5 # 别动"), "{raw}");
        assert!(raw.contains("enabled = true # 续期别动"), "{raw}");
        let s = load_from(&path).unwrap();
        assert!(s.auto_swap.enabled);
        assert_eq!(s.auto_swap.threshold, 0.5);
        assert!(s.proactive_renewal.enabled);
    }

    #[test]
    fn upsert_auto_swap_flips_existing_key_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[auto_swap]\n# 风险自负\nenabled = false\nthreshold = 0.5\n",
        )
        .unwrap();
        upsert_auto_swap(&path, true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# 风险自负"), "{raw}");
        assert!(raw.contains("enabled = true"), "{raw}");
        assert!(raw.contains("threshold = 0.5"), "{raw}");
        let s = load_from(&path).unwrap();
        assert!(s.auto_swap.enabled);
        assert_eq!(s.auto_swap.threshold, 0.5);
    }

    /// 存量同名键不是表：整体换成新表，最终能按 Settings 解析。
    #[test]
    fn upsert_auto_swap_repairs_non_table_section() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "auto_swap = 1\n").unwrap();
        upsert_auto_swap(&path, true).unwrap();
        assert!(load_from(&path).unwrap().auto_swap.enabled);
    }

    /// issue #70：写回只动 `auto_swap.prefer_lower_tier`：手写注释、其它键与其它段原样保留。
    #[test]
    fn upsert_auto_swap_prefer_lower_tier_preserves_comments_and_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "# 我的调优\n[auto_swap]\nthreshold = 0.5 # 别动\nenabled = true # 开关别动\n[proactive_renewal]\nenabled = true # 续期别动\n",
        )
        .unwrap();
        upsert_auto_swap_prefer_lower_tier(&path, true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# 我的调优"), "{raw}");
        assert!(raw.contains("threshold = 0.5 # 别动"), "{raw}");
        assert!(raw.contains("enabled = true # 开关别动"), "{raw}");
        assert!(raw.contains("enabled = true # 续期别动"), "{raw}");
        let s = load_from(&path).unwrap();
        assert!(s.auto_swap.prefer_lower_tier);
        assert!(s.auto_swap.enabled);
        assert_eq!(s.auto_swap.threshold, 0.5);
        assert!(s.proactive_renewal.enabled);
    }

    /// 成品不再是合法 Settings 时报错，且不动盘。
    #[test]
    fn upsert_auto_swap_rejects_invalid_settings_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let original = "[auto_swap]\nfoo = 1\n";
        std::fs::write(&path, original).unwrap();
        let err = upsert_auto_swap(&path, true).unwrap_err().to_string();
        assert!(err.contains("foo"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
