#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use tauri::{Emitter, Manager};

#[cfg(target_os = "linux")]
use voice_runtime::service::{Runtime, Snapshot, UiCommand};
struct UiSmokeState {
    enabled: bool,
    delivery: bool,
}

#[tauri::command]
fn ui_smoke_report(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    smoke: tauri::State<'_, UiSmokeState>,
    report: serde_json::Value,
) -> Result<(), String> {
    if !cfg!(debug_assertions) || !smoke.enabled || window.label() != "main" {
        return Err("此检查仅用于显式开发验证".into());
    }
    let fields: &[&str] = if smoke.delivery {
        &[
            "disabled_before_probe",
            "native_input_readback",
            "enabled_after_probe",
            "operation_finished",
        ]
    } else {
        &[
            "ipc_snapshot",
            "three_checks",
            "record_disabled_before_setup",
            "settings_navigation",
            "default_values",
        ]
    };
    let passed = fields
        .iter()
        .all(|key| report.get(key).and_then(|v| v.as_bool()) == Some(true));
    let checks: serde_json::Map<String, serde_json::Value> = fields
        .iter()
        .map(|key| {
            (
                key.to_string(),
                serde_json::json!(report.get(key).and_then(|v| v.as_bool()).unwrap_or(false)),
            )
        })
        .collect();
    let diagnostics: serde_json::Map<String, serde_json::Value> = if smoke.delivery {
        [
            "trusted_input_seen",
            "trusted_paste_seen",
            "dom_input_focused",
            "document_focused",
            "ui_dispatch_failed",
            "ui_mismatch",
        ]
        .into_iter()
        .map(|key| {
            (
                key.to_owned(),
                serde_json::json!(
                    report
                        .get(key)
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false)
                ),
            )
        })
        .collect()
    } else {
        serde_json::Map::new()
    };
    println!(
        "{}",
        serde_json::json!({"scope":if smoke.delivery {"actual own-window synthetic native paste/readback through production typed UI flow; no microphone or other-window delivery"}else{"actual Tauri WebKit DOM/IPC developer smoke; no recording or injection"},"checks":checks,"diagnostics":diagnostics,"passed":passed})
    );
    app.exit(if passed { 0 } else { 1 });
    Ok(())
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn snapshot(runtime: tauri::State<'_, Runtime>) -> Result<Snapshot, voice_core::AppError> {
    runtime.snapshot()
}
#[cfg(target_os = "linux")]
#[tauri::command]
async fn execute(
    runtime: tauri::State<'_, Runtime>,
    command: UiCommand,
) -> Result<serde_json::Value, voice_core::AppError> {
    runtime.execute(command)
}
#[cfg(target_os = "linux")]
#[tauri::command]
fn diagnostics(
    runtime: tauri::State<'_, Runtime>,
) -> Result<serde_json::Value, voice_core::AppError> {
    let state = runtime.snapshot()?;
    let delivery = if state.capabilities.automatic_delivery_verified {
        "verified for this application session; each target is still guarded"
    } else {
        "manual until explicit own-window verification"
    };
    let warnings:Vec<_>=state.warnings.iter().map(|warning|serde_json::json!({"code":warning.code,"stage":warning.stage,"retryable":warning.retryable})).collect();
    Ok(
        serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"architecture":std::env::consts::ARCH,"platform":state.capabilities,"phase":state.state.phase,"operation_busy":state.operation_busy,"inference_backend":"cpu","automatic_delivery":delivery,"warnings":warnings}),
    )
}
#[tauri::command]
fn download_info(model_id: String) -> Result<serde_json::Value, String> {
    voice_runtime::models::catalog().into_iter().find(|m|m.id==model_id).map(|m|serde_json::json!({"id":m.id,"name":m.filename,"bytes":m.bytes,"source":m.source,"license":m.license})).ok_or_else(||"模型不在内置清单中".into())
}

#[cfg(target_os = "linux")]
fn main() {
    let ui_smoke = cfg!(debug_assertions) && std::env::args().any(|arg| arg == "--ui-smoke");
    let delivery_smoke =
        cfg!(debug_assertions) && std::env::args().any(|arg| arg == "--delivery-smoke");
    let socket = match voice_platform::ipc::SessionSocket::bind() {
        Ok(socket) => socket,
        Err(_) => {
            eprintln!("声笺已运行，或当前登录会话的私有控制接口不可用。");
            std::process::exit(1);
        }
    };
    let app = tauri::Builder::default()
        .manage(UiSmokeState { enabled: ui_smoke || delivery_smoke, delivery: delivery_smoke })
        .setup(move |app| {
            let handle = app.handle().clone();
            let menu = tauri::menu::Menu::with_items(
                app,
                &[
                    &tauri::menu::MenuItem::with_id(
                        app,
                        "settings",
                        "打开声笺",
                        true,
                        None::<&str>,
                    )?,
                    &tauri::menu::MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?,
                ],
            )?;
            let mut tray = tauri::tray::TrayIconBuilder::with_id("status")
                .tooltip("声笺 · 准备就绪")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "settings" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            let tray_available = tray.build(app).is_ok();
            let idle_icon=app.default_window_icon().map(|icon|tauri::image::Image::new_owned(icon.rgba().to_vec(),icon.width(),icon.height()));
            let recording_icon=idle_icon.as_ref().map(|icon|{
                let width=icon.width();let height=icon.height();let mut pixels=icon.rgba().to_vec();
                let radius=(width.min(height)/5).max(2) as i64;let cx=width as i64-radius-2;let cy=radius+2;
                for y in 0..height{for x in 0..width{if (x as i64-cx).pow(2)+(y as i64-cy).pow(2)<=radius.pow(2){let index=((y*width+x)*4) as usize;pixels[index..index+4].copy_from_slice(&[221,84,56,255]);}}}
                tauri::image::Image::new_owned(pixels,width,height)
            });
            let data = app.path().app_data_dir()?;
            let executable = std::env::current_exe()?;
            let worker_dir = executable
                .parent()
                .ok_or("executable directory missing")?
                .to_path_buf();
            #[cfg(debug_assertions)]
            let worker_dir = if worker_dir.join("voice-asr-worker").exists() {
                worker_dir
            } else {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../build/workers")
                    .canonicalize()?
            };
            let runtime = Runtime::start(data, worker_dir, socket, move |state| {
                let _ = handle.emit("state_changed", state);
                let label = match state.phase {
                    voice_core::Phase::Recording => "声笺 · 正在录音",
                    voice_core::Phase::Transcribing => "声笺 · 正在转写",
                    voice_core::Phase::Rewriting => "声笺 · 正在整理",
                    voice_core::Phase::Delivering => "声笺 · 正在交付",
                    _ => "声笺 · 准备就绪",
                };
                if let Some(tray) = handle.tray_by_id("status") {
                    let _ = tray.set_tooltip(Some(label));
                    let _=tray.set_icon(if state.phase==voice_core::Phase::Recording{recording_icon.clone()}else{idle_icon.clone()});
                }
                if state.phase == voice_core::Phase::Recording {
                    std::thread::spawn(|| {
                        let _ = std::process::Command::new("notify-send")
                            .args([
                                "--app-name=声笺",
                                "正在录音",
                                "再次使用快捷键停止，最长 60 秒。",
                            ])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                    });
                }
            })?;
            app.manage(runtime);
            if ui_smoke && let Some(window)=app.get_webview_window("main") {
                window.eval(r#"(()=>{const timer=setInterval(()=>{const status=document.getElementById('phase');if(!status||status.textContent==='正在连接')return;clearInterval(timer);const report={ipc_snapshot:true,three_checks:document.querySelectorAll('.check-item').length===3,record_disabled_before_setup:document.getElementById('start').disabled,default_values:document.getElementById('max-seconds').value==='60'&&document.getElementById('llm-mode').value==='local'};document.querySelector('[data-panel="settings"]').click();report.settings_navigation=!document.getElementById('settings').hidden;window.__TAURI__.core.invoke('ui_smoke_report',{report});},100);})()"#)?;
                let handle=app.handle().clone();
                std::thread::spawn(move||{std::thread::sleep(std::time::Duration::from_secs(15));eprintln!("ui_smoke_timeout");handle.exit(1);});
            }
            if delivery_smoke && let Some(window) = app.get_webview_window("main") {
                window.show()?;
                window.set_focus()?;
                window.eval(include_str!("../../scripts/ui-delivery-smoke.js"))?;
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(20));
                    eprintln!("delivery_smoke_timeout");
                    handle.exit(1);
                });
            }
            if tray_available && let Some(window) = app.get_webview_window("main") {
                let owned = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = owned.hide();
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            execute,
            diagnostics,
            download_info,
            ui_smoke_report
        ])
        .build(tauri::generate_context!())
        .expect("声笺初始化失败，请检查本机数据与依赖");
    app.run(|handle, event| {
        if matches!(event, tauri::RunEvent::Exit)
            && let Some(runtime) = handle.try_state::<Runtime>()
        {
            runtime.shutdown();
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("此构建的桌面运行时目前仅完成 Linux 集成，其他平台仍待适配验收。");
}
