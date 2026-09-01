//! Tauri 入口：把 `kimi-switch-gui` 的命令接到 webview。

#[cfg(all(not(debug_assertions), dev))]
compile_error!("生产构建不能走 Vite devUrl，请启用 feature custom-protocol（或用 npm run tauri build）");

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
fn open_url(url: String) -> Result<(), CommandError> {
    Backend::open_url(&url).map_err(map_err)
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

pub fn run() {
    let backend = match Backend::new() {
        Ok(backend) => {
            backend.sync_local_active();
            Arc::new(backend)
        }
        Err(err) => {
            eprintln!("kimi-switch: 初始化失败: {err:#}");
            panic!("kimi-switch 初始化失败: {err:#}");
        }
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
                let _ = window.set_background_color(Some(tauri::utils::config::Color(
                    244, 241, 236, 255,
                )));
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running kimi-switch");
}
