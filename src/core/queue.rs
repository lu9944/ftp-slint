//! 传输队列：类型、状态机、并发调度器与下载例程（DEVELOPMENT.md §4.1/§4.4）。

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::core::local as local_fs;
use crate::ftp::client::{FtpSession, SessionError};
use crate::ftp::types::{ConnectConfig, FtpEvent, LogDir, Outcome, split_remote};

pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
pub const READ_BUFFER_SIZE: usize = 64 * 1024;
pub const DEFAULT_CONCURRENCY: usize = 2;

pub type Emitter = Arc<dyn Fn(FtpEvent) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Running,
    Paused,
    Done,
    Failed,
    Canceled,
}

#[derive(Debug, Clone)]
pub struct TransferRequest {
    pub task_id: u64,
    pub direction: Direction,
    pub remote: String,
    pub local: PathBuf,
    pub size: u64,
}

#[derive(Clone, Default)]
pub struct CancelHandle(Arc<AtomicBool>);

impl CancelHandle {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[allow(clippy::unnested_or_patterns)]
pub fn can_transition(from: TaskState, to: TaskState) -> bool {
    use TaskState::{Canceled, Done, Failed, Paused, Pending, Running};
    matches!(
        (from, to),
        (Pending, Running)
            | (Running, Paused)
            | (Paused, Running)
            | (Running, Done)
            | (Running, Failed)
            | (Running, Canceled)
            | (Paused, Canceled)
            | (Failed, Pending)
    )
}

pub enum QueueCommand {
    SetConfig(ConnectConfig),
    Enqueue(Vec<TransferRequest>),
    Cancel(u64),
    TaskDone(u64),
}

pub struct Scheduler {
    emit: Emitter,
    tx: mpsc::Sender<QueueCommand>,
    rx: mpsc::Receiver<QueueCommand>,
    cfg: Option<ConnectConfig>,
    pending: VecDeque<TransferRequest>,
    running: HashMap<u64, CancelHandle>,
    max_concurrent: usize,
}

impl Scheduler {
    pub fn spawn(emit: Emitter, tx: mpsc::Sender<QueueCommand>, rx: mpsc::Receiver<QueueCommand>) {
        let scheduler = Self {
            emit,
            tx,
            rx,
            cfg: None,
            pending: VecDeque::new(),
            running: HashMap::new(),
            max_concurrent: DEFAULT_CONCURRENCY,
        };
        let _ = thread::Builder::new()
            .name("queue-scheduler".into())
            .spawn(|| scheduler.run());
    }

    fn run(mut self) {
        while let Ok(cmd) = self.rx.recv() {
            match cmd {
                QueueCommand::SetConfig(cfg) => self.cfg = Some(cfg),
                QueueCommand::Enqueue(tasks) => {
                    if self.cfg.is_none() {
                        (self.emit)(FtpEvent::Error {
                            context: "传输队列".to_string(),
                            message: "未连接服务器，无法开始传输".to_string(),
                        });
                        continue;
                    }
                    self.pending.extend(tasks);
                    self.dispatch();
                }
                QueueCommand::Cancel(task_id) => self.cancel(task_id),
                QueueCommand::TaskDone(task_id) => {
                    self.running.remove(&task_id);
                    self.dispatch();
                }
            }
        }
    }

    fn cancel(&mut self, task_id: u64) {
        if let Some(handle) = self.running.get(&task_id) {
            handle.cancel();
            return;
        }
        if let Some(idx) = self.pending.iter().position(|t| t.task_id == task_id) {
            if let Some(req) = self.pending.remove(idx) {
                (self.emit)(FtpEvent::TaskFinished {
                    task_id: req.task_id,
                    outcome: Outcome::Canceled,
                });
            }
        }
    }

    fn dispatch(&mut self) {
        while self.running.len() < self.max_concurrent {
            let Some(req) = self.pending.pop_front() else {
                break;
            };
            let Some(cfg) = self.cfg.clone() else { break };
            let handle = CancelHandle::new();
            self.running.insert(req.task_id, handle.clone());
            spawn_transfer(self.emit.clone(), self.tx.clone(), cfg, req, handle);
        }
    }
}

fn spawn_transfer(
    emit: Emitter,
    done_tx: mpsc::Sender<QueueCommand>,
    cfg: ConnectConfig,
    req: TransferRequest,
    cancel: CancelHandle,
) {
    let _ = thread::Builder::new()
        .name(format!("transfer-{}", req.task_id))
        .spawn(move || {
            emit(FtpEvent::TaskProgress {
                task_id: req.task_id,
                done: 0,
                total: req.size,
                speed: 0,
            });
            let outcome = match req.direction {
                Direction::Download => run_download(&req, &cfg, &cancel, &emit),
                Direction::Upload => run_upload(&req, &cfg, &cancel, &emit),
            };
            emit(FtpEvent::TaskFinished {
                task_id: req.task_id,
                outcome: outcome.clone(),
            });
            let _ = done_tx.send(QueueCommand::TaskDone(req.task_id));
        });
}

pub fn run_download(
    req: &TransferRequest,
    cfg: &ConnectConfig,
    cancel: &CancelHandle,
    emit: &Emitter,
) -> Outcome {
    if cancel.is_cancelled() {
        return Outcome::Canceled;
    }
    let mut session = match FtpSession::connect(cfg) {
        Ok(session) => session,
        Err(err) => return Outcome::Failed(err.to_string()),
    };
    let result = download_via(&mut session, req, cancel, emit);
    if result.is_ok() {
        refresh_local_after_download(emit, req);
    }
    drop(session);
    match result {
        Ok(()) => Outcome::Completed,
        Err(_err) if cancel.is_cancelled() => {
            remove_partial(&req.local);
            Outcome::Canceled
        }
        Err(err) => {
            remove_partial(&req.local);
            Outcome::Failed(err.to_string())
        }
    }
}

fn download_via(
    session: &mut FtpSession,
    req: &TransferRequest,
    cancel: &CancelHandle,
    emit: &Emitter,
) -> Result<(), SessionError> {
    let (dir, name) = split_remote(&req.remote);
    if !dir.is_empty() {
        session.cwd(&dir)?;
    }
    let total = if req.size > 0 {
        req.size
    } else {
        session.size(&name).unwrap_or(0)
    };
    let mut file = File::create(&req.local)?;
    let mut buf = vec![0u8; READ_BUFFER_SIZE];
    let mut done: u64 = 0;
    let mut last_done: u64 = 0;
    let mut last_emit = Instant::now();
    session.download_file(&name, &mut |stream: &mut dyn Read| {
        loop {
            if cancel.is_cancelled() {
                return Err(SessionError::Other("已取消".to_string()));
            }
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            done += u64::try_from(n).unwrap_or(u64::MAX);
            let now = Instant::now();
            let elapsed = now.duration_since(last_emit);
            if elapsed >= PROGRESS_INTERVAL {
                let speed = speed_of(done - last_done, elapsed);
                emit(FtpEvent::TaskProgress {
                    task_id: req.task_id,
                    done,
                    total,
                    speed,
                });
                last_emit = now;
                last_done = done;
            }
        }
        Ok(())
    })?;
    emit(FtpEvent::TaskProgress {
        task_id: req.task_id,
        done: if total > 0 { total } else { done },
        total,
        speed: 0,
    });
    Ok(())
}

fn speed_of(bytes: u64, elapsed: Duration) -> u64 {
    let ms = u64::try_from(elapsed.as_millis()).unwrap_or(1).max(1);
    bytes * 1000 / ms
}

pub fn run_upload(
    req: &TransferRequest,
    cfg: &ConnectConfig,
    cancel: &CancelHandle,
    emit: &Emitter,
) -> Outcome {
    if cancel.is_cancelled() {
        return Outcome::Canceled;
    }
    let mut session = match FtpSession::connect(cfg) {
        Ok(session) => session,
        Err(err) => return Outcome::Failed(err.to_string()),
    };
    let result = upload_via(&mut session, req, cancel, emit);
    if result.is_ok() {
        refresh_remote_after_upload(&mut session, emit, req);
    }
    match result {
        Ok(()) => Outcome::Completed,
        Err(_err) if cancel.is_cancelled() => {
            remove_remote_partial(&mut session, req);
            Outcome::Canceled
        }
        Err(err) => {
            remove_remote_partial(&mut session, req);
            Outcome::Failed(err.to_string())
        }
    }
}

fn upload_via(
    session: &mut FtpSession,
    req: &TransferRequest,
    cancel: &CancelHandle,
    emit: &Emitter,
) -> Result<(), SessionError> {
    let (dir, name) = split_remote(&req.remote);
    if !dir.is_empty() {
        session.cwd(&dir)?;
    }
    let file = File::open(&req.local)?;
    let total = if req.size > 0 {
        req.size
    } else {
        file.metadata().map_or(0, |m| m.len())
    };
    let mut reader = ProgressReader {
        inner: file,
        cancel: cancel.clone(),
        done: 0,
        last_done: 0,
        total,
        task_id: req.task_id,
        emit: emit.clone(),
        last_emit: Instant::now(),
    };
    session.put_file(&name, &mut reader)?;
    emit(FtpEvent::TaskProgress {
        task_id: req.task_id,
        done: total,
        total,
        speed: 0,
    });
    Ok(())
}

struct ProgressReader<R: Read> {
    inner: R,
    cancel: CancelHandle,
    done: u64,
    last_done: u64,
    total: u64,
    task_id: u64,
    emit: Emitter,
    last_emit: Instant,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "已取消",
            ));
        }
        let n = self.inner.read(buf)?;
        self.done += u64::try_from(n).unwrap_or(u64::MAX);
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_emit);
        if elapsed >= PROGRESS_INTERVAL {
            let speed = speed_of(self.done - self.last_done, elapsed);
            (self.emit)(FtpEvent::TaskProgress {
                task_id: self.task_id,
                done: self.done,
                total: self.total,
                speed,
            });
            self.last_emit = now;
            self.last_done = self.done;
        }
        Ok(n)
    }
}

fn refresh_remote_after_upload(session: &mut FtpSession, emit: &Emitter, req: &TransferRequest) {
    let (dir, _name) = split_remote(&req.remote);
    let path = if dir.is_empty() {
        session.pwd().unwrap_or_default()
    } else {
        dir
    };
    match session.list_dir(Some(&path)) {
        Ok(entries) => emit(FtpEvent::Listing { path, entries }),
        Err(err) => emit(FtpEvent::LogLine {
            dir: LogDir::Error,
            text: format!("刷新远程目录失败：{err}"),
        }),
    }
}

fn remove_remote_partial(session: &mut FtpSession, req: &TransferRequest) {
    let (_dir, name) = split_remote(&req.remote);
    let _ = session.remove(&name);
}

fn remove_partial(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

fn refresh_local_after_download(emit: &Emitter, req: &TransferRequest) {
    let Some(parent) = req.local.parent() else {
        return;
    };
    match local_fs::list_dir(parent) {
        Ok(entries) => emit(FtpEvent::LocalListing {
            path: parent.to_path_buf(),
            entries,
        }),
        Err(err) => emit(FtpEvent::LogLine {
            dir: LogDir::Error,
            text: format!("刷新本地目录失败：{err}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{CancelHandle, Direction, TaskState, TransferRequest, can_transition};
    use crate::ftp::types::Outcome;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn cancel_handle_flags_once() {
        let h = CancelHandle::new();
        assert!(!h.is_cancelled());
        h.cancel();
        assert!(h.is_cancelled());
        let h2 = h.clone();
        assert!(h2.is_cancelled());
    }

    #[test]
    fn state_machine_happy_path() {
        assert!(can_transition(TaskState::Pending, TaskState::Running));
        assert!(can_transition(TaskState::Running, TaskState::Paused));
        assert!(can_transition(TaskState::Paused, TaskState::Running));
        assert!(can_transition(TaskState::Running, TaskState::Done));
    }

    #[test]
    fn state_machine_retry_and_cancel() {
        assert!(can_transition(TaskState::Running, TaskState::Failed));
        assert!(can_transition(TaskState::Failed, TaskState::Pending));
        assert!(can_transition(TaskState::Running, TaskState::Canceled));
        assert!(can_transition(TaskState::Paused, TaskState::Canceled));
    }

    #[test]
    fn state_machine_rejects_illegal() {
        assert!(!can_transition(TaskState::Pending, TaskState::Done));
        assert!(!can_transition(TaskState::Done, TaskState::Running));
        assert!(!can_transition(TaskState::Canceled, TaskState::Pending));
        assert!(!can_transition(TaskState::Failed, TaskState::Done));
        assert!(!can_transition(TaskState::Paused, TaskState::Done));
    }

    #[test]
    fn transfer_request_shape() {
        let req = TransferRequest {
            task_id: 7,
            direction: Direction::Upload,
            remote: "/a/b.txt".to_string(),
            local: PathBuf::from("C:/tmp/b.txt"),
            size: 123,
        };
        assert_eq!(req.task_id, 7);
        assert_eq!(req.direction, Direction::Upload);
    }

    #[test]
    fn speed_of_scales_to_seconds() {
        assert_eq!(
            super::speed_of(1500, Duration::from_millis(500)),
            3000,
            "500ms 传 1500B 应折算 3000B/s"
        );
        assert_eq!(super::speed_of(0, Duration::from_millis(0)), 0);
    }

    #[test]
    fn download_canceled_before_start_keeps_no_file() {
        use crate::ftp::types::{ConnectConfig, FtpEvent};
        use std::sync::Arc;
        use std::sync::mpsc;

        let cfg = ConnectConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            user: "anonymous".to_string(),
            password: "x".to_string(),
            ..ConnectConfig::default()
        };
        let (tx, _rx) = mpsc::channel::<FtpEvent>();
        let emit: super::Emitter = Arc::new(move |ev| {
            let _ = tx.send(ev);
        });
        let local =
            std::env::temp_dir().join(format!("ftp-slint-cancel-{}.tmp", std::process::id()));
        let req = TransferRequest {
            task_id: 1,
            direction: Direction::Download,
            remote: "/x.bin".to_string(),
            local: local.clone(),
            size: 0,
        };
        let handle = CancelHandle::new();
        handle.cancel();
        let outcome = super::run_download(&req, &cfg, &handle, &emit);
        assert_eq!(outcome, Outcome::Canceled);
        assert!(!local.exists(), "预取消不应产生文件");
    }

    #[test]
    fn upload_canceled_before_start_reports_canceled() {
        use crate::ftp::types::{ConnectConfig, FtpEvent};
        use std::sync::Arc;
        use std::sync::mpsc;

        let cfg = ConnectConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            user: "anonymous".to_string(),
            password: "x".to_string(),
            ..ConnectConfig::default()
        };
        let (tx, _rx) = mpsc::channel::<FtpEvent>();
        let emit: super::Emitter = Arc::new(move |ev| {
            let _ = tx.send(ev);
        });
        let local =
            std::env::temp_dir().join(format!("ftp-slint-cancel-up-{}.tmp", std::process::id()));
        std::fs::write(&local, b"payload").expect("写测试文件");
        let req = TransferRequest {
            task_id: 2,
            direction: Direction::Upload,
            remote: "/x-up.bin".to_string(),
            local: local.clone(),
            size: 0,
        };
        let handle = CancelHandle::new();
        handle.cancel();
        let outcome = super::run_upload(&req, &cfg, &handle, &emit);
        assert_eq!(outcome, Outcome::Canceled);
        std::fs::remove_file(&local).ok();
    }

    #[test]
    fn upload_missing_local_file_fails_fast() {
        use crate::ftp::types::{ConnectConfig, FtpEvent};
        use std::sync::Arc;
        use std::sync::mpsc;

        let cfg = ConnectConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            user: "anonymous".to_string(),
            password: "x".to_string(),
            ..ConnectConfig::default()
        };
        let (tx, _rx) = mpsc::channel::<FtpEvent>();
        let emit: super::Emitter = Arc::new(move |ev| {
            let _ = tx.send(ev);
        });
        let req = TransferRequest {
            task_id: 3,
            direction: Direction::Upload,
            remote: "/ghost.bin".to_string(),
            local: std::env::temp_dir()
                .join(format!("ftp-slint-missing-{}.bin", std::process::id())),
            size: 0,
        };
        let outcome = super::run_upload(&req, &cfg, &CancelHandle::new(), &emit);
        assert!(
            matches!(outcome, Outcome::Failed(_)),
            "本地文件不存在应快速失败：{outcome:?}"
        );
    }
}
