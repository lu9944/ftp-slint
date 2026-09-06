use anyhow::Context;
use ftp_slint::app::App;
use ftp_slint::bridge::MainWindow;
use ftp_slint::ftp::types::LogDir;
use ftp_slint::store;
use slint::ComponentHandle;
use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn main() -> anyhow::Result<()> {
    let _log_guard = init_logging();
    tracing::info!("ftp-slint 启动");
    store::env::load();

    let ui = MainWindow::new().context("创建主窗口失败")?;
    let app = App::install(&ui);

    prefill_from_env(&ui);
    app.open_initial_local();
    install_drag_poc(&ui, &app);

    ui.run().context("UI 事件循环异常退出")?;
    Ok(())
}

fn prefill_from_env(ui: &MainWindow) {
    let Some(env) = store::env::ftp_env() else {
        return;
    };
    ui.set_host(env.host.into());
    ui.set_port(env.port.to_string().into());
    ui.set_username(env.user.into());
    let source = if store::env::sandbox_present() {
        "沙盒"
    } else {
        "环境"
    };
    ui.set_status_text(format!("就绪（已从 {source} 配置预填）").into());
}

fn install_drag_poc(ui: &MainWindow, app: &App) {
    let app = app.clone();
    ui.window()
        .on_winit_window_event(move |_slint_window, event| {
            if let winit::event::WindowEvent::DroppedFile(path) = event {
                app.log(
                    LogDir::Info,
                    format!("OS 拖入 PoC：收到文件 {}", path.display()),
                );
            }
            EventResult::Propagate
        });
}

fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = std::fs::create_dir_all("logs");
    let appender = tracing_appender::rolling::daily("logs", "ftp-slint.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer);
    let console = tracing_subscriber::fmt::layer();
    tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file_layer)
        .init();
    guard
}
