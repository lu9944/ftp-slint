//! 沙盒轨集成测试：检测到 `.env.local`（自部署沙盒服务器）时才运行，否则自动跳过
//! （DEVELOPMENT.md §8）。日常开发调试默认使用该轨，不污染真实服务器。

use std::sync::Arc;

use ftp_slint::core::queue::{
    CancelHandle, Direction, Emitter, TransferRequest, run_download, run_upload,
};
use ftp_slint::ftp::client::FtpSession;
use ftp_slint::ftp::types::{ConnectConfig, Outcome, PassiveMode};
use ftp_slint::store::env;

fn sandbox_cfg() -> Option<ConnectConfig> {
    if !env::sandbox_present() {
        eprintln!("跳过：未检测到 .env.local（沙盒服务器不可用）");
        return None;
    }
    env::load();
    let Some(ftp_env) = env::ftp_env() else {
        eprintln!("跳过：环境变量不完整（需要 FTP_SERVER_URL/FTP_USER/FTP_PWD）");
        return None;
    };
    Some(ConnectConfig {
        host: ftp_env.host.clone(),
        port: ftp_env.port,
        user: ftp_env.user.clone(),
        password: ftp_env.password,
        mode: PassiveMode::Auto,
        ..ConnectConfig::default()
    })
}

#[test]
fn sandbox_connect_and_browse() {
    let Some(cfg) = sandbox_cfg() else { return };
    let mut session = FtpSession::connect(&cfg).expect("沙盒服务器连接失败");

    let pwd = session.pwd().expect("PWD 失败");
    let entries = session.list_dir(None).expect("LIST 失败");
    eprintln!("沙盒服务器 {pwd}：{} 个条目", entries.len());
    for entry in entries.iter().take(10) {
        let kind = if entry.is_dir {
            "<DIR>".to_string()
        } else {
            format!("{}", entry.size)
        };
        eprintln!(
            "  {kind:<10} {} {}",
            entry.name,
            entry.perms.clone().unwrap_or_default()
        );
    }

    let subdir = format!("{pwd}/ftp-slint-test-{}", std::process::id());
    session.mkdir(&subdir).expect("沙盒 MKD 失败");
    let names: Vec<String> = session
        .list_dir(None)
        .expect("LIST 失败")
        .into_iter()
        .map(|e| e.name)
        .collect();
    let created = subdir.rsplit('/').next().unwrap_or_default().to_string();
    assert!(
        names.contains(&created),
        "沙盒新建目录后应能在列表中看到 {created}：{names:?}"
    );
    session.remove_dir(&subdir).expect("沙盒 RMD 失败");

    session.quit().expect("QUIT 失败");
}

#[test]
fn sandbox_upload_download_roundtrip() {
    let Some(cfg) = sandbox_cfg() else { return };

    let payload: Vec<u8> = (0..=u8::MAX).cycle().take(256 * 1024 + 13).collect();
    let local_src =
        std::env::temp_dir().join(format!("ftp-slint-sb-src-{}.bin", std::process::id()));
    let local_dst =
        std::env::temp_dir().join(format!("ftp-slint-sb-dst-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&local_dst);
    std::fs::write(&local_src, &payload).expect("写本地源文件");

    let (event_tx, _event_rx) = std::sync::mpsc::channel();
    let emit: Emitter = Arc::new(move |ev| {
        let _ = event_tx.send(ev);
    });

    let up = TransferRequest {
        task_id: 98,
        direction: Direction::Upload,
        remote: "/ftp-slint-sb-roundtrip.bin".to_string(),
        local: local_src.clone(),
        size: 0,
    };
    assert_eq!(
        run_upload(&up, &cfg, &CancelHandle::new(), &emit),
        Outcome::Completed,
        "沙盒端到端上传应完成"
    );

    let down = TransferRequest {
        task_id: 99,
        direction: Direction::Download,
        remote: "/ftp-slint-sb-roundtrip.bin".to_string(),
        local: local_dst.clone(),
        size: 0,
    };
    assert_eq!(
        run_download(&down, &cfg, &CancelHandle::new(), &emit),
        Outcome::Completed,
        "沙盒端到端下载应完成"
    );
    assert_eq!(
        std::fs::read(&local_dst).expect("下载文件应存在"),
        payload,
        "沙盒上传→下载往返应逐字节一致"
    );

    std::fs::remove_file(&local_src).ok();
    std::fs::remove_file(&local_dst).ok();

    let mut session = FtpSession::connect(&cfg).expect("沙盒连接失败（清理阶段）");
    let _ = session.remove("/ftp-slint-sb-roundtrip.bin");
    session.quit().ok();
}
