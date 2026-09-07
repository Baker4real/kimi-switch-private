//! Tauri 入口：把 `kimi-switch-gui` 的命令接到 webview。

#[cfg(all(not(debug_assertions), dev))]
compile_error!(
    "生产构建不能走 Vite devUrl，请启用 feature custom-protocol（或用 npm run tauri build）"
);

use std::sync::Arc;

use kimi_switch_gui::{Backend, CommandError, QuotaCollectReason, Snapshot, ThemeId};
use tauri::{Manager, State};

struct AppState {
    backend: Arc<Backend>,
}

fn map_err(err: anyhow::Error) -> CommandError {
    CommandError::from_anyhow(err)
}

/// 列出账号并 collect 额度（节流）。启动加载、静默查询、刷新按钮共用这一条。
/// `user_initiated` 缺省为计划内；点「刷新」传 true 走用户发起的额度查询。
#[tauri::command]
async fn list_accounts(
    state: State<'_, AppState>,
    user_initiated: Option<bool>,
) -> Result<Snapshot, CommandError> {
    let reason = if user_initiated.unwrap_or(false) {
        QuotaCollectReason::UserInitiated
    } else {
        QuotaCollectReason::Scheduled
    };
    Ok(state.backend.snapshot(None, reason).await)
}

#[tauri::command]
async fn import_current(state: State<'_, AppState>) -> Result<Snapshot, CommandError> {
    state.backend.import().await.map_err(map_err)
}

#[tauri::command]
async fn swap_account(state: State<'_, AppState>, id: String) -> Result<Snapshot, CommandError> {
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

#[tauri::command]
async fn open_url(url: String) -> Result<(), CommandError> {
    Backend::open_url(&url).await.map_err(map_err)
}

#[tauri::command]
fn set_theme(theme: String) -> Result<ThemeId, CommandError> {
    let theme = ThemeId::parse(&theme);
    Backend::save_theme(theme).map_err(map_err)?;
    Ok(theme)
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

pub fn run() {
    let backend = match Backend::new() {
        Ok(backend) => {
            backend.sync_local_active();
            Arc::new(backend)
        }
        Err(err) => abort_on_init_failure(&err),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .manage(AppState { backend })
        .setup(|app| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window
                    .set_background_color(Some(tauri::utils::config::Color(244, 241, 236, 255)));
                #[cfg(windows)]
                fix_taskbar_icon(&window);
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
            open_url,
            set_theme,
            set_proactive_renewal,
            set_auto_swap,
            set_auto_swap_prefer_lower_tier,
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
