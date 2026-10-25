//! 运行时可加载的数值调优配置。
//!
//! 设计：
//! - **唯一 source of truth**：`<config_dir>/config.toml`（缺失则用 [`crate::defaults`] 编译期默认值）。
//! - **加载**：[`current`] 每次从默认 `config.toml` 补水并更新内存快照，因此 GUI 不必调用
//!   [`reload_from_file`]，设置页落盘后下一次读取可见新值。单元测试跳过补水，以免本机配置污染并行用例。
//! - **显式重载**：[`reload_from_file`] 供 CLI 启动时把解析/校验错误返回给调用方（CLI 短命）。
//! - **失败兜底**：文件不存在 → 用默认值；文件存在但解析或校验失败 → 沿用上一次成功加载的值并打 warn。
//! - **校验**：`auto_swap.threshold`、`auto_swap.candidate_margin`、
//!   `auto_swap.pace_warn_threshold` 与 `auto_swap.pre_reset_burn_min_ratio`
//!   收敛到 `0.0..=1.0`；冷却/宽限等时间类拒绝负值。
//! - **访问入口**：[`current`] 返回 `Arc<Settings>`，所有调用点都从这里读，不要长期缓存返回值。
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
    /// 合格停泊账号中优先选距 `reset_at` 更近者（issue #109 / ADR-0011）。
    /// 默认关闭；只改耗尽路径候选排序，不改资格线，不作用于回驻。
    pub prefer_soonest_reset: bool,
    /// 回驻子开关（issue #95 / ADR-0008）：当前账号未耗尽、但存在档位严格更低的
    /// 合格停泊账号时自动切回其中档位最低者。默认关闭；不影响耗尽切换。
    pub resident_lower_tier: bool,
    /// 手动切换后的保持期（毫秒）。期间不回驻、不新开重置前抢用；耗尽被迫切换不受影响。
    pub manual_hold_ms: i64,
    /// 耗尽 / pace 路径候选防抖边际（0.0~1.0，issue #108 / ADR-0010）。停泊账号
    /// 最大已用比例须 ≤ 当前账号最大已用比例 − 此值，才成为目标。默认 0，与未配置
    /// 时行为一致。不改触发线，不作用于回驻。pace 复用此键，不另造差值（ADR-0013）。
    pub candidate_margin: f64,
    /// 回驻确认期（issue #111 / ADR-0012）：目标停泊账号须连续这么多个快照周期
    /// 保持合格才真正切回。默认 2；0 与 1 同义。只约束回驻路径，不进 GUI。
    pub resident_confirm_n: u32,
    /// pace 预切换子开关（issue #114 / ADR-0013）。默认关闭。开启后，未达耗尽
    /// 阈值也可能因预测撑不到 reset 而提前切换。不进 GUI。
    pub pace_pre_swap: bool,
    /// pace 预切换预警水位（0.0~1.0）。当前账号最大已用须 ≥ 此值才考虑 pace。
    /// 加载时 clamp 到 `0.0..=1.0`（与 threshold 同一套）。默认 0.8。
    pub pace_warn_threshold: f64,
    /// 重置前抢用子开关（issue #147 / ADR-0014）。默认关闭。开启后，未达耗尽
    /// 且 pace 未命中时，可切到临近重置且该窗口已用超水位的停泊账号。
    pub pre_reset_burn: bool,
    /// 重置前抢用水位（0.0~1.0）。临近重置的窗口自身已用须 > 此值。
    /// 加载时须有限并 clamp 到 `0.0..=1.0`（与 threshold 同一套）。默认 0.7。
    pub pre_reset_burn_min_ratio: f64,
    /// 5 小时窗视为临近重置的剩余时间（毫秒）。默认 1 小时。
    pub pre_reset_burn_near_5h_ms: i64,
    /// 7 天窗视为临近重置的剩余时间（毫秒）。默认 2 天。
    pub pre_reset_burn_near_7d_ms: i64,
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
            prefer_soonest_reset: false,
            resident_lower_tier: false,
            manual_hold_ms: defaults::AUTO_SWAP_MANUAL_HOLD_MS,
            candidate_margin: defaults::AUTO_SWAP_CANDIDATE_MARGIN,
            resident_confirm_n: defaults::AUTO_SWAP_RESIDENT_CONFIRM_N,
            pace_pre_swap: defaults::AUTO_SWAP_PACE_PRE_SWAP,
            pace_warn_threshold: defaults::AUTO_SWAP_PACE_WARN_THRESHOLD,
            pre_reset_burn: defaults::AUTO_SWAP_PRE_RESET_BURN,
            pre_reset_burn_min_ratio: defaults::AUTO_SWAP_PRE_RESET_BURN_MIN_RATIO,
            pre_reset_burn_near_5h_ms: defaults::AUTO_SWAP_PRE_RESET_BURN_NEAR_5H_MS,
            pre_reset_burn_near_7d_ms: defaults::AUTO_SWAP_PRE_RESET_BURN_NEAR_7D_MS,
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
/// 各模块**每次访问**都调一次，不要把返回值长期缓存。生产路径每次从默认 `config.toml` 补水，
/// 设置页只落盘时下一次读取仍能看到新值；文件损坏则 warn 并沿用上次成功值。
pub fn current() -> Arc<Settings> {
    hydrate_from_default_file();
    cell().read().expect("settings rwlock poisoned").clone()
}

fn hydrate_from_default_file() {
    // 单元测试保持编译期默认，避免本机 config.toml 或并行用例的 KIMI_SWITCH_HOME 串台。
    if cfg!(test) {
        return;
    }
    match load_from_default_path() {
        Ok(parsed) => install(parsed),
        Err(e) => tracing::warn!(err = %e, "配置文件无效，沿用当前值"),
    }
}

fn load_from_default_path() -> Result<Arc<Settings>> {
    let path = AppPaths::resolve()?.config_file();
    load_from(&path)
}

/// 把解析后的配置收敛到合法区间；时间类负值返回明确错误。
fn validate_and_normalize(settings: &mut Settings) -> Result<()> {
    let threshold = settings.auto_swap.threshold;
    if !threshold.is_finite() {
        return Err(Error::Config("auto_swap.threshold 必须是有限数值".into()));
    }
    settings.auto_swap.threshold = threshold.clamp(0.0, 1.0);

    let margin = settings.auto_swap.candidate_margin;
    if !margin.is_finite() {
        return Err(Error::Config(
            "auto_swap.candidate_margin 必须是有限数值".into(),
        ));
    }
    settings.auto_swap.candidate_margin = margin.clamp(0.0, 1.0);

    let pace_warn = settings.auto_swap.pace_warn_threshold;
    if !pace_warn.is_finite() {
        return Err(Error::Config(
            "auto_swap.pace_warn_threshold 必须是有限数值".into(),
        ));
    }
    settings.auto_swap.pace_warn_threshold = pace_warn.clamp(0.0, 1.0);

    let burn_min = settings.auto_swap.pre_reset_burn_min_ratio;
    if !burn_min.is_finite() {
        return Err(Error::Config(
            "auto_swap.pre_reset_burn_min_ratio 必须是有限数值".into(),
        ));
    }
    settings.auto_swap.pre_reset_burn_min_ratio = burn_min.clamp(0.0, 1.0);

    fn reject_negative(name: &str, value: i64) -> Result<()> {
        if value < 0 {
            Err(Error::Config(format!("{name} 不能为负（当前 {value}）")))
        } else {
            Ok(())
        }
    }

    reject_negative("auto_swap.cooldown_ms", settings.auto_swap.cooldown_ms)?;
    reject_negative(
        "auto_swap.settle_grace_ms",
        settings.auto_swap.settle_grace_ms,
    )?;
    reject_negative(
        "auto_swap.pre_reset_burn_near_5h_ms",
        settings.auto_swap.pre_reset_burn_near_5h_ms,
    )?;
    reject_negative(
        "auto_swap.pre_reset_burn_near_7d_ms",
        settings.auto_swap.pre_reset_burn_near_7d_ms,
    )?;
    reject_negative("token.refresh_slack_ms", settings.token.refresh_slack_ms)?;
    reject_negative(
        "daemon.idle_threshold_ms",
        settings.daemon.idle_threshold_ms,
    )?;
    reject_negative(
        "beta.usage_cache_max_age_ms",
        settings.beta.usage_cache_max_age_ms,
    )?;
    Ok(())
}

fn settings_from_toml(raw: &str, path: &Path) -> Result<Settings> {
    let mut parsed: Settings =
        toml::from_str(raw).map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))?;
    validate_and_normalize(&mut parsed)?;
    Ok(parsed)
}

/// 重新从默认路径 `<config_dir>/config.toml` 读取配置。
///
/// 行为：
/// - 文件不存在 → 装入 [`Settings::default`] 并返回。
/// - 解析且校验成功 → 替换全局当前值并返回新快照。
/// - 解析或校验失败 → 保留旧值，返回 `Err`（调用方一般 warn 一行就继续）。
pub fn reload_from_file() -> Result<Arc<Settings>> {
    let parsed = load_from_default_path()?;
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
    Ok(Arc::new(settings_from_toml(&raw, path)?))
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
    let parsed = settings_from_toml(&rendered, path)?;
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

/// 把「额度恢复后回驻低档账号」写入 `<config_dir>/config.toml`，返回新快照。
///
/// 只落盘，不改全局当前值；写序与 [`set_auto_swap_enabled`] 相同。
pub fn set_auto_swap_resident_lower_tier(enabled: bool) -> Result<Arc<Settings>> {
    let path = AppPaths::resolve()?.config_file();
    upsert_auto_swap_resident_lower_tier(&path, enabled)
}

/// 把「重置前抢用」写入 `<config_dir>/config.toml`，返回新快照。
///
/// 只落盘，不改全局当前值；写序与 [`set_auto_swap_enabled`] 相同。
pub fn set_auto_swap_pre_reset_burn(enabled: bool) -> Result<Arc<Settings>> {
    let path = AppPaths::resolve()?.config_file();
    upsert_auto_swap_pre_reset_burn(&path, enabled)
}

/// 把「pace 预切换」写入 `<config_dir>/config.toml`，返回新快照。
///
/// 只落盘，不改全局当前值；写序与 [`set_auto_swap_enabled`] 相同。
pub fn set_auto_swap_pace_pre_swap(enabled: bool) -> Result<Arc<Settings>> {
    let path = AppPaths::resolve()?.config_file();
    upsert_auto_swap_pace_pre_swap(&path, enabled)
}

/// 读-改-写指定配置文件里的 `auto_swap.enabled`。
fn upsert_auto_swap(path: &Path, enabled: bool) -> Result<Arc<Settings>> {
    upsert_auto_swap_bool(path, "enabled", enabled)
}

/// 读-改-写指定配置文件里的 `auto_swap.prefer_lower_tier`。
fn upsert_auto_swap_prefer_lower_tier(path: &Path, prefer: bool) -> Result<Arc<Settings>> {
    upsert_auto_swap_bool(path, "prefer_lower_tier", prefer)
}

/// 读-改-写指定配置文件里的 `auto_swap.resident_lower_tier`。
fn upsert_auto_swap_resident_lower_tier(path: &Path, enabled: bool) -> Result<Arc<Settings>> {
    upsert_auto_swap_bool(path, "resident_lower_tier", enabled)
}

/// 读-改-写指定配置文件里的 `auto_swap.pre_reset_burn`（issue #150）。
fn upsert_auto_swap_pre_reset_burn(path: &Path, enabled: bool) -> Result<Arc<Settings>> {
    upsert_auto_swap_bool(path, "pre_reset_burn", enabled)
}

/// 读-改-写指定配置文件里的 `auto_swap.pace_pre_swap`（issue #159）。
fn upsert_auto_swap_pace_pre_swap(path: &Path, enabled: bool) -> Result<Arc<Settings>> {
    upsert_auto_swap_bool(path, "pace_pre_swap", enabled)
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
    let parsed = settings_from_toml(&rendered, path)?;
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

    /// issue #109：键缺失或 Default 均为 false，存量无此键的 `[auto_swap]` 仍能加载。
    #[test]
    fn prefer_soonest_reset_defaults_off() {
        assert!(!AutoSwap::default().prefer_soonest_reset);
        assert!(!Settings::default().auto_swap.prefer_soonest_reset);
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert!(!s.auto_swap.prefer_soonest_reset);

        let path = tmp.path().join("stock.toml");
        std::fs::write(&path, "[auto_swap]\nenabled = true\nthreshold = 0.5\n").unwrap();
        let stock = load_from(&path).unwrap();
        assert!(stock.auto_swap.enabled);
        assert!(!stock.auto_swap.prefer_soonest_reset);

        let explicit = tmp.path().join("explicit.toml");
        std::fs::write(&explicit, "[auto_swap]\nprefer_soonest_reset = true\n").unwrap();
        assert!(load_from(&explicit).unwrap().auto_swap.prefer_soonest_reset);
    }

    /// issue #108：键缺失或 Default 均为 0，存量无此键的 `[auto_swap]` 仍能加载。
    #[test]
    fn candidate_margin_defaults_zero() {
        assert_eq!(AutoSwap::default().candidate_margin, 0.0);
        assert_eq!(Settings::default().auto_swap.candidate_margin, 0.0);
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert_eq!(s.auto_swap.candidate_margin, 0.0);

        let path = tmp.path().join("stock.toml");
        std::fs::write(&path, "[auto_swap]\nenabled = true\nthreshold = 0.5\n").unwrap();
        let stock = load_from(&path).unwrap();
        assert!(stock.auto_swap.enabled);
        assert_eq!(stock.auto_swap.candidate_margin, 0.0);

        let explicit = tmp.path().join("explicit.toml");
        std::fs::write(&explicit, "[auto_swap]\ncandidate_margin = 0.05\n").unwrap();
        assert_eq!(
            load_from(&explicit).unwrap().auto_swap.candidate_margin,
            0.05
        );
    }

    /// issue #111：键缺失或 Default 均为 2，存量无此键的 `[auto_swap]` 仍能加载。
    #[test]
    fn resident_confirm_n_defaults_two() {
        assert_eq!(AutoSwap::default().resident_confirm_n, 2);
        assert_eq!(Settings::default().auto_swap.resident_confirm_n, 2);
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert_eq!(s.auto_swap.resident_confirm_n, 2);

        let path = tmp.path().join("stock.toml");
        std::fs::write(&path, "[auto_swap]\nenabled = true\nthreshold = 0.5\n").unwrap();
        let stock = load_from(&path).unwrap();
        assert!(stock.auto_swap.enabled);
        assert_eq!(stock.auto_swap.resident_confirm_n, 2);

        let explicit = tmp.path().join("explicit.toml");
        std::fs::write(&explicit, "[auto_swap]\nresident_confirm_n = 4\n").unwrap();
        assert_eq!(
            load_from(&explicit).unwrap().auto_swap.resident_confirm_n,
            4
        );
    }

    /// issue #114：键缺失或 Default 均为 false，存量无此键的 `[auto_swap]` 仍能加载。
    #[test]
    fn pace_pre_swap_defaults_off() {
        assert!(!AutoSwap::default().pace_pre_swap);
        assert!(!Settings::default().auto_swap.pace_pre_swap);
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert!(!s.auto_swap.pace_pre_swap);

        let path = tmp.path().join("stock.toml");
        std::fs::write(&path, "[auto_swap]\nenabled = true\nthreshold = 0.5\n").unwrap();
        let stock = load_from(&path).unwrap();
        assert!(stock.auto_swap.enabled);
        assert!(!stock.auto_swap.pace_pre_swap);

        let explicit = tmp.path().join("explicit.toml");
        std::fs::write(&explicit, "[auto_swap]\npace_pre_swap = true\n").unwrap();
        assert!(load_from(&explicit).unwrap().auto_swap.pace_pre_swap);
    }

    /// issue #114：键缺失或 Default 均为 0.8，存量无此键的 `[auto_swap]` 仍能加载。
    #[test]
    fn pace_warn_threshold_defaults_point_eight() {
        assert_eq!(AutoSwap::default().pace_warn_threshold, 0.8);
        assert_eq!(Settings::default().auto_swap.pace_warn_threshold, 0.8);
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert_eq!(s.auto_swap.pace_warn_threshold, 0.8);

        let path = tmp.path().join("stock.toml");
        std::fs::write(&path, "[auto_swap]\nenabled = true\nthreshold = 0.5\n").unwrap();
        let stock = load_from(&path).unwrap();
        assert!(stock.auto_swap.enabled);
        assert_eq!(stock.auto_swap.pace_warn_threshold, 0.8);

        let explicit = tmp.path().join("explicit.toml");
        std::fs::write(&explicit, "[auto_swap]\npace_warn_threshold = 0.7\n").unwrap();
        assert_eq!(
            load_from(&explicit).unwrap().auto_swap.pace_warn_threshold,
            0.7
        );
    }

    /// issue #147：键缺失或 Default 均为 false，存量无此键的 `[auto_swap]` 仍能加载。
    #[test]
    fn pre_reset_burn_defaults_off() {
        assert!(!AutoSwap::default().pre_reset_burn);
        assert!(!Settings::default().auto_swap.pre_reset_burn);
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert!(!s.auto_swap.pre_reset_burn);

        let path = tmp.path().join("stock.toml");
        std::fs::write(&path, "[auto_swap]\nenabled = true\nthreshold = 0.5\n").unwrap();
        let stock = load_from(&path).unwrap();
        assert!(stock.auto_swap.enabled);
        assert!(!stock.auto_swap.pre_reset_burn);

        let explicit = tmp.path().join("explicit.toml");
        std::fs::write(&explicit, "[auto_swap]\npre_reset_burn = true\n").unwrap();
        assert!(load_from(&explicit).unwrap().auto_swap.pre_reset_burn);
    }

    /// issue #147：键缺失或 Default 均为 0.7，存量无此键的 `[auto_swap]` 仍能加载。
    #[test]
    fn pre_reset_burn_min_ratio_defaults_point_seven() {
        assert_eq!(AutoSwap::default().pre_reset_burn_min_ratio, 0.7);
        assert_eq!(Settings::default().auto_swap.pre_reset_burn_min_ratio, 0.7);
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert_eq!(s.auto_swap.pre_reset_burn_min_ratio, 0.7);

        let path = tmp.path().join("stock.toml");
        std::fs::write(&path, "[auto_swap]\nenabled = true\nthreshold = 0.5\n").unwrap();
        let stock = load_from(&path).unwrap();
        assert!(stock.auto_swap.enabled);
        assert_eq!(stock.auto_swap.pre_reset_burn_min_ratio, 0.7);

        let explicit = tmp.path().join("explicit.toml");
        std::fs::write(&explicit, "[auto_swap]\npre_reset_burn_min_ratio = 0.8\n").unwrap();
        assert_eq!(
            load_from(&explicit)
                .unwrap()
                .auto_swap
                .pre_reset_burn_min_ratio,
            0.8
        );
    }

    /// issue #147：5h 默认 1 小时、7d 默认 2 天；缺键与显式值均可加载。
    #[test]
    fn pre_reset_burn_near_ms_defaults() {
        assert_eq!(AutoSwap::default().pre_reset_burn_near_5h_ms, 3_600_000);
        assert_eq!(AutoSwap::default().pre_reset_burn_near_7d_ms, 172_800_000);
        assert_eq!(
            Settings::default().auto_swap.pre_reset_burn_near_5h_ms,
            3_600_000
        );
        assert_eq!(
            Settings::default().auto_swap.pre_reset_burn_near_7d_ms,
            172_800_000
        );
        let tmp = tempfile::tempdir().unwrap();
        let s = load_from(&tmp.path().join("config.toml")).unwrap();
        assert_eq!(s.auto_swap.pre_reset_burn_near_5h_ms, 3_600_000);
        assert_eq!(s.auto_swap.pre_reset_burn_near_7d_ms, 172_800_000);

        let path = tmp.path().join("stock.toml");
        std::fs::write(&path, "[auto_swap]\nenabled = true\nthreshold = 0.5\n").unwrap();
        let stock = load_from(&path).unwrap();
        assert!(stock.auto_swap.enabled);
        assert_eq!(stock.auto_swap.pre_reset_burn_near_5h_ms, 3_600_000);
        assert_eq!(stock.auto_swap.pre_reset_burn_near_7d_ms, 172_800_000);

        let explicit = tmp.path().join("explicit.toml");
        std::fs::write(
            &explicit,
            "[auto_swap]\npre_reset_burn_near_5h_ms = 1000\npre_reset_burn_near_7d_ms = 2000\n",
        )
        .unwrap();
        let loaded = load_from(&explicit).unwrap();
        assert_eq!(loaded.auto_swap.pre_reset_burn_near_5h_ms, 1000);
        assert_eq!(loaded.auto_swap.pre_reset_burn_near_7d_ms, 2000);
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

    /// issue #150：写回只动 `auto_swap.pre_reset_burn`：手写注释、其它键与其它段原样保留。
    #[test]
    fn upsert_auto_swap_pre_reset_burn_preserves_comments_and_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "# 我的调优\n[auto_swap]\nthreshold = 0.5 # 别动\nenabled = true # 开关别动\n[proactive_renewal]\nenabled = true # 续期别动\n",
        )
        .unwrap();
        upsert_auto_swap_pre_reset_burn(&path, true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# 我的调优"), "{raw}");
        assert!(raw.contains("threshold = 0.5 # 别动"), "{raw}");
        assert!(raw.contains("enabled = true # 开关别动"), "{raw}");
        assert!(raw.contains("enabled = true # 续期别动"), "{raw}");
        let s = load_from(&path).unwrap();
        assert!(s.auto_swap.pre_reset_burn);
        assert!(s.auto_swap.enabled);
        assert_eq!(s.auto_swap.threshold, 0.5);
        assert!(s.proactive_renewal.enabled);
    }

    /// issue #159：写回只动 `auto_swap.pace_pre_swap`：手写注释、其它键与其它段原样保留。
    #[test]
    fn upsert_auto_swap_pace_pre_swap_preserves_comments_and_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "# 我的调优\n[auto_swap]\nthreshold = 0.5 # 别动\nenabled = true # 开关别动\n[proactive_renewal]\nenabled = true # 续期别动\n",
        )
        .unwrap();
        upsert_auto_swap_pace_pre_swap(&path, true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# 我的调优"), "{raw}");
        assert!(raw.contains("threshold = 0.5 # 别动"), "{raw}");
        assert!(raw.contains("enabled = true # 开关别动"), "{raw}");
        assert!(raw.contains("enabled = true # 续期别动"), "{raw}");
        let s = load_from(&path).unwrap();
        assert!(s.auto_swap.pace_pre_swap);
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

    /// issue #74：阈值收敛到 0..=1；冷却/宽限等时间类拒绝负值。
    #[test]
    fn out_of_range_numbers_are_clamped_or_rejected() {
        enum Expect {
            Threshold(f64),
            Margin(f64),
            PaceWarn(f64),
            BurnMin(f64),
            ErrContains(&'static str),
        }
        let cases: &[(&str, &str, Expect)] = &[
            (
                "threshold_above_one",
                "[auto_swap]\nthreshold = 1.5\n",
                Expect::Threshold(1.0),
            ),
            (
                "threshold_below_zero",
                "[auto_swap]\nthreshold = -0.2\n",
                Expect::Threshold(0.0),
            ),
            (
                "threshold_zero",
                "[auto_swap]\nthreshold = 0.0\n",
                Expect::Threshold(0.0),
            ),
            (
                "threshold_one",
                "[auto_swap]\nthreshold = 1.0\n",
                Expect::Threshold(1.0),
            ),
            (
                "candidate_margin_above_one",
                "[auto_swap]\ncandidate_margin = 1.5\n",
                Expect::Margin(1.0),
            ),
            (
                "candidate_margin_below_zero",
                "[auto_swap]\ncandidate_margin = -0.2\n",
                Expect::Margin(0.0),
            ),
            (
                "candidate_margin_zero",
                "[auto_swap]\ncandidate_margin = 0.0\n",
                Expect::Margin(0.0),
            ),
            (
                "candidate_margin_one",
                "[auto_swap]\ncandidate_margin = 1.0\n",
                Expect::Margin(1.0),
            ),
            (
                "pace_warn_above_one",
                "[auto_swap]\npace_warn_threshold = 1.5\n",
                Expect::PaceWarn(1.0),
            ),
            (
                "pace_warn_below_zero",
                "[auto_swap]\npace_warn_threshold = -0.2\n",
                Expect::PaceWarn(0.0),
            ),
            (
                "pace_warn_zero",
                "[auto_swap]\npace_warn_threshold = 0.0\n",
                Expect::PaceWarn(0.0),
            ),
            (
                "pace_warn_one",
                "[auto_swap]\npace_warn_threshold = 1.0\n",
                Expect::PaceWarn(1.0),
            ),
            (
                "burn_min_above_one",
                "[auto_swap]\npre_reset_burn_min_ratio = 1.5\n",
                Expect::BurnMin(1.0),
            ),
            (
                "burn_min_below_zero",
                "[auto_swap]\npre_reset_burn_min_ratio = -0.2\n",
                Expect::BurnMin(0.0),
            ),
            (
                "burn_min_zero",
                "[auto_swap]\npre_reset_burn_min_ratio = 0.0\n",
                Expect::BurnMin(0.0),
            ),
            (
                "burn_min_one",
                "[auto_swap]\npre_reset_burn_min_ratio = 1.0\n",
                Expect::BurnMin(1.0),
            ),
            (
                "cooldown_negative",
                "[auto_swap]\ncooldown_ms = -1\n",
                Expect::ErrContains("cooldown_ms"),
            ),
            (
                "settle_grace_negative",
                "[auto_swap]\nsettle_grace_ms = -1\n",
                Expect::ErrContains("settle_grace_ms"),
            ),
            (
                "burn_near_5h_negative",
                "[auto_swap]\npre_reset_burn_near_5h_ms = -1\n",
                Expect::ErrContains("pre_reset_burn_near_5h_ms"),
            ),
            (
                "burn_near_7d_negative",
                "[auto_swap]\npre_reset_burn_near_7d_ms = -1\n",
                Expect::ErrContains("pre_reset_burn_near_7d_ms"),
            ),
            (
                "refresh_slack_negative",
                "[token]\nrefresh_slack_ms = -1\n",
                Expect::ErrContains("refresh_slack_ms"),
            ),
            (
                "idle_threshold_negative",
                "[daemon]\nidle_threshold_ms = -1\n",
                Expect::ErrContains("idle_threshold_ms"),
            ),
            (
                "usage_cache_max_age_negative",
                "[beta]\nusage_cache_max_age_ms = -1\n",
                Expect::ErrContains("usage_cache_max_age_ms"),
            ),
        ];

        for (name, body, expect) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("config.toml");
            std::fs::write(&path, body).unwrap();
            let got = load_from(&path);
            match expect {
                Expect::Threshold(want) => {
                    let s = got.unwrap_or_else(|e| panic!("{name}: unexpected error {e}"));
                    assert_eq!(s.auto_swap.threshold, *want, "{name}");
                }
                Expect::Margin(want) => {
                    let s = got.unwrap_or_else(|e| panic!("{name}: unexpected error {e}"));
                    assert_eq!(s.auto_swap.candidate_margin, *want, "{name}");
                }
                Expect::PaceWarn(want) => {
                    let s = got.unwrap_or_else(|e| panic!("{name}: unexpected error {e}"));
                    assert_eq!(s.auto_swap.pace_warn_threshold, *want, "{name}");
                }
                Expect::BurnMin(want) => {
                    let s = got.unwrap_or_else(|e| panic!("{name}: unexpected error {e}"));
                    assert_eq!(s.auto_swap.pre_reset_burn_min_ratio, *want, "{name}");
                }
                Expect::ErrContains(needle) => {
                    let err = got.unwrap_err().to_string();
                    assert!(
                        err.contains(needle),
                        "{name}: error should mention {needle}: {err}"
                    );
                    assert!(
                        err.contains("负") || err.to_ascii_lowercase().contains("negative"),
                        "{name}: error should say the value is negative: {err}"
                    );
                }
            }
        }
    }

    /// issue #74：`[quota]` 段加载后覆盖编译期默认，供收集/查询/着色消费。
    #[test]
    fn quota_section_overrides_compile_time_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[quota]
warn_pct = 70.0
exhausted_pct = 95.0
fetch_timeout_ms = 12345
fetch_retries = 3
fetch_retry_delay_ms = 7
min_refresh_interval_ms = 1000
failure_backoff_max_ms = 2000
"#,
        )
        .unwrap();
        let s = load_from(&path).unwrap();
        assert_eq!(s.quota.warn_pct, 70.0);
        assert_eq!(s.quota.exhausted_pct, 95.0);
        assert_eq!(s.quota.fetch_timeout_ms, 12345);
        assert_eq!(s.quota.fetch_retries, 3);
        assert_eq!(s.quota.fetch_retry_delay_ms, 7);
        assert_eq!(s.quota.min_refresh_interval_ms, 1000);
        assert_eq!(s.quota.failure_backoff_max_ms, 2000);
    }

    /// issue #74：零冷却仍合法；负值才拒绝。
    #[test]
    fn zero_cooldown_is_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[auto_swap]\ncooldown_ms = 0\nsettle_grace_ms = 0\n").unwrap();
        let s = load_from(&path).unwrap();
        assert_eq!(s.auto_swap.cooldown_ms, 0);
        assert_eq!(s.auto_swap.settle_grace_ms, 0);
    }
}
