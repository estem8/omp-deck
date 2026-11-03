//! OmpDeck: desktop wrapper for the omp coding-agent TUI.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod activate;
mod advisor_page;
mod agent_config;
mod agent_ctl;
mod agent_hub;
mod agent_model;
mod agent_output;
mod agent_popup;
mod app;
mod app_update;
mod backup_view;
mod badge;
mod card;
mod changes_view;
mod chat_view;
mod cli;
mod clipboard_bridge;
mod col_alloc;
mod confirm;
mod cost_tip;
mod ctx_tip;
mod dashboard;
mod dashboard_model;
mod deco;
mod file_drop;
mod folder_scope;
mod gw;
mod icons;
mod mobile_view;
mod model_order;
mod model_picker;
mod notify;
mod notify_panel;
mod notify_view;
mod omp_rows;
mod omp_settings;
mod paint_cache;
mod palette;
mod panes;
mod plugins_page;
mod resume_model;
mod review;
mod running_model;
mod running_view;
mod scope_bar;
mod sessions_model;
mod sessions_view;
mod settings_page;
mod sidebar;
mod statusbar;
mod summary;
mod sync_view;
mod tabs;
mod theme;
mod tips;
mod titlebar;
mod toast;
mod transcript_view;
mod transfer;
mod tree_layout;
mod tree_view;
mod ui_fonts;
mod update;
mod usage_fmt;
mod usage_resets;
mod usage_state;
mod usage_view;
mod widgets;
mod worktree_dialog;
mod worktree_ui;

use anyhow::anyhow;
use deck_core::config::{AppConfig, config_dir};

use crate::app::{App, AppInit};
use crate::cli::Parsed;

const MIN_SIZE: [f32; 2] = [640.0, 400.0];

/// The 256px window icon embedded at build time.
fn window_icon() -> egui::IconData {
    static PNG: &[u8] = include_bytes!("../assets/icon-256.png");
    eframe::icon_data::from_png_bytes(PNG).unwrap_or_default()
}

fn viewport(cfg: &AppConfig) -> egui::ViewportBuilder {
    let mut vb = egui::ViewportBuilder::default()
        .with_title("OmpDeck")
        .with_app_id("omp-deck")
        .with_icon(window_icon())
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size(MIN_SIZE)
        .with_decorations(cfg.native_decorations);
    if let Some(w) = cfg.window
        && w.width.is_finite()
        && w.height.is_finite()
    {
        vb = vb.with_inner_size([w.width.max(MIN_SIZE[0]), w.height.max(MIN_SIZE[1])]);
        if let (Some(x), Some(y)) = (w.x, w.y)
            && x.is_finite()
            && y.is_finite()
        {
            vb = vb.with_position([x, y]);
        }
        if w.maximized {
            vb = vb.with_maximized(true);
        }
    }
    vb
}

/// Logging. A Windows release build is a GUI-subsystem process without a console, so it logs to
/// `<cache>/omp-deck/omp-deck.log`; everything else logs to stderr.
fn init_logging() {
    #[cfg(all(windows, not(debug_assertions)))]
    if let Some(file) = log_file() {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .target(env_logger::Target::Pipe(Box::new(file)))
            .init();
        return;
    }
    env_logger::init();
}

#[cfg(all(windows, not(debug_assertions)))]
fn log_file() -> Option<std::fs::File> {
    let dir = dirs::cache_dir()?.join("omp-deck");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("omp-deck.log"))
        .ok()
}

/// A failure before or while opening the window: stderr, the log, and (Windows release, no
/// console) a message box.
fn report_fatal(err: &anyhow::Error) {
    eprintln!("omp-deck: {err:#}");
    log::error!("fatal: {err:#}");
    #[cfg(all(windows, not(debug_assertions)))]
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("OmpDeck")
        .set_description(format!("{err:#}"))
        .show();
}

fn main() -> anyhow::Result<()> {
    // `omp-deck --gateway <command>`: the gateway command line, used when no `omp-deck-gateway`
    // sits next to this executable. First, before logging and any thread: the gateway sets up
    // its own log and its `serve` forks.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(deck_service::gateway::GATEWAY_FLAG) {
        std::process::exit(deck_service::gateway::cli::main(args));
    }
    init_logging();
    #[cfg(windows)]
    notify::init_process();
    if let Err(e) = run() {
        report_fatal(&e);
        std::process::exit(1);
    }
    Ok(())
}

fn run() -> anyhow::Result<()> {
    let cli = match cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(cli)) => cli,
        Ok(Parsed::Help) => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Err(msg) => {
            eprintln!("omp-deck: {msg}\n\n{}", cli::HELP);
            std::process::exit(2);
        }
    };

    // The profile this window is a client of. `--sessions-root` selects another sessions store;
    // the gateway serving that profile owns the scans and backends for it.
    let mut profile = deck_service::gateway::Profile::default_for_user();
    if let Some(root) = &cli.sessions_root {
        profile.sessions_root.clone_from(root);
    }
    if let Some(ticket) = &cli.activate {
        return activate::run(ticket, profile);
    }

    // Native window geometry and appearance are read once, read-only, before the window exists.
    // Every later setting goes through the gateway's settings service (the single writer).
    let cfg = AppConfig::load();
    log::debug!("config dir: {}", config_dir().display());

    // One option set per renderer; `viewport` is rebuilt because the builder is consumed.
    let cfg_for_window = cfg.clone();
    let options = move |renderer| eframe::NativeOptions {
        viewport: viewport(&cfg_for_window),
        clipboard_shortcuts: false,
        // deck-term installs its own filtered system provider (it skips `term-primary`, which must stay primary-only).
        system_font_fallback: false,
        renderer,
        ..Default::default()
    };
    // Blocking filesystem checks are fine here: the window does not exist yet.
    let start_dir = std::env::current_dir().unwrap_or_default();
    let folder = cli.folder.as_ref().map(|f| {
        folder_scope::resolve(
            &f.to_string_lossy(),
            dirs::home_dir().as_deref(),
            &start_dir,
        )
    });
    let init = AppInit {
        cli,
        cfg,
        profile,
        ui_fonts: ui_fonts::UiFonts::load(),
        folder,
        start_dir,
    };
    let init = std::sync::Arc::new(std::sync::Mutex::new(Some(init)));
    let result = run_window(options, init);
    // Terminal drops hand tmux detaches to background threads; let them finish before exiting.
    deck_term::wait_detached(std::time::Duration::from_secs(2));
    // A self-update restart: the old process is gone (state flushed, sessions detached).
    app_update::spawn_successor();
    result
}

/// Opens the window and runs the app. Windows: wgpu first (DX12; a software adapter when there
/// is no GPU, as on VMs, RDP sessions and CI runners where OpenGL is only GDI 1.1), glow when
/// wgpu cannot start. Elsewhere: glow only. `init` is taken by whichever attempt reaches the
/// app-creation step, so a renderer that fails earlier leaves it for the next one.
fn run_window(
    options: impl Fn(eframe::Renderer) -> eframe::NativeOptions,
    init: std::sync::Arc<std::sync::Mutex<Option<AppInit>>>,
) -> anyhow::Result<()> {
    let attempt = |renderer: eframe::Renderer| {
        let init = init.clone();
        eframe::run_native(
            "OmpDeck",
            options(renderer),
            Box::new(move |cc| {
                let init = init
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take())
                    .ok_or("the app was already created")?;
                Ok(Box::new(App::new(cc, init)))
            }),
        )
    };
    #[cfg(windows)]
    {
        match attempt(eframe::Renderer::Wgpu) {
            Ok(()) => Ok(()),
            Err(wgpu_err) => {
                log::error!("wgpu renderer failed: {wgpu_err}; trying OpenGL");
                attempt(eframe::Renderer::Glow).map_err(|glow_err| {
                    anyhow!("no usable renderer: wgpu: {wgpu_err}; OpenGL: {glow_err}")
                })
            }
        }
    }
    #[cfg(not(windows))]
    attempt(eframe::Renderer::Glow).map_err(|e| anyhow!("eframe: {e}"))
}
