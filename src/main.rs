#![windows_subsystem = "windows"]

mod claude_cli;
mod claude_desktop;
mod compact_layout;
mod compact_view;
mod diagnose;
mod http_client;
mod localization;
mod models;
mod native_interop;
mod placement;
mod poller;
mod provider_tile;
mod settings;
mod theme;
mod tray_icon;
mod updater;
mod window;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Diagnostics are always on (append + rotation, see diagnose.rs); the old
    // --diagnose flag is accepted but no longer required.
    match diagnose::init() {
        Ok(path) => diagnose::log(format!("startup args={args:?} log_path={}", path.display())),
        Err(error) => {
            // Logging may not be available yet, but keep startup behavior unchanged.
            let _ = error;
        }
    }

    // Any panic must leave a trace in the diagnostic log; with the default
    // hook the process just vanished (stderr is invisible in a GUI subsystem).
    std::panic::set_hook(Box::new(|info| {
        diagnose::log(format!("PANIC: {info}"));
    }));

    if let Some(exit_code) = updater::handle_cli_mode(&args) {
        diagnose::log(format!("cli mode exited with code {exit_code}"));
        std::process::exit(exit_code);
    }
    let startup_notice = updater::winget_failure_notice(&args);

    // Explicit, read-only support command. Background recovery may run only
    // `claude update`; `claude auth status` remains user-triggered here.
    if args.iter().any(|arg| arg == "--claude-auth-diagnostics") {
        let report = poller::claude_auth_diagnostics_report();
        println!("{report}");
        diagnose::log(format!("user-triggered Claude auth diagnostics:\n{report}"));
        std::process::exit(0);
    }

    // Diagnostic: render every tray icon state to BMP files and exit.
    if let Some(pos) = args.iter().position(|arg| arg == "--dump-tray-icons") {
        let dir = args
            .get(pos + 1)
            .cloned()
            .unwrap_or_else(|| ".".to_string());
        std::process::exit(tray_icon::dump_icons(&dir));
    }

    // Diagnostic: render the detail popup with representative data to a BMP
    // and exit. Used to eyeball popup layout changes and to render README
    // previews. Optional tokens after the directory select the fixture
    // language (`en`/`zh`, default `zh`), force a theme (`dark`/`light`,
    // default: follow the system theme), and select a credential fixture
    // (`auth-update`/`auth-login`, default: representative usage).
    if let Some(pos) = args.iter().position(|arg| arg == "--dump-detail-popup") {
        let dir = args
            .get(pos + 1)
            .cloned()
            .unwrap_or_else(|| ".".to_string());
        let mut english = false;
        let mut force_dark = None;
        let mut fixture = window::DetailPopupDumpFixture::Usage;
        for token in args.iter().skip(pos + 2) {
            match token.to_ascii_lowercase().as_str() {
                "en" => english = true,
                "zh" | "zh-cn" => english = false,
                "dark" => force_dark = Some(true),
                "light" => force_dark = Some(false),
                "auth-update" => fixture = window::DetailPopupDumpFixture::ClaudeUpdate,
                "auth-login" => fixture = window::DetailPopupDumpFixture::ClaudeLogin,
                _ => break,
            }
        }
        std::process::exit(window::dump_detail_popup(
            &dir, english, force_dark, fixture,
        ));
    }

    // Diagnostic: render both compact surfaces with representative data and
    // exit. This is the fast visual gate before launching a live Debug build.
    if let Some(pos) = args
        .iter()
        .position(|arg| arg == "--dump-widget" || arg == "--dump-compact-surfaces")
    {
        let dir = args
            .get(pos + 1)
            .cloned()
            .unwrap_or_else(|| ".".to_string());
        std::process::exit(window::dump_widget(&dir));
    }

    let Some(instance_guard) = window::acquire_single_instance() else {
        return;
    };

    diagnose::log("entering window::run");
    window::run(instance_guard, startup_notice);
}
