//! 产品级**编译期**默认常量集中地。
//!
//! 运行时实际生效的值来自 [`crate::settings::current`]，由 `config.toml` 覆盖。
//! 这里的常量只是 `Settings::default()` 的 fallback，缺字段 / 配置文件缺失时使用。
//!
//! 命名约定：`<DOMAIN>_<NAME>`。
//! 单位：百分比统一 0.0~1.0 或 0~100，命名里点明；时间统一毫秒。

// ============================================================
// 自动切换
// ============================================================

/// AutoSwap 触发阈值（0.0~1.0）。
///
/// 任一窗口 used/limit ≥ 此值即触发切换。
///
/// 配套不变量：AGENTS.md #5。
pub const AUTO_SWAP_THRESHOLD: f64 = 0.99;

/// 自动切换冷却期（毫秒）。
///
/// 两次自动切换成功之间的最短间隔，防止阈值线附近乒乓。
/// 时间锚是上次自动切换成功时刻，不是「刚被切走的账号不能当候选」。
pub const AUTO_SWAP_COOLDOWN_MS: i64 = 5 * 60 * 1000;

/// 新激活账号的「安顿宽限期」（毫秒）。
///
/// 刚被手动选中的当前账号，在 `last_used_at` 起的此窗口内不得被自动切走，
/// 即使已达阈值。
pub const AUTO_SWAP_SETTLE_GRACE_MS: i64 = 60 * 1000;

/// 手动切换后的「回驻保持期」（毫秒）（issue #95 / ADR-0008）。
///
/// 任何手动切换成功后写 `manual_hold_until`；此窗口内不触发回驻（非耗尽的
/// 档位偏好切换），耗尽被迫切换不受影响。不进 GUI，只在 config.toml 调。
pub const AUTO_SWAP_MANUAL_HOLD_MS: i64 = 8 * 60 * 60 * 1000;

/// 耗尽 / pace 路径候选防抖边际（0.0~1.0）（issue #108 / ADR-0010，pace 复用见 ADR-0013）。
///
/// 停泊账号最大已用比例须 ≤ 当前账号最大已用比例 − 此值，才成为耗尽或 pace 目标。
/// 默认 0：与未配置时行为一致。不进 GUI，只在 config.toml 调；不作用于回驻。
pub const AUTO_SWAP_CANDIDATE_MARGIN: f64 = 0.0;

/// 回驻确认期：目标须连续合格的快照周期数（issue #111 / ADR-0012）。
///
/// 默认 2：首个合格周期不切回。0 与 1 同义（第一个合格周期即切）。
/// 只约束回驻路径，不进 GUI，只在 config.toml 调。
pub const AUTO_SWAP_RESIDENT_CONFIRM_N: u32 = 2;

/// pace 预切换子开关（issue #114 / ADR-0013）。默认关闭。
///
/// 开启后，当前账号未达耗尽阈值、但已用/窗口已流逝比 > 1（预测撑不到 reset）
/// 且最大已用已达预警水位时，可提前切到更健康的停泊账号。不进 GUI。
pub const AUTO_SWAP_PACE_PRE_SWAP: bool = false;

/// pace 预切换预警水位（0.0~1.0）（issue #114 / ADR-0013）。
///
/// 当前账号最大已用比例须 ≥ 此值才走 pace 路径。默认 0.8。不进 GUI。
pub const AUTO_SWAP_PACE_WARN_THRESHOLD: f64 = 0.8;

// ============================================================
// 额度状态视觉阈值（仅影响展示，不影响 AutoSwap 决策）
// ============================================================

/// Provider 将 [`crate::QuotaStatus::Warn`] 标记给 quota 的阈值（百分比 0~100）。
///
/// 用途：CLI 展示着色 / 用户感知接近上限。**不耦合 [`AUTO_SWAP_THRESHOLD`]**。
/// 设低于 AutoSwap 阈值，让用户在自动切换发生之前就能看到 WARN。
pub const QUOTA_WARN_PCT: f64 = 90.0;

/// `QuotaStatus::Exhausted` 的阈值（百分比 0~100）。通常就是 100。
pub const QUOTA_EXHAUSTED_PCT: f64 = 100.0;

/// Beta usage 实时接口字段漂移时，允许使用旧版本地缓存的最长时间。
///
/// 仅作为兼容兜底；过期缓存不参与展示/自动切换，避免 stale quota 误导策略。
pub const BETA_USAGE_CACHE_MAX_AGE_MS: i64 = 10 * 60 * 1000;

/// 单次 quota 查询 attempt 的超时（毫秒）。
///
/// CLI 与 daemon 都通过统一重试包装查询 quota。单次 attempt 超过此值会被取消，并按
/// [`QUOTA_FETCH_RETRIES`] 决定是否重试。
///
/// 必须盖住 Beta active 的官方 app-server 会话上限（20s），以及 Kimi active 401 自愈
/// （`kimi --version` + 持锁刷新 + 重查）。过短会把慢但正常的查询取消成可重试 timeout，
/// 最终显示 `timeout after N attempts` 并回落旧缓存。
pub const QUOTA_FETCH_TIMEOUT_MS: u64 = 20_000;

/// quota 查询失败后的重试次数。
///
/// 这里表示「首次请求之外」额外再试几次。默认 1 次；401/403/429 不会重试。
/// 单次 attempt 已按慢路径（Beta app-server / Kimi 自愈）拉到 20s，不宜再叠多次重试。
pub const QUOTA_FETCH_RETRIES: u32 = 1;

/// quota 查询首次重试前等待多久（毫秒）。
///
/// 后续按 `base * 2^(attempt-1)` 指数退避，给瞬时网络错误恢复窗口。
pub const QUOTA_FETCH_RETRY_DELAY_MS: u64 = 500;

/// 同一账号两次真实 quota 查询的最小间隔（毫秒），默认 90s。
///
/// 上游 usage 端点限流极严（实测每账号约每分钟才放行 1 次）。kimi-switch 的 CLI 每次运行
/// 与 daemon 每轮都会把所有账号一起查，极易并发打爆该端点触发 429。设此下限后，缓存比它新就
/// 直接复用、不再请求；取 90s（> daemon 60s 轮询）使 daemon 也会跳过部分轮次，把每账号请求频率
/// 稳定压到 ~90s 一次。daemon 与 CLI 共用 `quota_cache.json`，节流对两条路径同时生效。
pub const QUOTA_MIN_REFRESH_INTERVAL_MS: u64 = 90_000;

/// 用户发起额度查询时，成功缓存的连点窗口（毫秒）。不进 GUI 设置。
///
/// 计划内路径仍走 [`QUOTA_MIN_REFRESH_INTERVAL_MS`]；点「刷新」用更短窗口，
/// 按该账号上次成功写入缓存的时间判断。429 / 鉴权失败两种原因都继续退避；
/// 超时、网络等可重试失败在用户发起时忽略退避再查。
pub const QUOTA_USER_INITIATED_MIN_INTERVAL_MS: u64 = 5_000;

/// quota 查询失败后的退避上限（毫秒），默认 15 分钟。
///
/// 成功结果会写进缓存并被 [`QUOTA_MIN_REFRESH_INTERVAL_MS`] 节流；**失败结果没有缓存**，
/// 因此在引入退避之前，一个持续查不出的账号会被 daemon 每轮（60s）无节制地重查，
/// 请求频率反而高于健康账号，把 usage 端点的限流桶打空 → 429 蔓延到其他账号。
/// 退避从 `min_refresh_interval_ms` 起按连续失败次数翻倍，封顶到这里。
pub const QUOTA_FAILURE_BACKOFF_MAX_MS: u64 = 900_000;

// ============================================================
// 当前账号主动续期（ADR-0003 第 2 条；issue #42 设置壳，issue #44 接入运行时消费）
// ============================================================

/// 「当前账号主动续期」开关默认值：关闭。
///
/// 仅 Windows 等无官方刷新锁平台的可选能力（见 docs/adr/0003）：开启后当前账号
/// 额度查询在 access token 临期时主动轮换 live 凭证；锁探测 fail-closed，
/// 探测不到「确定无锁」一律不碰 live。
pub const PROACTIVE_RENEWAL_ENABLED: bool = false;

// ============================================================
// Token 生命周期
// ============================================================

/// Token 距离过期还有多少毫秒内视为「需要预刷新」。
///
/// Alpha `activate` 路径会在此窗口内尝试 best-effort 刷新；daemon 后台保活也用同一窗口。
pub const REFRESH_SLACK_MS: i64 = 5 * 60 * 1000;

// ============================================================
// daemon 周期
// ============================================================

/// daemon 轮询周期（毫秒）。M4 使用。活跃时（最近有客户端在跑）的频率。
pub const DAEMON_POLL_INTERVAL_MS: u64 = 60 * 1000;

/// daemon 「空闲」判定阈值（毫秒）。
///
/// 客户端凭证文件 mtime 距今超过此值 → 视为用户没在用 AI，
/// daemon 退到 [`DAEMON_IDLE_POLL_INTERVAL_MS`] 节奏，减少无意义的 quota 请求。
pub const DAEMON_IDLE_THRESHOLD_MS: i64 = 30 * 60 * 1000;

/// daemon 空闲时的轮询周期（毫秒）。
///
/// 由 [`DAEMON_IDLE_THRESHOLD_MS`] 判定空闲后启用；一旦 probe 文件再次变动，
/// 下一轮立刻回到 [`DAEMON_POLL_INTERVAL_MS`]。
pub const DAEMON_IDLE_POLL_INTERVAL_MS: u64 = 30 * 60 * 1000;
