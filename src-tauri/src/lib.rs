use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{fs::OpenOptions, io::Write, sync::Mutex};

use multimeters_core::{api, paths, AlertLog, AppEngine, AppSettings, Dashboard};
use serde_json::json;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_positioner::{Position, WindowExt};

/// Upper bound on how long the background loop sleeps between due-time checks.
const REFRESH_TICK_SECS: u64 = 20;

/// Identifies the tray icon so its tooltip can be refreshed after each fetch.
const TRAY_ID: &str = "multimeters";

/// Put the pinned meters on the tray tooltip, so the numbers are readable on hover.
async fn update_tray_tooltip(app: &AppHandle, engine: &AppEngine) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    warn_result(
        tray.set_tooltip(Some(engine.tray_summary().await)),
        "update the tray tooltip",
    );
}

struct State {
    engine: Arc<AppEngine>,
    notified: Arc<Mutex<AlertLog>>,
    /// Mirrors `AppSettings::hide_on_blur`. The window event handler runs outside the async
    /// runtime, so it reads the flag here rather than awaiting the settings lock.
    hide_on_blur: Arc<AtomicBool>,
}

#[tauri::command]
async fn get_dashboard(state: tauri::State<'_, State>) -> Result<Dashboard, String> {
    Ok(state.engine.dashboard().await)
}

#[tauri::command]
async fn refresh_all(
    app: AppHandle,
    state: tauri::State<'_, State>,
    force: bool,
) -> Result<Dashboard, String> {
    state.engine.refresh_all(force).await;
    deliver_notifications(&app, &state.engine, &state.notified).await;
    update_tray_tooltip(&app, &state.engine).await;
    Ok(state.engine.dashboard().await)
}

#[tauri::command]
async fn refresh_one(
    app: AppHandle,
    state: tauri::State<'_, State>,
    id: String,
    force: bool,
) -> Result<Dashboard, String> {
    state.engine.refresh_one(&id, force).await?;
    deliver_notifications(&app, &state.engine, &state.notified).await;
    update_tray_tooltip(&app, &state.engine).await;
    Ok(state.engine.dashboard().await)
}

#[tauri::command]
async fn get_settings(state: tauri::State<'_, State>) -> Result<AppSettings, String> {
    Ok(state.engine.settings().await)
}

#[tauri::command]
async fn patch_settings(
    app: AppHandle,
    state: tauri::State<'_, State>,
    patch: serde_json::Value,
) -> Result<AppSettings, String> {
    let previous = state.engine.settings().await;
    let settings = state
        .engine
        .patch_settings(patch.clone())
        .await
        .map_err(|error| error.to_string())?;

    if patch.get("hide_on_blur").is_some() {
        state
            .hide_on_blur
            .store(settings.hide_on_blur, Ordering::Relaxed);
    }
    if patch.get("launch_at_login").is_some() {
        if let Err(error) = set_autostart(&app, settings.launch_at_login) {
            let rollback = state
                .engine
                .patch_settings(json!({ "launch_at_login": previous.launch_at_login }))
                .await;
            return Err(match rollback {
                Ok(_) => error,
                Err(rollback) => format!(
                    "{error} MultiMeters also could not restore the saved setting: {rollback}"
                ),
            });
        }
    }
    if patch.get("global_shortcut").is_some() {
        if let Err(error) = set_global_shortcut(
            &app,
            previous.global_shortcut.as_deref(),
            settings.global_shortcut.as_deref(),
        ) {
            let rollback = state
                .engine
                .patch_settings(json!({ "global_shortcut": previous.global_shortcut }))
                .await;
            return Err(match rollback {
                Ok(_) => error,
                Err(rollback) => format!(
                    "{error} MultiMeters also could not restore the saved setting: {rollback}"
                ),
            });
        }
    }
    Ok(settings)
}

#[tauri::command]
async fn set_api_key(
    state: tauri::State<'_, State>,
    provider_id: String,
    value: String,
) -> Result<AppSettings, String> {
    let settings = state
        .engine
        .set_api_key(&provider_id, &value)
        .await
        .map_err(|error| error.to_string())?;
    if !value.trim().is_empty() {
        state.engine.refresh_one(&provider_id, true).await?;
    }
    Ok(settings)
}

#[tauri::command]
async fn reset_all_settings(
    app: AppHandle,
    state: tauri::State<'_, State>,
) -> Result<AppSettings, String> {
    let previous = state.engine.settings().await;
    let defaults = AppSettings::default();

    // Change the fallible OS integrations before persisting the reset. If any later step fails,
    // put both integrations back so the UI, settings file, and Windows state cannot drift apart.
    set_autostart(&app, defaults.launch_at_login)?;
    if let Err(error) = set_global_shortcut(
        &app,
        previous.global_shortcut.as_deref(),
        defaults.global_shortcut.as_deref(),
    ) {
        let rollback = set_autostart(&app, previous.launch_at_login);
        return Err(append_rollback_error(error, rollback));
    }

    match state.engine.reset_all_settings().await {
        Ok(settings) => {
            state
                .hide_on_blur
                .store(settings.hide_on_blur, Ordering::Relaxed);
            Ok(settings)
        }
        Err(error) => {
            let mut message = format!("Could not reset settings: {error}");
            message = append_rollback_error(
                message,
                set_global_shortcut(
                    &app,
                    defaults.global_shortcut.as_deref(),
                    previous.global_shortcut.as_deref(),
                ),
            );
            Err(append_rollback_error(
                message,
                set_autostart(&app, previous.launch_at_login),
            ))
        }
    }
}

#[tauri::command]
async fn get_customize(state: tauri::State<'_, State>) -> Result<serde_json::Value, String> {
    Ok(state.engine.customize().await)
}

#[tauri::command]
async fn toggle_pin(
    state: tauri::State<'_, State>,
    provider_id: String,
    widget_id: String,
) -> Result<AppSettings, String> {
    state.engine.toggle_pin(&provider_id, &widget_id).await
}

#[tauri::command]
fn reveal_log(app: AppHandle) -> Result<(), String> {
    let path = paths::log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    if !path.exists() {
        std::fs::write(&path, []).map_err(|error| error.to_string())?;
    }
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn hide_flyout(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "The MultiMeters window is unavailable.".to_string())?;
    window
        .hide()
        .map_err(|error| format!("Could not hide MultiMeters: {error}"))
}

fn show_flyout(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        warn_result(
            window.move_window(Position::TrayCenter),
            "position the flyout",
        );
        warn_result(window.show(), "show the flyout");
        warn_result(window.set_focus(), "focus the flyout");
        warn_result(
            window.emit("dashboard-updated", ()),
            "notify the flyout of an update",
        );
    } else {
        tracing::error!("could not show the flyout because its window is missing");
    }
}

fn show_screen(app: &AppHandle, screen: &str) {
    if let Some(window) = app.get_webview_window("main") {
        warn_result(window.emit("navigate", screen), "navigate the flyout");
    }
    show_flyout(app);
}

fn toggle_flyout(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        match window.is_visible() {
            Ok(true) => warn_result(window.hide(), "hide the flyout"),
            Ok(false) => show_flyout(app),
            Err(error) => {
                tracing::warn!(%error, "could not determine whether the flyout is visible");
                show_flyout(app);
            }
        }
    }
}

fn warn_result<T, E: std::fmt::Display>(result: Result<T, E>, action: &str) {
    if let Err(error) = result {
        tracing::warn!(%error, %action, "MultiMeters window operation failed");
    }
}

fn append_rollback_error(message: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => message,
        Err(error) => {
            format!("{message} MultiMeters also could not restore Windows state: {error}")
        }
    }
}

fn set_autostart(app: &AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if enabled {
        manager.enable()
    } else {
        manager.disable()
    }
    .map_err(|error| format!("Could not update Launch at Login: {error}"))
}

fn set_global_shortcut(
    app: &AppHandle,
    previous: Option<&str>,
    next: Option<&str>,
) -> Result<(), String> {
    let previous = previous.map(str::trim).filter(|value| !value.is_empty());
    let next = next.map(str::trim).filter(|value| !value.is_empty());
    if previous == next {
        return Ok(());
    }
    if let Some(shortcut) = previous {
        app.global_shortcut()
            .unregister(shortcut)
            .map_err(|error| format!("Could not unregister the old shortcut: {error}"))?;
    }
    if let Some(shortcut) = next {
        if let Err(error) = app.global_shortcut().register(shortcut) {
            let mut message = format!("Could not register that shortcut: {error}");
            if let Some(previous) = previous {
                if let Err(rollback) = app.global_shortcut().register(previous) {
                    message.push_str(&format!(
                        " The previous shortcut could not be restored: {rollback}"
                    ));
                }
            }
            return Err(message);
        }
    }
    Ok(())
}

async fn deliver_notifications(app: &AppHandle, engine: &AppEngine, notified: &Mutex<AlertLog>) {
    for notification in engine.notification_candidates().await {
        let should_deliver = match notified.lock() {
            Ok(mut delivered) => delivered.should_deliver(&notification),
            Err(error) => {
                tracing::error!(%error, "notification deduplication state is unavailable");
                false
            }
        };
        if should_deliver {
            if let Err(error) = app
                .notification()
                .builder()
                .title(notification.title)
                .body(notification.body)
                .show()
            {
                tracing::warn!(%error, "could not show usage notification");
            }
        }
    }
}

#[derive(Clone)]
struct LogWriter(Arc<Mutex<std::fs::File>>);

impl Write for LogWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("log lock poisoned"))?
            .write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("log lock poisoned"))?
            .flush()
    }
}

fn init_logging() -> anyhow::Result<()> {
    let path = paths::log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.len() > 2 * 1_024 * 1_024)
    {
        let old_path = path.with_extension("log.old");
        match std::fs::remove_file(&old_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::fs::rename(&path, old_path)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let file = Arc::new(Mutex::new(file));
    tracing_subscriber::fmt()
        .with_env_filter("multimeters=info,multimeters_core=info,warn")
        .with_target(true)
        .with_ansi(false)
        .with_writer(move || LogWriter(Arc::clone(&file)))
        .init();
    Ok(())
}

pub fn run() {
    if let Err(error) = init_logging() {
        eprintln!("MultiMeters could not initialize file logging: {error}");
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_flyout(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _, event| {
                    if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        toggle_flyout(app);
                    }
                })
                .build(),
        )
        .setup(|app| {
            let engine = AppEngine::new()?;
            let settings = tauri::async_runtime::block_on(engine.settings());
            if let Err(error) = set_autostart(app.handle(), settings.launch_at_login) {
                tracing::warn!(%error, "could not reconcile Launch at Login during startup");
            }
            if let Some(shortcut) = settings
                .global_shortcut
                .as_deref()
                .map(str::trim)
                .filter(|shortcut| !shortcut.is_empty())
            {
                if let Err(error) = app.global_shortcut().register(shortcut) {
                    tracing::warn!(%error, %shortcut, "could not register configured global shortcut");
                }
            }

            let seed_engine = Arc::clone(&engine);
            let seed_app = app.handle().clone();
            let notified = Arc::new(Mutex::new(AlertLog::default()));
            let hide_on_blur = Arc::new(AtomicBool::new(settings.hide_on_blur));
            let seed_notified = Arc::clone(&notified);
            tauri::async_runtime::spawn(async move {
                seed_engine.seed_if_needed().await;
                seed_engine.refresh_all(true).await;
                deliver_notifications(&seed_app, &seed_engine, &seed_notified).await;
                update_tray_tooltip(&seed_app, &seed_engine).await;
                warn_result(
                    seed_app.emit("dashboard-updated", ()),
                    "notify the flyout after initial refresh",
                );
            });

            let api_engine = Arc::clone(&engine);
            tauri::async_runtime::spawn(async move {
                api::start(api_engine).await;
            });

            let loop_engine = Arc::clone(&engine);
            let loop_app = app.handle().clone();
            let loop_notified = Arc::clone(&notified);
            tauri::async_runtime::spawn(async move {
                loop {
                    // Wake at least every REFRESH_TICK_SECS so a shortened refresh interval takes
                    // effect promptly instead of waiting out the interval that was in force when
                    // this iteration began.
                    let due_in = loop_engine.secs_until_refresh().await.max(0) as u64;
                    tokio::time::sleep(std::time::Duration::from_secs(
                        due_in.clamp(1, REFRESH_TICK_SECS),
                    ))
                    .await;
                    if !loop_engine.refresh_all(false).await {
                        // Nothing was due after all — a manual refresh or a still-fresh cache got
                        // there first. Wait out a tick rather than re-asking straight away.
                        tokio::time::sleep(std::time::Duration::from_secs(REFRESH_TICK_SECS)).await;
                        continue;
                    }
                    deliver_notifications(&loop_app, &loop_engine, &loop_notified).await;
                    update_tray_tooltip(&loop_app, &loop_engine).await;
                    warn_result(
                        loop_app.emit("dashboard-updated", ()),
                        "notify the flyout after background refresh",
                    );
                }
            });
            app.manage(State {
                engine,
                notified,
                hide_on_blur: Arc::clone(&hide_on_blur),
            });

            let show = MenuItem::with_id(app, "show", "Open MultiMeters", true, None::<&str>)?;
            let settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &settings, &quit])?;
            let icon = app
                .default_window_icon()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("the application icon is missing"))?;

            TrayIconBuilder::with_id(TRAY_ID)
                .icon(icon)
                .tooltip("MultiMeters")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_screen(app, "dash"),
                    "settings" => show_screen(app, "settings"),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_flyout(tray.app_handle());
                    }
                })
                .build(app)?;

            if let Some(window) = app.get_webview_window("main") {
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| match event {
                    tauri::WindowEvent::Focused(false) => {
                        if !hide_on_blur.load(Ordering::Relaxed) {
                            return;
                        }
                        if let Some(window) = app_handle.get_webview_window("main") {
                            warn_result(window.hide(), "hide the unfocused flyout");
                        }
                    }
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        if let Some(window) = app_handle.get_webview_window("main") {
                            warn_result(window.hide(), "hide the closed flyout");
                        }
                    }
                    _ => {}
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_dashboard,
            refresh_all,
            refresh_one,
            get_settings,
            patch_settings,
            set_api_key,
            reset_all_settings,
            get_customize,
            toggle_pin,
            reveal_log,
            hide_flyout
        ])
        .run(tauri::generate_context!())
        .expect("error while running MultiMeters");
}
