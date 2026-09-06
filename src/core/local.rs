use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::ftp::types::FileEntry;

pub fn list_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for b in b'A'..=b'Z' {
            let c = b as char;
            let p = PathBuf::from(format!("{c}:\\"));
            if p.exists() {
                roots.push(p);
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        roots.push(PathBuf::from("/"));
    }
    roots
}

pub fn list_dir(path: &Path) -> std::io::Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(path)? {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        let is_dir = meta.is_dir();
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        out.push(FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir,
            size: if is_dir { 0 } else { meta.len() },
            modified,
            perms: perm_string(&meta),
        });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

fn perm_string(meta: &fs::Metadata) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        Some(mode_string(u32::from(mode)))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

#[cfg(unix)]
fn mode_string(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6u32, 3, 0] {
        let tri = (mode >> shift) & 0o7;
        s.push(if tri & 0o4 != 0 { 'r' } else { '-' });
        s.push(if tri & 0o2 != 0 { 'w' } else { '-' });
        s.push(if tri & 0o1 != 0 { 'x' } else { '-' });
    }
    s
}

pub fn parent(path: &Path) -> Option<PathBuf> {
    let p = path.parent()?;
    if p.as_os_str().is_empty() {
        None
    } else {
        Some(p.to_path_buf())
    }
}

pub fn join(path: &Path, name: &str) -> PathBuf {
    path.join(name)
}

pub fn mkdir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)
}

pub fn delete(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

pub fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::{delete, join, list_dir, list_roots, mkdir, parent, rename};
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn roots_non_empty() {
        assert!(!list_roots().is_empty());
    }

    #[test]
    fn list_and_parent_tempdir() {
        let base = std::env::temp_dir().join(format!("ftp-slint-local-{}", std::process::id()));
        let dir = join(&base, "sub");
        fs::create_dir_all(&dir).unwrap();
        fs::write(join(&dir, "a.txt"), b"hello").unwrap();
        fs::write(join(&dir, "BB.txt"), b"x").unwrap();
        fs::create_dir_all(join(&dir, "zdir")).unwrap();

        let entries = list_dir(&dir).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_dir, "目录应排在最前");
        assert_eq!(entries[0].name, "zdir");
        assert_eq!(entries[1].name.to_lowercase(), "a.txt");
        assert_eq!(entries[2].size, 1);

        let p = parent(&dir).unwrap();
        assert_eq!(p, base);
        let root = &list_roots()[0];
        assert_eq!(parent(root), None, "根设备的父目录应为 None");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn join_handles_relative() {
        assert_eq!(
            join(&PathBuf::from("C:\\a"), "b"),
            PathBuf::from("C:\\a\\b")
        );
    }

    #[test]
    fn mkdir_rename_delete_roundtrip() {
        let base = std::env::temp_dir().join(format!("ftp-slint-ops-{}.tmp", std::process::id()));
        let dir = join(&base, "ops");
        mkdir(&dir).unwrap();
        assert!(dir.is_dir());

        let file = join(&dir, "a.txt");
        fs::write(&file, b"1").unwrap();
        let renamed = join(&dir, "b.txt");
        rename(&file, &renamed).unwrap();
        assert!(!file.exists());
        assert!(renamed.exists());

        delete(&renamed).unwrap();
        assert!(!renamed.exists());

        let sub = join(&dir, "sub");
        mkdir(&sub).unwrap();
        fs::write(join(&sub, "c.txt"), b"2").unwrap();
        delete(&dir).unwrap();
        assert!(!dir.exists(), "目录删除应递归");

        let _ = fs::remove_dir_all(&base);
    }
}
