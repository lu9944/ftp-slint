use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use suppaftp::FtpStream;
use suppaftp::types::Mode;
use thiserror::Error;

use crate::ftp::parser;
use crate::ftp::types::{ConnectConfig, FileEntry, PassiveMode, TlsMode};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("FTP 错误：{0}")]
    Ftp(#[from] suppaftp::FtpError),
    #[error("网络错误：{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Other(String),
}

pub struct FtpSession {
    stream: FtpStream,
    mode: Mode,
}

impl FtpSession {
    pub fn connect(cfg: &ConnectConfig) -> Result<Self, SessionError> {
        if cfg.tls != TlsMode::Plain {
            return Err(SessionError::Other("FTPS 将在 M6 里程碑提供".to_string()));
        }
        let tcp = dial(&cfg.host, cfg.port)?;
        tcp.set_read_timeout(Some(IO_TIMEOUT))?;
        tcp.set_write_timeout(Some(IO_TIMEOUT))?;
        tcp.set_nodelay(true)?;
        let mut stream = FtpStream::connect_with_stream(tcp)?;
        stream.login(&cfg.user, &cfg.password)?;
        let mut mode = match cfg.mode {
            PassiveMode::Passive => Mode::Passive,
            PassiveMode::ExtendedPassive | PassiveMode::Auto => Mode::ExtendedPassive,
        };
        stream.set_mode(mode);
        if cfg.mode == PassiveMode::Auto && stream.list(None).is_err() {
            mode = Mode::Passive;
            stream.set_mode(mode);
        }
        Ok(Self { stream, mode })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn size(&mut self, path: &str) -> Result<u64, SessionError> {
        let n = self.stream.size(path)?;
        Ok(u64::try_from(n).unwrap_or(u64::MAX))
    }

    pub fn download_file(
        &mut self,
        name: &str,
        consumer: &mut dyn FnMut(&mut dyn io::Read) -> Result<(), SessionError>,
    ) -> Result<(), SessionError> {
        self.stream
            .transfer_type(suppaftp::types::FileType::Binary)
            .map_err(SessionError::from)?;
        self.stream
            .retr(name, |stream: &mut dyn io::Read| {
                consumer(stream).map_err(|err| {
                    suppaftp::FtpError::ConnectionError(io::Error::other(err.to_string()))
                })
            })
            .map_err(SessionError::from)
    }

    pub fn put_file(
        &mut self,
        name: &str,
        reader: &mut impl io::Read,
    ) -> Result<u64, SessionError> {
        self.stream
            .transfer_type(suppaftp::types::FileType::Binary)
            .map_err(SessionError::from)?;
        Ok(self.stream.put_file(name, reader)?)
    }

    pub fn pwd(&mut self) -> Result<String, SessionError> {
        Ok(self.stream.pwd()?)
    }

    pub fn cwd(&mut self, path: &str) -> Result<(), SessionError> {
        Ok(self.stream.cwd(path)?)
    }

    pub fn cdup(&mut self) -> Result<(), SessionError> {
        Ok(self.stream.cdup()?)
    }

    pub fn mkdir(&mut self, path: &str) -> Result<(), SessionError> {
        Ok(self.stream.mkdir(path)?)
    }

    pub fn remove(&mut self, path: &str) -> Result<(), SessionError> {
        Ok(self.stream.rm(path)?)
    }

    pub fn remove_dir(&mut self, path: &str) -> Result<(), SessionError> {
        Ok(self.stream.rmdir(path)?)
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), SessionError> {
        Ok(self.stream.rename(from, to)?)
    }

    pub fn retr_as_buffer(&mut self, name: &str) -> Result<std::io::Cursor<Vec<u8>>, SessionError> {
        self.stream
            .transfer_type(suppaftp::types::FileType::Binary)
            .map_err(SessionError::from)?;
        Ok(self.stream.retr_as_buffer(name)?)
    }

    pub fn quit(&mut self) -> Result<(), SessionError> {
        Ok(self.stream.quit()?)
    }

    pub fn list_dir(&mut self, path: Option<&str>) -> Result<Vec<FileEntry>, SessionError> {
        if let Ok(lines) = self.stream.mlsd(path) {
            let mut entries = parse_lines(&lines);
            parser::sort_entries(&mut entries);
            return Ok(entries);
        }
        let mut entries = self.list_with_mode_fallback(path)?;
        parser::sort_entries(&mut entries);
        Ok(entries)
    }

    fn list_with_mode_fallback(
        &mut self,
        path: Option<&str>,
    ) -> Result<Vec<FileEntry>, SessionError> {
        match self.stream.list(path) {
            Ok(lines) => Ok(parse_lines(&lines)),
            Err(first_err) => {
                if self.mode == Mode::ExtendedPassive {
                    self.mode = Mode::Passive;
                    self.stream.set_mode(Mode::Passive);
                    match self.stream.list(path) {
                        Ok(lines) => Ok(parse_lines(&lines)),
                        Err(_second_err) => Err(first_err.into()),
                    }
                } else {
                    Err(first_err.into())
                }
            }
        }
    }
}

fn dial(host: &str, port: u16) -> Result<TcpStream, SessionError> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    if addrs.is_empty() {
        return Err(SessionError::Other(format!("无法解析地址 {host}:{port}")));
    }
    let mut last_err: Option<io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(err) => last_err = Some(err),
        }
    }
    Err(match last_err {
        Some(err) => SessionError::Io(err),
        None => SessionError::Other(format!("无法连接 {host}:{port}")),
    })
}

fn parse_lines(lines: &[String]) -> Vec<FileEntry> {
    lines
        .iter()
        .filter_map(|line| parser::parse_listing_line(line))
        .filter(|e| e.name != "." && e.name != "..")
        .collect()
}
