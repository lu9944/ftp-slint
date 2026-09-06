use std::collections::HashMap;
use std::fs;

fn load_env(path: &str) -> HashMap<String, String> {
    let content = fs::read_to_string(path).expect("cannot read .env");
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

#[test]
fn ftp_login_and_list() {
    let env = load_env(".env");
    let host = env.get("FTP_SERVER_URL").expect("FTP_SERVER_URL missing");
    let port: u16 = env
        .get("FTP_PORT")
        .map(|p| p.parse().expect("FTP_PORT invalid"))
        .unwrap_or(21);
    let user = env.get("FTP_USER").expect("FTP_USER missing");
    let pwd = env.get("FTP_PWD").expect("FTP_PWD missing");

    let mut ftp = suppaftp::FtpStream::connect((host.as_str(), port))
        .expect("connect failed");
    // server is behind NAT: PASV returns a private IP (10.x), EPSV works
    ftp.set_mode(suppaftp::types::Mode::ExtendedPassive);
    println!("connected to {host}:{port}");

    ftp.login(user, pwd).expect("login failed (check FTP_USER/FTP_PWD)");
    println!("login OK as {user}");

    let entries = ftp.list(None).expect("LIST failed");
    println!("root directory has {} entries", entries.len());
    for line in entries.iter().take(10) {
        println!("{line}");
    }

    let _ = ftp.quit();
}
