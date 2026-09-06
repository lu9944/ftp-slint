use std::path::Path;

pub struct FtpEnv {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

pub const ENV_FILE: &str = ".env";
pub const ENV_LOCAL_FILE: &str = ".env.local";

pub fn sandbox_present() -> bool {
    Path::new(ENV_LOCAL_FILE).exists()
}

pub fn load() {
    let _ = dotenvy::dotenv();
    if sandbox_present() {
        let _ = dotenvy::from_path_override(ENV_LOCAL_FILE);
    }
}

pub fn ftp_env() -> Option<FtpEnv> {
    ftp_env_from_lookup(|key| std::env::var(key).ok())
}

pub fn ftp_env_from_file(path: &str) -> Option<FtpEnv> {
    if dotenvy::from_path_override(path).is_err() {
        return None;
    }
    ftp_env()
}

fn ftp_env_from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Option<FtpEnv> {
    let host = lookup("FTP_SERVER_URL")?;
    let port = lookup("FTP_PORT")
        .and_then(|p| p.parse().ok())
        .unwrap_or(21);
    let user = lookup("FTP_USER")?;
    let password = lookup("FTP_PWD")?;
    Some(FtpEnv {
        host,
        port,
        user,
        password,
    })
}

#[cfg(test)]
mod tests {
    use super::ftp_env_from_lookup;
    use std::collections::HashMap;

    fn map_lookup<'a>(map: &'a [(&'a str, &'a str)]) -> impl FnMut(&str) -> Option<String> + 'a {
        let map: HashMap<String, String> = map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn parses_all_four_vars() {
        let lookup = map_lookup(&[
            ("FTP_SERVER_URL", "192.0.2.1"),
            ("FTP_PORT", "2121"),
            ("FTP_USER", "u"),
            ("FTP_PWD", "p"),
        ]);
        let env = ftp_env_from_lookup(lookup).expect("应有完整环境");
        assert_eq!(env.host, "192.0.2.1");
        assert_eq!(env.port, 2121);
        assert_eq!(env.user, "u");
        assert_eq!(env.password, "p");
    }

    #[test]
    fn port_defaults_to_21_when_missing_or_invalid() {
        let lookup = map_lookup(&[("FTP_SERVER_URL", "h"), ("FTP_USER", "u"), ("FTP_PWD", "p")]);
        assert_eq!(ftp_env_from_lookup(lookup).unwrap().port, 21);

        let lookup = map_lookup(&[
            ("FTP_SERVER_URL", "h"),
            ("FTP_PORT", "not-a-number"),
            ("FTP_USER", "u"),
            ("FTP_PWD", "p"),
        ]);
        assert_eq!(ftp_env_from_lookup(lookup).unwrap().port, 21);
    }

    #[test]
    fn missing_any_required_var_yields_none() {
        assert!(ftp_env_from_lookup(|_| None).is_none());

        let lookup = map_lookup(&[("FTP_SERVER_URL", "h")]);
        assert!(ftp_env_from_lookup(lookup).is_none());

        let lookup = map_lookup(&[("FTP_SERVER_URL", "h"), ("FTP_USER", "u")]);
        assert!(ftp_env_from_lookup(lookup).is_none());

        let lookup = map_lookup(&[("FTP_SERVER_URL", "h"), ("FTP_USER", "u"), ("FTP_PWD", "p")]);
        assert!(
            ftp_env_from_lookup(lookup).is_some(),
            "仅缺端口时使用默认值"
        );
    }
}
