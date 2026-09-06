//! 真实轨集成测试（`#[ignore]` 手动执行）：`.env` 中的真实服务器，NAT 后需 EPSV
//! （DEVELOPMENT.md §8）。运行：`cargo test --test ftp_check -- --ignored --nocapture`

use std::sync::Arc;

use ftp_slint::core::queue::{
    CancelHandle, Direction, Emitter, TransferRequest, run_download, run_upload,
};
use ftp_slint::ftp::client::FtpSession;
use ftp_slint::ftp::types::{ConnectConfig, FtpEvent, Outcome, PassiveMode};
use suppaftp::types::Mode;

fn real_cfg() -> ConnectConfig {
    let Some(ftp_env) = ftp_slint::store::env::ftp_env_from_file(".env") else {
        panic!("无法从 .env 读取完整配置（FTP_SERVER_URL/FTP_USER/FTP_PWD）");
    };
    ConnectConfig {
        host: ftp_env.host,
        port: ftp_env.port,
        user: ftp_env.user,
        password: ftp_env.password,
        mode: PassiveMode::Auto,
        ..ConnectConfig::default()
    }
}

#[test]
#[ignore = "真实服务器（NAT 后、需 EPSV），仅手动验证"]
fn real_server_connect_and_list() {
    let cfg = real_cfg();
    let host = cfg.host.clone();
    let user = cfg.user.clone();
    let port = cfg.port;

    let mut ftp = FtpSession::connect(&cfg).expect("connect failed");
    println!("connected to {host}:{port}");
    println!("login OK as {user}");

    let entries = ftp.list_dir(None).expect("LIST failed");
    println!("root directory has {} entries", entries.len());
    for line in entries.iter().take(10) {
        println!(
            "{} {} {}",
            if line.is_dir { "d" } else { "-" },
            line.name,
            line.size
        );
    }

    assert_eq!(
        ftp.mode(),
        Mode::ExtendedPassive,
        "真实服务器在 NAT 后，Auto 模式应协商为 EPSV"
    );

    let _ = ftp.quit();
}

#[test]
#[ignore = "真实服务器大文件传输（M2 验收：1KB~1GB、进度、独立连接、双向）"]
fn real_server_upload_download_with_progress() {
    let cfg = real_cfg();
    let size_mb: usize = std::env::var("FTP_TEST_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let remote_name = "ftp-slint-real-test.bin";
    let payload: Vec<u8> = (0..=u8::MAX).cycle().take(size_mb * 1024 * 1024).collect();

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let emit: Emitter = Arc::new(move |ev| {
        let _ = event_tx.send(ev);
    });

    let local_src = std::env::temp_dir().join("ftp-slint-real-src.bin");
    std::fs::write(&local_src, &payload).expect("写本地源文件");
    let up = TransferRequest {
        task_id: 1,
        direction: Direction::Upload,
        remote: format!("/{remote_name}"),
        local: local_src.clone(),
        size: 0,
    };
    let upload_started = std::time::Instant::now();
    assert_eq!(
        run_upload(&up, &cfg, &CancelHandle::new(), &emit),
        Outcome::Completed,
        "真实服务器上传应完成"
    );
    let upload_secs = upload_started.elapsed().as_secs();
    println!("uploaded {} bytes in {upload_secs}s", payload.len());

    let local = std::env::temp_dir().join("ftp-slint-real-dl.bin");
    let _ = std::fs::remove_file(&local);
    let down = TransferRequest {
        task_id: 2,
        direction: Direction::Download,
        remote: format!("/{remote_name}"),
        local: local.clone(),
        size: 0,
    };
    let download_started = std::time::Instant::now();
    assert_eq!(
        run_download(&down, &cfg, &CancelHandle::new(), &emit),
        Outcome::Completed,
        "真实服务器下载应完成"
    );
    let download_secs = download_started.elapsed().as_secs();
    println!("downloaded {} bytes in {download_secs}s", payload.len());
    assert_eq!(
        std::fs::read(&local).expect("下载文件应存在"),
        payload,
        "{size_mb}MB 级上传→下载往返应逐字节一致"
    );

    let mut upload_speed_events = 0;
    let mut download_speed_events = 0;
    while let Ok(ev) = event_rx.try_recv() {
        match ev {
            FtpEvent::TaskProgress {
                task_id: 1, speed, ..
            } if speed > 0 => {
                upload_speed_events += 1;
            }
            FtpEvent::TaskProgress {
                task_id: 2, speed, ..
            } if speed > 0 => {
                download_speed_events += 1;
            }
            _ => {}
        }
    }
    println!(
        "progress events with speed: upload={upload_speed_events} download={download_speed_events}"
    );
    assert!(upload_speed_events > 0, "上传应收到带速度的节流进度事件");
    assert!(download_speed_events > 0, "下载应收到带速度的节流进度事件");
    let _ = std::fs::remove_file(&local_src);
    let _ = std::fs::remove_file(&local);

    let mut session = FtpSession::connect(&cfg).expect("连接失败（清理阶段）");
    let _ = session.remove(&format!("/{remote_name}"));
    session.quit().ok();
    println!("OK: {size_mb}MB upload+download verified via independent connections");
}
