use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use crate::bridge::{FileRow, MainWindow, TaskRow};
use crate::core::local as local_fs;
use crate::core::queue::{Direction, Emitter, QueueCommand, Scheduler, TransferRequest};
use crate::ftp::client::FtpSession;
use crate::ftp::types::{
    ConnectConfig, FtpCommand, FtpEvent, LocalFsCommand, LogDir, Outcome, join_remote,
    parent_remote,
};

const LOG_MAX: usize = 500;

#[derive(Clone)]
pub struct App {
    ui: Weak<MainWindow>,
    ftp_tx: Sender<FtpCommand>,
    local_tx: Sender<LocalFsCommand>,
    sink: EventSink,
    next_task_id: Arc<AtomicU64>,
}

impl App {
    pub fn install(ui: &MainWindow) -> Self {
        let (ftp_tx, ftp_rx) = channel();
        let (local_tx, local_rx) = channel();
        let (queue_tx, queue_rx) = channel::<QueueCommand>();
        let sink = EventSink { ui: ui.as_weak() };

        let emitter: Emitter = sink.emitter();
        Scheduler::spawn(emitter, queue_tx.clone(), queue_rx);

        let control_sink = sink.clone();
        let control_queue_tx = queue_tx.clone();
        let _ = thread::Builder::new()
            .name("ftp-control".into())
            .spawn(move || {
                control_worker(control_sink, ftp_rx, control_queue_tx);
            });
        let local_sink = sink.clone();
        let _ = thread::Builder::new()
            .name("local-io".into())
            .spawn(move || {
                local_worker(local_sink, local_rx);
            });

        ui.set_remote_files(ModelRc::default());
        ui.set_local_files(ModelRc::default());
        ui.set_log_lines(ModelRc::from(Rc::new(VecModel::<SharedString>::default())));
        ui.set_tasks(ModelRc::from(Rc::new(VecModel::<TaskRow>::default())));

        let app = Self {
            ui: ui.as_weak(),
            ftp_tx,
            local_tx,
            sink,
            next_task_id: Arc::new(AtomicU64::new(1)),
        };
        install_callbacks(ui, &app);
        app
    }

    pub fn connect(&self, cfg: ConnectConfig) {
        let _ = self.ftp_tx.send(FtpCommand::Connect(cfg));
    }

    pub fn disconnect(&self) {
        let _ = self.ftp_tx.send(FtpCommand::Quit);
    }

    pub fn ping(&self) {
        let _ = self.ftp_tx.send(FtpCommand::Ping);
    }

    pub fn remote_navigate(&self, path: String) {
        let _ = self.ftp_tx.send(FtpCommand::Navigate { path });
    }

    pub fn remote_refresh(&self, path: String) {
        let _ = self.ftp_tx.send(FtpCommand::List { path });
    }

    pub fn local_list(&self, path: PathBuf) {
        let _ = self.local_tx.send(LocalFsCommand::List { path });
    }

    pub fn open_initial_local(&self) {
        let start = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map(PathBuf::from)
            .ok()
            .filter(|p| p.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.local_list(start);
    }

    pub fn log(&self, dir: LogDir, text: impl Into<String>) {
        self.sink.send(FtpEvent::LogLine {
            dir,
            text: text.into(),
        });
    }

    pub fn download_remote_file(&self, remote_dir: &str, name: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let local_dir = PathBuf::from(ui.get_local_path().to_string());
        let local = local_fs::join(&local_dir, name);
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        let req = TransferRequest {
            task_id,
            direction: Direction::Download,
            remote: join_remote(remote_dir, name),
            local,
            size: 0,
        };
        if let Some(model) = ui.get_tasks().as_any().downcast_ref::<VecModel<TaskRow>>() {
            model.push(TaskRow {
                task_id: i32::try_from(task_id).unwrap_or(0),
                name: SharedString::from(name),
                direction: SharedString::from("↓ 下载"),
                size_str: SharedString::from("…"),
                progress: 0.0,
                progress_str: SharedString::from("0%"),
                speed_str: SharedString::from("—"),
                status: SharedString::from("等待"),
            });
        }
        let _ = self
            .ftp_tx
            .send(FtpCommand::EnqueueTransfers { tasks: vec![req] });
        self.log(LogDir::Info, format!("已加入下载队列：{name}"));
    }

    pub fn upload_local_file(&self, local_dir: &str, name: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let remote_dir = ui.get_remote_path().to_string();
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        let req = TransferRequest {
            task_id,
            direction: Direction::Upload,
            remote: join_remote(&remote_dir, name),
            local: local_fs::join(&PathBuf::from(local_dir), name),
            size: 0,
        };
        if let Some(model) = ui.get_tasks().as_any().downcast_ref::<VecModel<TaskRow>>() {
            model.push(TaskRow {
                task_id: i32::try_from(task_id).unwrap_or(0),
                name: SharedString::from(name),
                direction: SharedString::from("↑ 上传"),
                size_str: SharedString::from("…"),
                progress: 0.0,
                progress_str: SharedString::from("0%"),
                speed_str: SharedString::from("—"),
                status: SharedString::from("等待"),
            });
        }
        let _ = self
            .ftp_tx
            .send(FtpCommand::EnqueueTransfers { tasks: vec![req] });
        self.log(LogDir::Info, format!("已加入上传队列：{name}"));
    }

    pub fn cancel_task(&self, task_id: i32) {
        let _ = self.ftp_tx.send(FtpCommand::CancelTask {
            task_id: u64::try_from(task_id).unwrap_or(u64::MAX),
        });
    }

    pub fn remote_mkdir(&self, base: &str, name: &str) {
        if !self.valid_target(base, name) {
            return;
        }
        let _ = self.ftp_tx.send(FtpCommand::Mkdir {
            path: join_remote(base, name),
        });
    }

    pub fn remote_delete(&self, base: &str, name: &str) {
        if !self.valid_target(base, name) {
            return;
        }
        let _ = self.ftp_tx.send(FtpCommand::Delete {
            paths: vec![join_remote(base, name)],
        });
    }

    pub fn remote_rename(&self, base: &str, from: &str, to: &str) {
        if !self.valid_target(base, from) || !self.valid_target(base, to) || from == to {
            return;
        }
        let _ = self.ftp_tx.send(FtpCommand::Rename {
            from: join_remote(base, from),
            to: join_remote(base, to),
        });
    }

    pub fn local_mkdir(&self, base: &str, name: &str) {
        if !self.valid_target(base, name) {
            return;
        }
        let _ = self.local_tx.send(LocalFsCommand::Mkdir {
            path: local_fs::join(&PathBuf::from(base), name),
        });
    }

    pub fn local_delete(&self, base: &str, name: &str) {
        if !self.valid_target(base, name) {
            return;
        }
        let _ = self.local_tx.send(LocalFsCommand::Delete {
            paths: vec![local_fs::join(&PathBuf::from(base), name)],
        });
    }

    pub fn local_rename(&self, base: &str, from: &str, to: &str) {
        if !self.valid_target(base, from) || !self.valid_target(base, to) || from == to {
            return;
        }
        let _ = self.local_tx.send(LocalFsCommand::Rename {
            from: local_fs::join(&PathBuf::from(base), from),
            to: local_fs::join(&PathBuf::from(base), to),
        });
    }

    fn valid_target(&self, base: &str, name: &str) -> bool {
        if base.is_empty() {
            self.log(LogDir::Error, "尚未就绪：请先浏览到有效目录");
            return false;
        }
        let name = name.trim();
        if name.is_empty() || name == "." || name == ".." {
            return false;
        }
        true
    }

    pub fn clear_finished_tasks(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let tasks_model = ui.get_tasks();
        let Some(model) = tasks_model.as_any().downcast_ref::<VecModel<TaskRow>>() else {
            return;
        };
        for i in (0..model.row_count()).rev() {
            let finished = model
                .row_data(i)
                .is_some_and(|r| matches!(r.status.as_str(), "已完成" | "失败" | "已取消"));
            if finished {
                model.remove(i);
            }
        }
    }
}

#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn install_callbacks(ui: &MainWindow, app: &App) {
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_quick_connect(move || {
            let Some(ui) = weak.upgrade() else { return };
            let host = ui.get_host().trim().to_string();
            if host.is_empty() {
                ui.set_status_text("请输入主机地址".into());
                return;
            }
            let port: u16 = ui.get_port().trim().parse().unwrap_or(21);
            let user = ui.get_username().trim().to_string();
            let password = ui.get_password().to_string();
            ui.set_status_text(format!("正在连接 {host}:{port} …").into());
            app.connect(ConnectConfig {
                host,
                port,
                user,
                password,
                ..ConnectConfig::default()
            });
        });
    }
    {
        let app = app.clone();
        ui.on_disconnect(move || app.disconnect());
    }
    {
        let app = app.clone();
        ui.on_test_event(move || app.ping());
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_remote_open(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let path = ui.get_remote_path().to_string();
            app.remote_navigate(join_remote(&path, &name));
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_remote_open_file(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let remote_dir = ui.get_remote_path().to_string();
            app.download_remote_file(&remote_dir, &name);
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_remote_refresh(move || {
            let Some(ui) = weak.upgrade() else { return };
            app.remote_refresh(ui.get_remote_path().to_string());
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_remote_up(move || {
            let Some(ui) = weak.upgrade() else { return };
            match parent_remote(&ui.get_remote_path()) {
                Some(parent) => app.remote_navigate(parent),
                None => app.log(LogDir::Info, "已在根目录"),
            }
        });
    }
    {
        let app = app.clone();
        ui.on_remote_goto(move |path| {
            let path = path.trim().to_string();
            if !path.is_empty() {
                app.remote_navigate(path);
            }
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_local_open(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let path = PathBuf::from(ui.get_local_path().to_string());
            app.local_list(local_fs::join(&path, &name));
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_local_open_file(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let local_dir = ui.get_local_path().to_string();
            app.upload_local_file(&local_dir, &name);
        });
    }
    {
        let app = app.clone();
        ui.on_cancel_task(move |task_id| app.cancel_task(task_id));
    }
    {
        let app = app.clone();
        ui.on_clear_finished(move || app.clear_finished_tasks());
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_local_refresh(move || {
            let Some(ui) = weak.upgrade() else { return };
            app.local_list(PathBuf::from(ui.get_local_path().to_string()));
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_local_up(move || {
            let Some(ui) = weak.upgrade() else { return };
            let path = PathBuf::from(ui.get_local_path().to_string());
            match local_fs::parent(&path) {
                Some(parent) => app.local_list(parent),
                None => app.log(LogDir::Info, "已在根目录"),
            }
        });
    }
    {
        let app = app.clone();
        ui.on_local_goto(move |path| {
            let path = path.trim().to_string();
            if !path.is_empty() {
                app.local_list(PathBuf::from(path));
            }
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_remote_mkdir(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let base = ui.get_remote_path().to_string();
            app.remote_mkdir(&base, &name);
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_remote_delete(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let base = ui.get_remote_path().to_string();
            app.remote_delete(&base, &name);
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_remote_rename(move |from, to| {
            let Some(ui) = weak.upgrade() else { return };
            let base = ui.get_remote_path().to_string();
            app.remote_rename(&base, &from, &to);
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_local_mkdir(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let base = ui.get_local_path().to_string();
            app.local_mkdir(&base, &name);
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_local_delete(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let base = ui.get_local_path().to_string();
            app.local_delete(&base, &name);
        });
    }
    {
        let app = app.clone();
        let weak = ui.as_weak();
        ui.on_local_rename(move |from, to| {
            let Some(ui) = weak.upgrade() else { return };
            let base = ui.get_local_path().to_string();
            app.local_rename(&base, &from, &to);
        });
    }
}

#[derive(Clone)]
struct EventSink {
    ui: Weak<MainWindow>,
}

impl EventSink {
    fn send(&self, ev: FtpEvent) {
        let weak = self.ui.clone();
        let result = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_event(&ui, ev);
            }
        });
        if let Err(err) = result {
            tracing::warn!("事件回流失败：{err}");
        }
    }

    fn emitter(&self) -> Emitter {
        let sink = self.clone();
        Arc::new(move |ev| sink.send(ev))
    }
}

fn apply_event(ui: &MainWindow, ev: FtpEvent) {
    match ev {
        FtpEvent::Connected => {
            ui.set_connected(true);
            ui.set_status_text("已连接".into());
            append_log(ui, LogDir::Info, "连接就绪");
        }
        FtpEvent::Listing { path, entries } => {
            let count = entries.len();
            ui.set_remote_path(path.clone().into());
            ui.set_remote_files(rows_model(&entries));
            append_log(
                ui,
                LogDir::Info,
                &format!("远程目录已更新（{count} 项）：{path}"),
            );
        }
        FtpEvent::LocalListing { path, entries } => {
            ui.set_local_path(path.display().to_string().into());
            ui.set_local_files(rows_model(&entries));
        }
        FtpEvent::LogLine { dir, text } => append_log(ui, dir, &text),
        FtpEvent::TaskProgress {
            task_id,
            done,
            total,
            speed,
        } => update_task_row(ui, task_id, done, total, speed),
        FtpEvent::TaskFinished { task_id, outcome } => finish_task_row(ui, task_id, &outcome),
        FtpEvent::Disconnected { reason } => {
            ui.set_connected(false);
            ui.set_status_text("未连接".into());
            append_log(ui, LogDir::Info, &format!("连接已断开：{reason}"));
        }
        FtpEvent::Error { context, message } => {
            append_log(ui, LogDir::Error, &format!("{context}：{message}"));
            ui.set_status_text(format!("错误：{context}").into());
        }
    }
}

fn rows_model(entries: &[crate::ftp::types::FileEntry]) -> ModelRc<FileRow> {
    let rows: Vec<FileRow> = entries
        .iter()
        .map(|e| FileRow {
            name: e.name.clone().into(),
            is_dir: e.is_dir,
            size_str: fmt_size(e.size).into(),
            modified_str: fmt_time(e.modified).into(),
            perms: e.perms.clone().unwrap_or_default().into(),
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

fn append_log(ui: &MainWindow, dir: LogDir, text: &str) {
    let log_model = ui.get_log_lines();
    let Some(model) = log_model.as_any().downcast_ref::<VecModel<SharedString>>() else {
        return;
    };
    model.insert(0, SharedString::from(format!("[{}] {}", dir.tag(), text)));
    if model.row_count() > LOG_MAX {
        model.remove(model.row_count() - 1);
    }
}

#[allow(clippy::cast_precision_loss)]
fn update_task_row(ui: &MainWindow, task_id: u64, done: u64, total: u64, speed: u64) {
    let tasks_model = ui.get_tasks();
    let Some(model) = tasks_model.as_any().downcast_ref::<VecModel<TaskRow>>() else {
        return;
    };
    for i in 0..model.row_count() {
        let Some(row) = model.row_data(i) else {
            continue;
        };
        if u64::try_from(row.task_id) != Ok(task_id) {
            continue;
        }
        let mut row = row;
        row.progress = if total > 0 {
            (done.min(total) as f32) / (total as f32)
        } else {
            0.0
        };
        row.progress_str = done
            .checked_mul(100)
            .and_then(|v| v.checked_div(total))
            .map_or_else(
                || fmt_size(done).into(),
                |pct| SharedString::from(format!("{pct}%")),
            );
        row.speed_str = if speed > 0 {
            SharedString::from(format!("{}/s", fmt_size(speed)))
        } else {
            SharedString::from("—")
        };
        row.status = "传输中".into();
        model.set_row_data(i, row);
        break;
    }
}

fn finish_task_row(ui: &MainWindow, task_id: u64, outcome: &Outcome) {
    let tasks_model = ui.get_tasks();
    let Some(model) = tasks_model.as_any().downcast_ref::<VecModel<TaskRow>>() else {
        return;
    };
    for i in 0..model.row_count() {
        let Some(row) = model.row_data(i) else {
            continue;
        };
        if u64::try_from(row.task_id) != Ok(task_id) {
            continue;
        }
        let mut row = row;
        match outcome {
            Outcome::Completed => {
                row.progress = 1.0;
                row.progress_str = "100%".into();
                row.speed_str = "—".into();
                row.status = "已完成".into();
            }
            Outcome::Failed(_) => {
                row.speed_str = "—".into();
                row.status = "失败".into();
            }
            Outcome::Canceled => {
                row.speed_str = "—".into();
                row.status = "已取消".into();
            }
        }
        model.set_row_data(i, row);
        break;
    }
}

fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn fmt_time(secs: Option<i64>) -> String {
    use chrono::{Local, TimeZone};
    secs.and_then(|s| Local.timestamp_opt(s, 0).single())
        .map_or_else(String::new, |t| t.format("%Y-%m-%d %H:%M").to_string())
}

fn error_event(context: &str, err: impl std::fmt::Display) -> FtpEvent {
    FtpEvent::Error {
        context: context.to_string(),
        message: err.to_string(),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn control_worker(sink: EventSink, rx: Receiver<FtpCommand>, queue_tx: Sender<QueueCommand>) {
    let mut session: Option<FtpSession> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            FtpCommand::Ping => {
                sink.send(FtpEvent::LogLine {
                    dir: LogDir::Info,
                    text: "pong：worker → invoke_from_event_loop 事件回流 OK".to_string(),
                });
            }
            FtpCommand::Connect(cfg) => {
                let host = cfg.host.clone();
                let port = cfg.port;
                sink.send(FtpEvent::LogLine {
                    dir: LogDir::Info,
                    text: format!("正在连接 {host}:{port} …"),
                });
                match FtpSession::connect(&cfg) {
                    Ok(mut s) => {
                        tracing::info!("已连接 {host}:{port}（协商数据模式：{:?}）", s.mode());
                        sink.send(FtpEvent::LogLine {
                            dir: LogDir::Info,
                            text: format!("登录成功：{}", cfg.user),
                        });
                        sink.send(FtpEvent::Connected);
                        let _ = queue_tx.send(QueueCommand::SetConfig(cfg.clone()));
                        refresh_listing(&mut s, &sink);
                        session = Some(s);
                    }
                    Err(err) => {
                        sink.send(error_event("连接", err));
                    }
                }
            }
            FtpCommand::List { path } => {
                with_session(&mut session, &sink, |s| match s.list_dir(Some(&path)) {
                    Ok(entries) => sink.send(FtpEvent::Listing { path, entries }),
                    Err(err) => sink.send(error_event("列目录", err)),
                });
            }
            FtpCommand::Navigate { path } => with_session(&mut session, &sink, |s| {
                if let Err(err) = s.cwd(&path) {
                    sink.send(error_event("进入目录", err));
                    return;
                }
                refresh_listing(s, &sink);
            }),
            FtpCommand::Mkdir { path } => with_session(&mut session, &sink, |s| {
                if let Err(err) = s.mkdir(&path) {
                    sink.send(error_event("新建目录", err));
                    return;
                }
                sink.send(FtpEvent::LogLine {
                    dir: LogDir::Info,
                    text: format!("已创建目录 {path}"),
                });
                refresh_listing(s, &sink);
            }),
            FtpCommand::Delete { paths } => with_session(&mut session, &sink, |s| {
                for path in &paths {
                    if s.remove(path).is_ok() {
                        continue;
                    }
                    if let Err(err) = s.remove_dir(path) {
                        sink.send(error_event("删除", err));
                    }
                }
                refresh_listing(s, &sink);
            }),
            FtpCommand::Rename { from, to } => with_session(&mut session, &sink, |s| {
                if let Err(err) = s.rename(&from, &to) {
                    sink.send(error_event("重命名", err));
                    return;
                }
                sink.send(FtpEvent::LogLine {
                    dir: LogDir::Info,
                    text: format!("已重命名 {from} → {to}"),
                });
                refresh_listing(s, &sink);
            }),
            FtpCommand::Chmod { .. } => sink.send(FtpEvent::Error {
                context: "CHMOD".to_string(),
                message: "将在 M5 提供".to_string(),
            }),
            FtpCommand::EnqueueTransfers { tasks } => {
                let _ = queue_tx.send(QueueCommand::Enqueue(tasks));
            }
            FtpCommand::CancelTask { task_id } => {
                let _ = queue_tx.send(QueueCommand::Cancel(task_id));
            }
            FtpCommand::Quit => {
                if let Some(mut s) = session.take() {
                    let _ = s.quit();
                }
                sink.send(FtpEvent::Disconnected {
                    reason: "已主动断开".to_string(),
                });
            }
        }
    }
}

fn with_session(
    session: &mut Option<FtpSession>,
    sink: &EventSink,
    action: impl FnOnce(&mut FtpSession),
) {
    match session.as_mut() {
        Some(s) => action(s),
        None => sink.send(FtpEvent::Error {
            context: "未连接".to_string(),
            message: "请先连接服务器".to_string(),
        }),
    }
}

fn refresh_listing(session: &mut FtpSession, sink: &EventSink) {
    match session.pwd() {
        Ok(pwd) => match session.list_dir(None) {
            Ok(entries) => sink.send(FtpEvent::Listing { path: pwd, entries }),
            Err(err) => sink.send(error_event("列目录", err)),
        },
        Err(err) => sink.send(error_event("获取当前目录", err)),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn local_worker(sink: EventSink, rx: Receiver<LocalFsCommand>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            LocalFsCommand::List { path } => match local_fs::list_dir(&path) {
                Ok(entries) => sink.send(FtpEvent::LocalListing { path, entries }),
                Err(err) => sink.send(error_event("本地目录", err)),
            },
            LocalFsCommand::Mkdir { path } => {
                if let Err(err) = local_fs::mkdir(&path) {
                    sink.send(error_event("本地新建目录", err));
                    continue;
                }
                refresh_local(&sink, &path);
            }
            LocalFsCommand::Delete { paths } => {
                for path in &paths {
                    if let Err(err) = local_fs::delete(path) {
                        sink.send(error_event("本地删除", err));
                    }
                }
                if let Some(first) = paths.first() {
                    refresh_local(&sink, first);
                }
            }
            LocalFsCommand::Rename { from, to } => {
                if let Err(err) = local_fs::rename(&from, &to) {
                    sink.send(error_event("本地重命名", err));
                    continue;
                }
                refresh_local(&sink, &to);
            }
        }
    }
}

fn refresh_local(sink: &EventSink, path: &std::path::Path) {
    let target = local_fs::parent(path).unwrap_or_else(|| path.to_path_buf());
    match local_fs::list_dir(&target) {
        Ok(entries) => sink.send(FtpEvent::LocalListing {
            path: target,
            entries,
        }),
        Err(err) => sink.send(error_event("本地目录", err)),
    }
}
