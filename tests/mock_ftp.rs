//! mock 轨集成测试：libunftp 本地 FTP 服务器，可自动化、CI 可跑（DEVELOPMENT.md §8）。
//!
//! libunftp 不支持 EPSV 与 MLSD，正好覆盖客户端的 Auto 降级路径（EPSV 失败退 PASV、
//! MLSD 失败退 LIST）。

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ftp_slint::core::queue::{
    CancelHandle, Direction, Emitter, QueueCommand, Scheduler, TransferRequest, run_download,
    run_upload,
};
use ftp_slint::ftp::client::FtpSession;
use ftp_slint::ftp::types::{ConnectConfig, FtpEvent, Outcome, PassiveMode};
use suppaftp::types::Mode;
use unftp_sbe_fs::ServerExt;

struct TestServer {
    port: u16,
    root: PathBuf,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("绑定随机端口失败")
        .local_addr()
        .expect("读取端口失败")
        .port()
}

fn start_server() -> TestServer {
    let root = std::env::temp_dir().join(format!(
        "ftp-slint-mock-{}-{}",
        std::process::id(),
        free_port()
    ));
    std::fs::create_dir_all(&root).expect("创建 mock 根目录失败");
    std::fs::write(root.join("hello.txt"), b"hello ftp").expect("写入种子文件失败");
    std::fs::create_dir_all(root.join("subdir")).expect("创建子目录失败");

    let port = free_port();
    let server_root = root.clone();
    let bind = format!("127.0.0.1:{port}");
    tokio::spawn(async move {
        let server = libunftp::Server::with_fs(server_root)
            .passive_ports(50000..50100)
            .build()
            .expect("构建 libunftp 失败");
        if let Err(err) = server.listen(bind).await {
            eprintln!("libunftp 退出：{err}");
        }
    });
    wait_for_port(port);

    TestServer { port, root }
}

fn wait_for_port(port: u16) {
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("mock 服务器在 5s 内未就绪（端口 {port}）");
}

fn connect(port: u16) -> FtpSession {
    let cfg = ConnectConfig {
        host: "127.0.0.1".to_string(),
        port,
        user: "anonymous".to_string(),
        password: "tester@example.com".to_string(),
        mode: PassiveMode::Auto,
        ..ConnectConfig::default()
    };
    FtpSession::connect(&cfg).expect("Auto 模式连接 mock 服务器失败")
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_list_and_mode_fallback() {
    let server = start_server();
    let mut session = connect(server.port);

    let entries = session.list_dir(None).expect("LIST 失败");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"hello.txt"), "应包含 hello.txt：{names:?}");
    assert!(names.contains(&"subdir"), "应包含 subdir：{names:?}");
    let subdir = entries.iter().find(|e| e.name == "subdir").unwrap();
    assert!(subdir.is_dir);

    let hello = entries.iter().find(|e| e.name == "hello.txt").unwrap();
    assert!(!hello.is_dir);
    assert_eq!(hello.size, 9);

    assert_eq!(
        session.mode(),
        Mode::Passive,
        "libunftp 无 EPSV，Auto 应已降级为 Passive"
    );

    let pwd = session.pwd().expect("PWD 失败");
    assert_eq!(pwd, "/");

    session.quit().expect("QUIT 失败");
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_download_roundtrip() {
    let server = start_server();
    let mut session = connect(server.port);

    let payload: Vec<u8> = (0..=u8::MAX).cycle().take(64 * 1024 + 7).collect();
    let mut reader = &payload[..];
    session
        .put_file("roundtrip.bin", &mut reader)
        .expect("上传失败");

    let mut downloaded = session.retr_as_buffer("roundtrip.bin").expect("下载失败");
    let got = downloaded.get_mut();
    assert_eq!(got.len(), payload.len(), "上传下载内容长度应一致");
    assert_eq!(&payload[..], &got[..], "上传下载内容应逐字节一致");

    let _ = session.remove("roundtrip.bin");
    session.quit().expect("QUIT 失败");
}

#[tokio::test(flavor = "multi_thread")]
async fn mkdir_rename_delete() {
    let server = start_server();
    let mut session = connect(server.port);

    session.mkdir("/mock-dir").expect("MKD 失败");
    let names: Vec<String> = session
        .list_dir(None)
        .expect("LIST 失败")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(
        names.contains(&"mock-dir".to_string()),
        "MKD 后应看到目录：{names:?}"
    );

    session
        .rename("/hello.txt", "/renamed.txt")
        .expect("RNFR/RNTO 失败");
    let names: Vec<String> = session
        .list_dir(None)
        .expect("LIST 失败")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(
        names.contains(&"renamed.txt".to_string()),
        "重命名后应看到新名：{names:?}"
    );
    assert!(!names.contains(&"hello.txt".to_string()));

    session.remove("/renamed.txt").expect("DELE 失败");
    session.remove_dir("/mock-dir").expect("RMD 失败");
    let names: Vec<String> = session
        .list_dir(None)
        .expect("LIST 失败")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(!names.contains(&"renamed.txt".to_string()));
    assert!(!names.contains(&"mock-dir".to_string()));

    session.quit().expect("QUIT 失败");
}

#[tokio::test(flavor = "multi_thread")]
async fn navigate_subdirectory() {
    let server = start_server();
    let mut session = connect(server.port);

    std::fs::write(server.root.join("subdir").join("inner.txt"), b"inner")
        .expect("准备子目录文件失败");

    session.cwd("/subdir").expect("CWD 失败");
    let entries = session.list_dir(None).expect("LIST 失败");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"inner.txt"),
        "子目录列表应包含 inner.txt：{names:?}"
    );

    session.cdup().expect("CDUP 失败");
    let pwd = session.pwd().expect("PWD 失败");
    assert_eq!(pwd, "/");

    session.quit().expect("QUIT 失败");
}

#[tokio::test(flavor = "multi_thread")]
async fn download_via_run_download() {
    let server = start_server();
    let cfg = ConnectConfig {
        host: "127.0.0.1".to_string(),
        port: server.port,
        user: "anonymous".to_string(),
        password: "tester@example.com".to_string(),
        mode: PassiveMode::Auto,
        ..ConnectConfig::default()
    };
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let emit: Emitter = Arc::new(move |ev| {
        let _ = event_tx.send(ev);
    });

    let local = std::env::temp_dir().join(format!("ftp-slint-dl-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&local);
    let req = TransferRequest {
        task_id: 11,
        direction: Direction::Download,
        remote: "/hello.txt".to_string(),
        local: local.clone(),
        size: 0,
    };
    let outcome = run_download(&req, &cfg, &CancelHandle::new(), &emit);
    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(
        std::fs::read(&local).expect("下载文件应存在"),
        b"hello ftp",
        "下载内容应与服务器一致"
    );

    let mut saw_progress = false;
    while let Ok(ev) = event_rx.try_recv() {
        if let FtpEvent::TaskProgress {
            done: 9, total: 9, ..
        } = ev
        {
            saw_progress = true;
        }
    }
    assert!(saw_progress, "应收到 done=total=9 的进度事件");
    let _ = std::fs::remove_file(&local);
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_enqueues_and_completes_download() {
    let server = start_server();
    let cfg = ConnectConfig {
        host: "127.0.0.1".to_string(),
        port: server.port,
        user: "anonymous".to_string(),
        password: "tester@example.com".to_string(),
        mode: PassiveMode::Auto,
        ..ConnectConfig::default()
    };
    let (queue_tx, queue_rx) = std::sync::mpsc::channel::<QueueCommand>();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let emit: Emitter = Arc::new(move |ev| {
        let _ = event_tx.send(ev);
    });
    Scheduler::spawn(emit, queue_tx.clone(), queue_rx);

    queue_tx
        .send(QueueCommand::SetConfig(cfg))
        .expect("发送 SetConfig");
    let local = std::env::temp_dir().join(format!("ftp-slint-sched-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&local);
    queue_tx
        .send(QueueCommand::Enqueue(vec![TransferRequest {
            task_id: 21,
            direction: Direction::Download,
            remote: "/hello.txt".to_string(),
            local: local.clone(),
            size: 0,
        }]))
        .expect("发送 Enqueue");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut finished = false;
    while std::time::Instant::now() < deadline {
        match event_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(FtpEvent::TaskFinished {
                task_id: 21,
                outcome,
            }) => {
                assert_eq!(outcome, Outcome::Completed, "调度器应完成任务 21");
                finished = true;
                break;
            }
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(finished, "10s 内未收到任务完成事件");
    assert_eq!(
        std::fs::read(&local).expect("调度器下载文件应存在"),
        b"hello ftp"
    );
    let _ = std::fs::remove_file(&local);
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_via_run_upload() {
    let server = start_server();
    let cfg = ConnectConfig {
        host: "127.0.0.1".to_string(),
        port: server.port,
        user: "anonymous".to_string(),
        password: "tester@example.com".to_string(),
        mode: PassiveMode::Auto,
        ..ConnectConfig::default()
    };
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let emit: Emitter = Arc::new(move |ev| {
        let _ = event_tx.send(ev);
    });

    let payload_len: u64 = 128 * 1024 + 5;
    let payload: Vec<u8> = (0..=u8::MAX)
        .cycle()
        .take(usize::try_from(payload_len).unwrap_or(0))
        .collect();
    let local = std::env::temp_dir().join(format!("ftp-slint-up-{}.bin", std::process::id()));
    std::fs::write(&local, &payload).expect("写本地测试文件");

    let req = TransferRequest {
        task_id: 31,
        direction: Direction::Upload,
        remote: "/uploaded.bin".to_string(),
        local: local.clone(),
        size: 0,
    };
    let outcome = run_upload(&req, &cfg, &CancelHandle::new(), &emit);
    assert_eq!(outcome, Outcome::Completed, "上传应完成");

    let mut saw_progress = false;
    while let Ok(ev) = event_rx.try_recv() {
        if let FtpEvent::TaskProgress { done, total, .. } = ev
            && done == payload_len
            && total == payload_len
        {
            saw_progress = true;
        }
    }
    assert!(saw_progress, "应收到 done=total 的进度事件");

    let mut session = connect(server.port);
    let got = session
        .retr_as_buffer("uploaded.bin")
        .expect("回读上传文件失败");
    assert_eq!(got.get_ref().len(), payload.len(), "上传内容长度应一致");
    assert_eq!(&payload[..], &got.get_ref()[..], "上传内容应逐字节一致");
    let _ = session.remove("uploaded.bin");
    session.quit().ok();
    std::fs::remove_file(&local).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_then_download_roundtrip() {
    let server = start_server();
    let cfg = ConnectConfig {
        host: "127.0.0.1".to_string(),
        port: server.port,
        user: "anonymous".to_string(),
        password: "tester@example.com".to_string(),
        mode: PassiveMode::Auto,
        ..ConnectConfig::default()
    };
    let (event_tx, _event_rx) = std::sync::mpsc::channel();
    let emit: Emitter = Arc::new(move |ev| {
        let _ = event_tx.send(ev);
    });

    let payload: Vec<u8> = (0..600_000)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    let local_src =
        std::env::temp_dir().join(format!("ftp-slint-rt-src-{}.bin", std::process::id()));
    let local_dst =
        std::env::temp_dir().join(format!("ftp-slint-rt-dst-{}.bin", std::process::id()));
    std::fs::write(&local_src, &payload).expect("写源文件");

    let up = TransferRequest {
        task_id: 41,
        direction: Direction::Upload,
        remote: "/roundtrip.bin".to_string(),
        local: local_src.clone(),
        size: 0,
    };
    assert_eq!(
        run_upload(&up, &cfg, &CancelHandle::new(), &emit),
        Outcome::Completed
    );
    let down = TransferRequest {
        task_id: 42,
        direction: Direction::Download,
        remote: "/roundtrip.bin".to_string(),
        local: local_dst.clone(),
        size: 0,
    };
    assert_eq!(
        run_download(&down, &cfg, &CancelHandle::new(), &emit),
        Outcome::Completed
    );
    assert_eq!(
        std::fs::read(&local_dst).expect("下载文件应存在"),
        payload,
        "上传→下载往返应逐字节一致"
    );

    let mut session = connect(server.port);
    let _ = session.remove("roundtrip.bin");
    session.quit().ok();
    std::fs::remove_file(&local_src).ok();
    std::fs::remove_file(&local_dst).ok();
}
