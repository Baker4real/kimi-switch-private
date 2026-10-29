//! Tauri 入口：把 `kimi-switch-gui` 的命令接到 webview。

#[cfg(all(not(debug_assertions), dev))]
compile_error!(
    "生产构建不能走 Vite devUrl，请启用 feature custom-protocol（或用 npm run tauri build）"
);

use std::sync::Arc;

use kimi_switch_gui::{
    finish_edge_web_bind, Backend, CommandError, QuotaCollectReason, Snapshot, SwapOutcome,
};
use tauri::{Manager, State};

mod web_bind;

struct AppState {
    backend: Arc<Backend>,
    web_bind: Arc<web_bind::WebBindBroker>,
}

fn map_err(err: anyhow::Error) -> CommandError {
    CommandError::from_anyhow(err)
}

/// 列出账号并 collect 额度（节流）。启动加载、静默查询、刷新按钮共用这一条。
/// `user_initiated` 缺省为计划内；点「刷新」传 true 走用户发起的额度查询。
/// `current_only` 传 true 时聚焦当前账号（issue #161 适应性轮询加密轮）：只对
/// 当前账号发真实查询，停泊账号以缓存行组装；Tauri v2 参数键为 camelCase
/// （前端传 `currentOnly`）。
#[tauri::command]
async fn list_accounts(
    state: State<'_, AppState>,
    user_initiated: Option<bool>,
    current_only: Option<bool>,
) -> Result<Snapshot, CommandError> {
    let reason = if user_initiated.unwrap_or(false) {
        QuotaCollectReason::UserInitiated
    } else {
        QuotaCollectReason::Scheduled
    };
    Ok(state
        .backend
        .snapshot_scoped(None, reason, current_only.unwrap_or(false))
        .await)
}

#[tauri::command]
async fn import_current(state: State<'_, AppState>) -> Result<Snapshot, CommandError> {
    state.backend.import().await.map_err(map_err)
}

/// 手动切换（issue #157）：activate 提交（含审计、手动保持期）后立即返回轻量
/// 结果，不再同步等待额度快照重列；前端据返回立即把所点卡片标记为当前账号，
/// 再后台触发既有 `list_accounts` 刷新额度，刷新期间可立刻再切另一张卡。
#[tauri::command]
async fn swap_account(state: State<'_, AppState>, id: String) -> Result<SwapOutcome, CommandError> {
    state.backend.swap(&id).await.map_err(map_err)
}

#[tauri::command]
async fn remove_account(state: State<'_, AppState>, id: String) -> Result<Snapshot, CommandError> {
    state.backend.remove(&id).await.map_err(map_err)
}

#[tauri::command]
async fn rename_account(
    state: State<'_, AppState>,
    id: String,
    label: String,
) -> Result<Snapshot, CommandError> {
    state.backend.rename(&id, &label).await.map_err(map_err)
}

/// 账号卡「摘出自动切换」开关（issue #112）：写 extra，返回含 toast 的快照。
#[tauri::command]
async fn set_auto_swap_opt_out(
    state: State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result<Snapshot, CommandError> {
    state
        .backend
        .set_auto_swap_opt_out(&id, enabled)
        .await
        .map_err(map_err)
}

// 按卡授权：`target` 传目标账号 id 即为该卡重新授权；缺省（null/不传）= 添加新账号。
#[tauri::command]
async fn start_device_auth(
    state: State<'_, AppState>,
    target: Option<String>,
) -> Result<kimi_switch_gui::AuthLink, CommandError> {
    state
        .backend
        .start_device_auth(target)
        .await
        .map_err(map_err)
}

#[tauri::command]
async fn complete_device_auth(state: State<'_, AppState>) -> Result<Snapshot, CommandError> {
    state.backend.complete_device_auth().await.map_err(map_err)
}

#[tauri::command]
fn cancel_device_auth(state: State<'_, AppState>) {
    state.backend.cancel_device_auth();
}

/// 绑定网页登录（issue #51）：`token` 由前端绑定对话框粘贴传入，只发给 #50 的
/// kimi 管道（域名固定 www.kimi.com），不落日志、不进返回的 Snapshot。
#[tauri::command]
async fn bind_web_account(
    state: State<'_, AppState>,
    id: String,
    token: String,
) -> Result<Snapshot, CommandError> {
    state.backend.bind_web(&id, &token).await.map_err(map_err)
}

/// 发起 Edge 免粘贴绑定：挑战 URL 只含一次性 nonce，不含网页凭证。
#[tauri::command]
async fn begin_web_bind(
    state: State<'_, AppState>,
    id: String,
) -> Result<web_bind::WebBindChallenge, CommandError> {
    let challenge = state.web_bind.begin(&id).await.map_err(map_err)?;
    if let Err(error) = web_bind::open_in_edge(&challenge.url) {
        state.web_bind.cancel(&challenge.nonce);
        return Err(map_err(error));
    }
    Ok(challenge)
}

/// 等待 Edge 扩展把网页凭证交给一次性回调，再复用既有 bind-web 管道。
/// 跨账号串线校验在 broker 内完成：目标账号已变化时丢弃回调凭证并报错。
/// token 只在 Rust 内存中流转，不进入 WebView 返回值、日志或错误文本。
/// wait 的错误由 broker 自己发布。管道结算并写完审计之后、额度快照之前，
/// 按既有 GUI 文案发布回查状态；WebView 仍等快照返回。返回给 GUI 的 Result 文案不变。
#[tauri::command]
async fn complete_web_bind(
    state: State<'_, AppState>,
    id: String,
    nonce: String,
) -> Result<Snapshot, CommandError> {
    let token = state.web_bind.wait(&nonce, &id).await.map_err(map_err)?;
    let settled = state.backend.settle_web_bind(&id, &token).await;
    finish_edge_web_bind(
        settled,
        |outcome| state.web_bind.publish_bind_outcome(&nonce, outcome),
        |message| {
            state
                .backend
                .snapshot(Some(message), QuotaCollectReason::Scheduled)
        },
    )
    .await
    .map_err(map_err)
}

/// 取消等待中的 Edge 绑定；不会写凭证，也不会改变已有绑定。
#[tauri::command]
fn cancel_web_bind(state: State<'_, AppState>, nonce: String) {
    state.web_bind.cancel(&nonce);
}

#[tauri::command]
async fn open_url(url: String) -> Result<(), CommandError> {
    Backend::open_url(&url).await.map_err(map_err)
}

/// 「当前账号主动续期」开关（issue #42）：写 config.toml，返回落盘后的生效值。
#[tauri::command]
fn set_proactive_renewal(enabled: bool) -> Result<bool, CommandError> {
    Backend::save_proactive_renewal(enabled).map_err(map_err)
}

/// 「额度耗尽自动切换」开关（issue #68）：写 config.toml，返回落盘后的生效值。
#[tauri::command]
fn set_auto_swap(enabled: bool) -> Result<bool, CommandError> {
    Backend::save_auto_swap(enabled).map_err(map_err)
}

/// 「自动切换时优先低档位账号」子开关（issue #71）：写 config.toml，返回落盘后的生效值。
#[tauri::command]
fn set_auto_swap_prefer_lower_tier(enabled: bool) -> Result<bool, CommandError> {
    Backend::save_auto_swap_prefer_lower_tier(enabled).map_err(map_err)
}

/// 「额度恢复后回驻低档账号」子开关（issue #95）：写 config.toml，返回落盘后的生效值。
#[tauri::command]
fn set_auto_swap_resident_lower_tier(enabled: bool) -> Result<bool, CommandError> {
    Backend::save_auto_swap_resident_lower_tier(enabled).map_err(map_err)
}

/// 「重置前抢用」子开关（issue #150）：写 config.toml，返回落盘后的生效值。
#[tauri::command]
fn set_auto_swap_pre_reset_burn(enabled: bool) -> Result<bool, CommandError> {
    Backend::save_auto_swap_pre_reset_burn(enabled).map_err(map_err)
}

/// 「pace 预切换」子开关（issue #159，ADR-0013）：写 config.toml，返回落盘后的生效值。
#[tauri::command]
fn set_auto_swap_pace_pre_swap(enabled: bool) -> Result<bool, CommandError> {
    Backend::save_auto_swap_pace_pre_swap(enabled).map_err(map_err)
}

/// 任务栏图标修正（Windows）：tao 只给窗口设 16px 的 ICON_SMALL，从不设 ICON_BIG；
/// Win11 任务栏按钮在 >100% 缩放下读 ICON_BIG 槽位，为空时渲染白色占位图、
/// 不会回退到 SMALL，表现为「任务栏图标空白」。
///
/// 实测 explorer 只在按钮建立时读一次图标，之后仅对 BIG 槽位「NULL -> 非空」
/// 的变化刷新（等值换句柄不刷新）；而启动瞬间设置又赶不上 explorer 首读。
/// 因此这里延迟 1s 再按窗口 DPI 从 exe 图标资源（tauri-build 嵌入，ID 32512）
/// 取最佳尺寸补齐 BIG/SMALL（alt-tab 缩略图同样取 BIG，一并修复）。
#[cfg(windows)]
fn fix_taskbar_icon(window: &tauri::WebviewWindow) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        LoadImageW, SendMessageW, HICON, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR,
        WM_SETICON,
    };

    let Ok(hwnd) = window.hwnd() else {
        eprintln!("kimi-switch: 任务栏图标修正跳过：拿不到窗口句柄");
        return;
    };
    let scale: f64 = window.scale_factor().unwrap_or(1.0).clamp(1.0, 4.0);
    let big = (32.0 * scale).round() as i32;
    let small = (16.0 * scale).round() as i32;
    let hwnd = hwnd.0 as isize; // 裸指针不能跨线程，按整数传递

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let make = |size: i32| {
                LoadImageW(
                    hinst,
                    32512usize as *const u16, // tauri-winres 嵌入的应用图标资源 ID
                    IMAGE_ICON as u32,
                    size,
                    size,
                    LR_DEFAULTCOLOR,
                ) as HICON
            };
            let (h_big, h_small) = (make(big), make(small));
            if h_big.is_null() || h_small.is_null() {
                eprintln!("kimi-switch: 任务栏图标修正失败：LoadImageW 返回空句柄");
                return;
            }
            let hwnd = hwnd as *mut _;
            SendMessageW(hwnd, WM_SETICON as u32, ICON_BIG as usize, h_big as isize);
            SendMessageW(
                hwnd,
                WM_SETICON as u32,
                ICON_SMALL as usize,
                h_small as isize,
            );
            // HICON 不转移所有权给窗口，须存活到进程结束；GUI 生命周期内有意不释放。
            let _ = (h_big, h_small);
        }
    });
}

/// 初始化失败时要弹给用户的文案与进程退出码（无控制台子系统下不能只 panic）。
struct InitFailureNotice {
    title: String,
    message: String,
    exit_code: i32,
}

fn init_failure_notice(err: &impl std::fmt::Display) -> InitFailureNotice {
    InitFailureNotice {
        title: "kimi-switch".into(),
        message: format!("初始化失败：{err}"),
        exit_code: 1,
    }
}

fn show_native_error_dialog(title: &str, message: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }
        let title = wide(title);
        let message = wide(message);
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("{title}: {message}");
    }
}

fn abort_on_init_failure(err: &anyhow::Error) -> ! {
    let notice = init_failure_notice(err);
    eprintln!("kimi-switch: {}", notice.message);
    show_native_error_dialog(&notice.title, &notice.message);
    std::process::exit(notice.exit_code);
}

/// 后台启动参数（issue #196）：跟随启动的拉起器（#197）带它拉起 GUI，手动带上也能验证。
const BACKGROUND_ARG: &str = "--background";

/// 一次启动的方式，由命令行参数决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchMode {
    /// 普通启动：首次启动显示并聚焦主窗口。
    Normal,
    /// 后台启动：首次启动把主窗口最小化到任务栏，不激活、不抢焦点，也不先在前台闪现。
    Background,
}

/// 判定启动方式。参数形如 `std::env::args_os()`，首项是程序路径，不参与判定；单实例
/// 回调转交的第二次启动参数同形。
fn launch_mode<I>(args: I) -> LaunchMode
where
    I: IntoIterator,
    I::Item: AsRef<std::ffi::OsStr>,
{
    if args
        .into_iter()
        .skip(1)
        .any(|arg| arg.as_ref() == BACKGROUND_ARG)
    {
        LaunchMode::Background
    } else {
        LaunchMode::Normal
    }
}

/// 第二次启动（单实例回调）是否改动已有窗口：普通启动还原、显示并聚焦；后台启动（如多个
/// 会话同时开始时并发拉起）一律不动，已有窗口保持原状、不抢焦点。
fn relaunch_reveals_window(mode: LaunchMode) -> bool {
    mode == LaunchMode::Normal
}

/// 按启动方式创建主窗口（配置里 `create: false`，Tauri 不自动创建）。普通启动与配置一致：
/// 创建即显示并聚焦。后台启动先不显示、不聚焦，再由 [`show_minimized_inactive`] 最小化显示；
/// 不聚焦是为了让 WebView2 创建时不 MoveFocus：否则还隐藏着的窗口也会被激活，进程有前台
/// 权限时还会拿走前台。
fn build_main_window(
    app: &tauri::App,
    launch: LaunchMode,
) -> Result<tauri::WebviewWindow, Box<dyn std::error::Error>> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .ok_or("tauri.conf.json 缺少 main 窗口配置")?;
    let builder = tauri::WebviewWindowBuilder::from_config(app.handle(), config)?;
    // 只有 Windows 实现了后台显示；其他平台照普通启动显示。
    let builder = if cfg!(windows) && launch == LaunchMode::Background {
        builder.visible(false).focused(false)
    } else {
        builder
    };
    Ok(builder.build()?)
}

/// 后台启动的首次显示（Windows）：最小化到任务栏，不激活、不抢焦点，也不先在前台闪现。
///
/// 窗口按 `visible(false)` 创建，这里直接 `ShowWindow(SW_SHOWMINNOACTIVE)`，让它从隐藏直接
/// 变成最小化，与「以最小化方式运行」的快捷方式同一条路径。tao 的两个接口都不合用：
/// `minimize()` 走 SW_MINIMIZE，会激活 Z 序里的下一个窗口；`show()` 在 `focused(false)` 下
/// 走 SW_SHOWNOACTIVATE，会把窗口先以普通状态显示出来。
///
/// 绕开 tao 之后，它记的窗口标志与实际不符，要补两处：
/// - MINIMIZED：随即 `minimize()`。tao 先按 IsIconic 原地同步，与目标一致就没有 diff，不发
///   任何 Win32 调用。
/// - VISIBLE：仍为假。tao 每次变更窗口标志时（标题栏的最小化 / 最大化按钮都会），只要它为假
///   就补一次 SW_HIDE，窗口会从任务栏消失。能同步它的只有 `show()`，而 `show()` 会把最小化的
///   窗口还原出来，所以等窗口首次离开最小化（用户从任务栏还原）时再补：此时窗口已在显示，
///   SW_SHOWNOACTIVATE 是空操作。单实例回调先 `show()` 再 `unminimize()`，也先完成这次同步。
#[cfg(windows)]
fn show_minimized_inactive(window: &tauri::WebviewWindow) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWMINNOACTIVE};

    let Ok(hwnd) = window.hwnd() else {
        // 宁可普通显示（不激活），也不能让窗口一直隐藏着。
        eprintln!("kimi-switch: 后台启动最小化跳过：拿不到窗口句柄");
        let _ = window.show();
        return;
    };
    unsafe {
        ShowWindow(hwnd.0, SW_SHOWMINNOACTIVE);
    }
    let _ = window.minimize();

    let visible_synced = AtomicBool::new(false);
    let handle = window.clone();
    window.on_window_event(move |event| {
        let tauri::WindowEvent::Resized(size) = event else {
            return;
        };
        if size.width == 0 || size.height == 0 || handle.is_minimized().unwrap_or(true) {
            return;
        }
        if !visible_synced.swap(true, Ordering::Relaxed) {
            let _ = handle.show();
        }
    });
}

pub fn run() {
    let launch = launch_mode(std::env::args_os());
    let backend = match Backend::new() {
        Ok(backend) => {
            // 启动一致性自检（issue #155）：以 live 为准纠正 active / 提示工具外身份
            // 可导入；一致时零副作用。提示由首个快照的 toast 带出。
            backend.startup_check();
            Arc::new(backend)
        }
        Err(err) => abort_on_init_failure(&err),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if !relaunch_reveals_window(launch_mode(&args)) {
                return;
            }
            if let Some(window) = app.get_webview_window("main") {
                // 先 show 再 unminimize：后台启动的窗口首次离开最小化前，tao 记的 VISIBLE 仍为
                // 假（见 show_minimized_inactive），先 unminimize 会在还原后补一次 SW_HIDE。普通
                // 启动的窗口 VISIBLE 一直为真，show 是空操作，顺序不影响。
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .manage(AppState {
            backend,
            web_bind: Arc::new(web_bind::WebBindBroker::default()),
        })
        .setup(move |app| {
            let window = build_main_window(app, launch)?;
            let _ =
                window.set_background_color(Some(tauri::utils::config::Color(244, 241, 236, 255)));
            #[cfg(windows)]
            {
                fix_taskbar_icon(&window);
                if launch == LaunchMode::Background {
                    show_minimized_inactive(&window);
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_accounts,
            import_current,
            swap_account,
            remove_account,
            rename_account,
            start_device_auth,
            complete_device_auth,
            cancel_device_auth,
            bind_web_account,
            begin_web_bind,
            complete_web_bind,
            cancel_web_bind,
            open_url,
            set_proactive_renewal,
            set_auto_swap,
            set_auto_swap_prefer_lower_tier,
            set_auto_swap_resident_lower_tier,
            set_auto_swap_pre_reset_burn,
            set_auto_swap_pace_pre_swap,
            set_auto_swap_opt_out,
        ])
        .run(tauri::generate_context!())
        .expect("error while running kimi-switch");
}

#[cfg(test)]
mod init_failure_tests {
    use super::init_failure_notice;

    #[test]
    fn init_failure_notice_uses_chinese_message_and_nonzero_exit() {
        let notice = init_failure_notice(&"凭证文件损坏");
        assert_eq!(notice.title, "kimi-switch");
        assert_eq!(notice.message, "初始化失败：凭证文件损坏");
        assert_eq!(notice.exit_code, 1);
    }
}

#[cfg(test)]
mod launch_mode_tests {
    use super::{launch_mode, relaunch_reveals_window, LaunchMode};

    const EXE: &str = r"C:\Program Files\kimi-switch\kimi-switch.exe";

    /// 跟随启动的拉起器（#197）带 `--background` 拉起 GUI：首次启动走后台，最小化、不激活。
    #[test]
    fn first_launch_with_background_flag_is_background() {
        assert_eq!(launch_mode([EXE, "--background"]), LaunchMode::Background);
    }

    /// 用户双击启动不带参数：首次启动照旧显示并聚焦。
    #[test]
    fn first_launch_without_flag_is_normal() {
        assert_eq!(launch_mode([EXE]), LaunchMode::Normal);
    }

    /// GUI 已在运行时用户再双击启动：还原、显示并聚焦已有窗口（现状）。
    #[test]
    fn normal_relaunch_reveals_existing_window() {
        assert!(relaunch_reveals_window(launch_mode([EXE])));
    }

    /// GUI 已在运行（最小化、在后台或在前台）时拉起器再带 `--background` 启动（如多个会话
    /// 同时开始）：已有窗口不动，不还原、不抢焦点。
    #[test]
    fn background_relaunch_leaves_existing_window_untouched() {
        assert!(!relaunch_reveals_window(launch_mode([EXE, "--background"])));
    }
}

#[cfg(test)]
mod window_config_tests {
    /// 主窗口由 setup 按启动方式创建（后台启动先不显示）。配置若让 Tauri 自动创建，窗口会
    /// 先以普通状态出现，setup 再建同名窗口也会失败，GUI 起不来。
    #[test]
    fn main_window_is_created_by_setup_not_by_config() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json");
        let main = config["app"]["windows"]
            .as_array()
            .and_then(|windows| windows.iter().find(|w| w["label"] == "main"))
            .expect("配置里要有 main 窗口");
        assert_eq!(
            main["create"],
            serde_json::Value::Bool(false),
            "main 窗口必须 create: false，由 setup 创建"
        );
    }
}

#[cfg(test)]
mod csp_tests {
    /// 额度条与会员轨道用 `style="width:N%"` 画填充（会员=已度过比例，剩余越短越满）。
    /// 仅 `default-src 'self'` 时 style 属性被禁，块级 `.quota-fill` 拉满轨道。
    #[test]
    fn webview_csp_allows_style_attrs_for_bar_width() {
        let raw = include_str!("../tauri.conf.json");
        assert!(
            raw.contains("default-src 'self'"),
            "脚本与资源仍只允许 self"
        );
        assert!(
            raw.contains("style-src-attr 'unsafe-inline'"),
            "必须放行 style 属性，否则会员/额度条宽无效、看起来永远满条"
        );
    }
}
