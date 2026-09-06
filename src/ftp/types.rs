use std::path::PathBuf;

use crate::core::queue::TransferRequest;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<i64>,
    pub perms: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PassiveMode {
    #[default]
    Auto,
    Passive,
    ExtendedPassive,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TlsMode {
    #[default]
    Plain,
    ExplicitFtps,
    ImplicitFtps,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Utf8,
    Gbk,
}

#[derive(Debug, Clone)]
pub struct ConnectConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub mode: PassiveMode,
    pub tls: TlsMode,
    pub encoding: Encoding,
}

impl Default for ConnectConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 21,
            user: String::new(),
            password: String::new(),
            mode: PassiveMode::default(),
            tls: TlsMode::default(),
            encoding: Encoding::default(),
        }
    }
}

#[derive(Debug)]
pub enum FtpCommand {
    Connect(ConnectConfig),
    List { path: String },
    Navigate { path: String },
    Mkdir { path: String },
    Delete { paths: Vec<String> },
    Rename { from: String, to: String },
    Chmod { path: String, mode: String },
    EnqueueTransfers { tasks: Vec<TransferRequest> },
    CancelTask { task_id: u64 },
    Ping,
    Quit,
}

#[derive(Debug)]
pub enum LocalFsCommand {
    List { path: PathBuf },
    Mkdir { path: PathBuf },
    Delete { paths: Vec<PathBuf> },
    Rename { from: PathBuf, to: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogDir {
    Send,
    Recv,
    Info,
    Error,
}

impl LogDir {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Send => "命令",
            Self::Recv => "响应",
            Self::Info => "信息",
            Self::Error => "错误",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed(String),
    Canceled,
}

#[derive(Debug)]
pub enum FtpEvent {
    Connected,
    Listing {
        path: String,
        entries: Vec<FileEntry>,
    },
    LocalListing {
        path: PathBuf,
        entries: Vec<FileEntry>,
    },
    LogLine {
        dir: LogDir,
        text: String,
    },
    TaskProgress {
        task_id: u64,
        done: u64,
        total: u64,
        speed: u64,
    },
    TaskFinished {
        task_id: u64,
        outcome: Outcome,
    },
    Disconnected {
        reason: String,
    },
    Error {
        context: String,
        message: String,
    },
}

pub fn join_remote(base: &str, name: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        format!("/{name}")
    } else {
        format!("{base}/{name}")
    }
}

pub fn parent_remote(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    let idx = trimmed.rfind('/')?;
    Some(if idx == 0 {
        "/".to_string()
    } else {
        trimmed[..idx].to_string()
    })
}

pub fn split_remote(path: &str) -> (String, String) {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => ("/".to_string(), trimmed[1..].to_string()),
        Some(i) => (trimmed[..i].to_string(), trimmed[i + 1..].to_string()),
        None => (String::new(), trimmed.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_remote_basics() {
        assert_eq!(join_remote("/", "pub"), "/pub");
        assert_eq!(join_remote("/a/b", "c"), "/a/b/c");
        assert_eq!(join_remote("/a/b/", "c"), "/a/b/c");
        assert_eq!(join_remote("/", "a b"), "/a b");
        assert_eq!(join_remote("", "x"), "/x");
    }

    #[test]
    fn parent_remote_basics() {
        assert_eq!(parent_remote("/a/b"), Some("/a".to_string()));
        assert_eq!(parent_remote("/a"), Some("/".to_string()));
        assert_eq!(parent_remote("/"), None);
        assert_eq!(parent_remote("/a/b/"), Some("/a".to_string()));
        assert_eq!(parent_remote(""), None);
    }

    #[test]
    fn split_remote_basics() {
        assert_eq!(
            split_remote("/a/b/c.txt"),
            ("/a/b".to_string(), "c.txt".to_string())
        );
        assert_eq!(
            split_remote("/c.txt"),
            ("/".to_string(), "c.txt".to_string())
        );
        assert_eq!(split_remote("c.txt"), (String::new(), "c.txt".to_string()));
        assert_eq!(split_remote("/a/b/"), ("/a".to_string(), "b".to_string()));
        assert_eq!(
            split_remote("/dir/我 的文件.txt"),
            ("/dir".to_string(), "我 的文件.txt".to_string())
        );
    }
}
