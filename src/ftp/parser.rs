use crate::ftp::types::FileEntry;

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

pub fn parse_listing_line(line: &str) -> Option<FileEntry> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0);
    parse_listing_line_at(line, now)
}

pub fn parse_listing_line_at(line: &str, now: i64) -> Option<FileEntry> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let trimmed = trimmed.trim();
    if trimmed.is_empty() || looks_like_total(trimmed) {
        return None;
    }
    parse_mlsd(trimmed)
        .or_else(|| parse_unix(trimmed, now))
        .or_else(|| parse_windows(trimmed))
}

fn looks_like_total(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.starts_with("total ") || lower.starts_with("总计")
}

pub fn parse_unix(line: &str, now: i64) -> Option<FileEntry> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 9 {
        return None;
    }
    let perms = fields[0];
    let bytes = perms.as_bytes();
    if bytes.len() != 10 {
        return None;
    }
    if !matches!(bytes[0], b'-' | b'd' | b'l' | b'c' | b'b' | b's' | b'p') {
        return None;
    }
    if !bytes[1..]
        .iter()
        .all(|&c| matches!(c, b'r' | b'w' | b'x' | b'-' | b's' | b'S' | b't' | b'T'))
    {
        return None;
    }
    let is_dir = matches!(bytes[0], b'd' | b'l');
    let size: u64 = fields[4].parse().ok()?;
    let month = month_index(fields[5])?;
    let day: u32 = fields[6].parse().ok()?;
    let modified = if fields[7].contains(':') {
        let (hh, mm) = fields[7].split_once(':')?;
        let hh: u32 = hh.parse().ok()?;
        let mm: u32 = mm.parse().ok()?;
        if hh > 23 || mm > 59 {
            return None;
        }
        let mut year = year_of(now);
        let mut ts = ts_from_ymdhm(year, month, day, hh, mm);
        if ts - now > 86_400 {
            year -= 1;
            ts = ts_from_ymdhm(year, month, day, hh, mm);
        }
        Some(ts)
    } else {
        let year: i64 = fields[7].parse().ok()?;
        if !(1970..=2200).contains(&year) {
            return None;
        }
        Some(ts_from_ymdhm(year, month, day, 0, 0))
    };
    let mut name = fields[8..].join(" ");
    if let Some((real, _target)) = name.split_once(" -> ") {
        name = real.to_string();
    }
    if name.is_empty() {
        return None;
    }
    Some(FileEntry {
        name,
        is_dir,
        size,
        modified,
        perms: Some(perms.to_string()),
    })
}

pub fn parse_windows(line: &str) -> Option<FileEntry> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 4 {
        return None;
    }
    let dp: Vec<&str> = fields[0].split(['-', '/']).collect();
    if dp.len() != 3 {
        return None;
    }
    let month: u32 = dp[0].parse().ok()?;
    let day: u32 = dp[1].parse().ok()?;
    let mut year: i64 = dp[2].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if year < 100 {
        year += 2000;
    }
    let (hour12, min, pm) = parse_ampm_time(fields[1])?;
    let hour = if hour12 == 12 {
        if pm { 12 } else { 0 }
    } else if pm {
        hour12 + 12
    } else {
        hour12
    };
    let modified = ts_from_ymdhm(year, month, day, hour, min);
    let is_dir = fields[2].eq_ignore_ascii_case("<DIR>");
    let size: u64 = if is_dir { 0 } else { fields[2].parse().ok()? };
    let name = fields[3..].join(" ");
    if name.is_empty() {
        return None;
    }
    Some(FileEntry {
        name,
        is_dir,
        size,
        modified: Some(modified),
        perms: None,
    })
}

fn parse_ampm_time(token: &str) -> Option<(u32, u32, bool)> {
    let upper = token.to_ascii_uppercase();
    let (body, pm) = if let Some(t) = upper.strip_suffix("AM") {
        (t, false)
    } else if let Some(t) = upper.strip_suffix("PM") {
        (t, true)
    } else {
        (upper.as_str(), false)
    };
    let (hh, mm) = body.split_once(':')?;
    let hh: u32 = hh.parse().ok()?;
    let mm: u32 = mm.parse().ok()?;
    if !(1..=12).contains(&hh) || mm > 59 {
        return None;
    }
    Some((hh, mm, pm))
}

fn parse_mlsd(line: &str) -> Option<FileEntry> {
    let mut pos = 0usize;
    let mut entry_type: Option<String> = None;
    let mut size: Option<u64> = None;
    let mut modify: Option<String> = None;
    let mut mode: Option<String> = None;
    let mut seen_fact = false;
    while let Some(rel) = line[pos..].find(';') {
        let token = line[pos..pos + rel].trim();
        let Some((key, value)) = token.split_once('=') else {
            break;
        };
        seen_fact = true;
        pos += rel + 1;
        match key.to_ascii_lowercase().as_str() {
            "type" => entry_type = Some(value.to_ascii_lowercase()),
            "size" => size = value.parse().ok(),
            "modify" => modify = Some(value.to_string()),
            "unix.mode" => mode = Some(value.to_string()),
            _ => {}
        }
    }
    if !seen_fact {
        return None;
    }
    let name = line[pos..].trim();
    if name.is_empty() {
        return None;
    }
    let is_dir = matches!(entry_type.as_deref(), Some("dir" | "cdir" | "pdir"));
    let modified = modify.as_deref().and_then(parse_mlsd_time);
    let perms = mode.as_deref().map(octal_mode_to_perm);
    Some(FileEntry {
        name: name.to_string(),
        is_dir,
        size: size.unwrap_or(0),
        modified,
        perms,
    })
}

fn parse_mlsd_time(value: &str) -> Option<i64> {
    let digits: Vec<char> = value.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 14 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let s: String = digits[range].iter().collect();
        s.parse().ok()
    };
    let year = num(0..4)?;
    let month = u32::try_from(num(4..6)?).ok()?;
    let day = u32::try_from(num(6..8)?).ok()?;
    let hh = u32::try_from(num(8..10)?).ok()?;
    let mm = u32::try_from(num(10..12)?).ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hh > 23 || mm > 59 {
        return None;
    }
    Some(ts_from_ymdhm(year, month, day, hh, mm))
}

fn octal_mode_to_perm(mode: &str) -> String {
    let parsed = u32::from_str_radix(mode.trim_start_matches('0'), 8).unwrap_or(0) & 0o777;
    let mut s = String::with_capacity(9);
    for shift in [6u32, 3, 0] {
        let tri = (parsed >> shift) & 0o7;
        s.push(if tri & 0o4 != 0 { 'r' } else { '-' });
        s.push(if tri & 0o2 != 0 { 'w' } else { '-' });
        s.push(if tri & 0o1 != 0 { 'x' } else { '-' });
    }
    s
}

fn month_index(token: &str) -> Option<u32> {
    let lower = token.to_ascii_lowercase();
    let first3: String = lower.chars().take(3).collect();
    let idx = MONTHS.iter().position(|m| *m == first3)?;
    u32::try_from(idx).ok().map(|i| i + 1)
}

fn year_of(now: i64) -> i64 {
    let days = now.div_euclid(86_400);
    let mut year = 1970;
    let mut remaining = days;
    loop {
        let len = civil_year_len(year);
        if remaining < len {
            break;
        }
        remaining -= len;
        year += 1;
    }
    year
}

fn civil_year_len(year: i64) -> i64 {
    if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
        366
    } else {
        365
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn ts_from_ymdhm(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    days_from_civil(year, month, day) * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60
}

pub fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

#[cfg(test)]
mod tests {
    use super::{
        days_from_civil, parse_listing_line_at, parse_mlsd, parse_unix, parse_windows, sort_entries,
    };
    use crate::ftp::types::FileEntry;

    const NOW: i64 = 1_717_228_800;

    fn ts(y: i64, m: u32, d: u32, hh: u32, mm: u32) -> i64 {
        days_from_civil(y, m, d) * 86_400 + i64::from(hh) * 3_600 + i64::from(mm) * 60
    }

    fn parse(line: &str) -> FileEntry {
        parse_listing_line_at(line, NOW).unwrap_or_else(|| panic!("应能解析: {line}"))
    }

    #[test]
    fn unix_basic_dir_and_file() {
        let e = parse("drwxr-xr-x   2 ftp      ftp          4096 Jan 12  2024 pub");
        assert!(e.is_dir);
        assert_eq!(e.name, "pub");
        assert_eq!(e.size, 4096);
        assert_eq!(e.modified, Some(ts(2024, 1, 12, 0, 0)));
        assert_eq!(e.perms.as_deref(), Some("drwxr-xr-x"));

        let e = parse("-rw-r--r--   1 ftp      ftp           512 Jan 12  2024 readme.txt");
        assert!(!e.is_dir);
        assert_eq!(e.name, "readme.txt");
        assert_eq!(e.size, 512);
    }

    #[test]
    fn unix_recent_time_infers_year_and_rolls_back() {
        let e = parse("drwxr-xr-x   5 ftp      ftp          4096 Sep 06 10:30 logs");
        assert_eq!(
            e.modified,
            Some(ts(2023, 9, 6, 10, 30)),
            "未来日期应回退一年"
        );

        let e = parse("-rw-r--r--   1 ftp      ftp          123 Mar 01 08:00 a.txt");
        assert_eq!(e.modified, Some(ts(2024, 3, 1, 8, 0)), "过去日期用当前年");
    }

    #[test]
    fn unix_numeric_ids_and_symlink() {
        let e = parse("-rw-r--r--   1 0        0         1048576 Aug 01  2023 big.bin");
        assert_eq!(e.size, 1_048_576);
        assert_eq!(e.modified, Some(ts(2023, 8, 1, 0, 0)));

        let e = parse("lrwxrwxrwx   1 ftp      ftp             7 Jan 01  2024 latest -> pub");
        assert!(e.is_dir, "符号链接按目录处理");
        assert_eq!(e.name, "latest");
    }

    #[test]
    fn unix_names_with_spaces_and_unicode() {
        let e = parse("drwxrwxrwx   2 ftp      ftp          4096 Feb 29 12:00 leap dir");
        assert_eq!(e.name, "leap dir");
        assert_eq!(e.modified, Some(ts(2024, 2, 29, 12, 0)));

        let e = parse("drwxr-xr-x   3 ftp      ftp          4096 Mar 03  2023 我 的 文件");
        assert_eq!(e.name, "我 的 文件");
    }

    #[test]
    fn unix_dotfile_and_hidden_and_large_size() {
        let e = parse("-rw-r--r-- 1 ftp ftp 0 Dec 31 23:59 .hidden");
        assert_eq!(e.name, ".hidden");
        assert_eq!(e.size, 0);

        let e = parse("-rw-r--r--   1 ftp      ftp    1099511627776 Jul 20  2024 huge.img");
        assert_eq!(e.size, 1_099_511_627_776);
    }

    #[test]
    fn unix_special_perm_chars() {
        let e = parse("drwxrwsr-x   2 ftp      ftp          4096 Jan 12  2024 shared");
        assert_eq!(e.name, "shared");
        let e = parse("-rwSr--r--   1 ftp      ftp           10 Jan 12  2024 setuid.bin");
        assert_eq!(e.name, "setuid.bin");
    }

    #[test]
    fn unix_rejects_garbage() {
        assert!(parse_unix("total 20", NOW).is_none());
        assert!(parse_listing_line_at("total 20", NOW).is_none());
        assert!(parse_listing_line_at("", NOW).is_none());
        assert!(parse_listing_line_at("  ", NOW).is_none());
        assert!(parse_listing_line_at("random text line", NOW).is_none());
    }

    #[test]
    fn windows_basic_dir_and_file() {
        let e = parse("03-15-24  10:32AM       <DIR>          pub");
        assert!(e.is_dir);
        assert_eq!(e.name, "pub");
        assert_eq!(e.modified, Some(ts(2024, 3, 15, 10, 32)));
        assert_eq!(e.size, 0);

        let e = parse("06-25-24  08:11AM       1234567 file.txt");
        assert!(!e.is_dir);
        assert_eq!(e.size, 1_234_567);
        assert_eq!(e.modified, Some(ts(2024, 6, 25, 8, 11)));
    }

    #[test]
    fn windows_4digit_year_and_spaces() {
        let e = parse("01-01-2025  12:00AM       <DIR>          New folder");
        assert_eq!(e.name, "New folder");
        assert_eq!(e.modified, Some(ts(2025, 1, 1, 0, 0)), "12AM 即 0 点");

        let e = parse("07-04-24  03:05PM       10485760 big archive.zip");
        assert_eq!(e.name, "big archive.zip");
        assert_eq!(e.modified, Some(ts(2024, 7, 4, 15, 5)));
    }

    #[test]
    fn windows_edge_names_and_zero() {
        let e = parse("12-31-23  11:59PM              0 zero.log");
        assert_eq!(e.name, "zero.log");
        assert_eq!(e.modified, Some(ts(2023, 12, 31, 23, 59)));

        let e = parse("05-05-05  05:05AM        999 data#1.txt");
        assert_eq!(e.name, "data#1.txt");

        let e = parse("11-11-11  11:11AM       <DIR>          .hidden");
        assert_eq!(e.name, ".hidden");

        let e = parse("08-20-24  09:00AM       <DIR>          我 的 目 录");
        assert_eq!(e.name, "我 的 目 录");
    }

    #[test]
    fn windows_rejects_bad_time() {
        assert!(parse_windows("13-15-24  25:99AM       12 x.txt").is_none());
        assert!(parse_windows("garbage").is_none());
    }

    #[test]
    fn mlsd_standard_facts() {
        let e = parse("type=dir;size=0;modify=20240112103000;UNIX.mode=0755; pub");
        assert!(e.is_dir);
        assert_eq!(e.name, "pub");
        assert_eq!(e.modified, Some(ts(2024, 1, 12, 10, 30)));
        assert_eq!(e.perms.as_deref(), Some("rwxr-xr-x"));

        let e = parse("type=file;size=512;modify=20240112080000;README.md");
        assert!(!e.is_dir);
        assert_eq!(e.name, "README.md");
        assert_eq!(e.size, 512);
    }

    #[test]
    fn mlsd_spaces_after_semicolon_and_missing_facts() {
        let e = parse("type=dir; modify=20230707120000; 我 的目录");
        assert_eq!(e.name, "我 的目录");
        assert!(e.is_dir);
        assert_eq!(e.perms, None);

        let e = parse("type=file; a; b.txt");
        assert_eq!(e.name, "a; b.txt", "文件名包含分号仍可解析");
    }

    #[test]
    fn mlsd_cdir_pdir_and_fractional_time() {
        let e = parse("type=cdir;modify=20240101000000;.");
        assert!(e.is_dir);
        let e = parse("type=pdir;modify=20240101000000;..");
        assert!(e.is_dir);
        let e = parse("type=file;size=7;modify=20240112103000.123; tiny.txt");
        assert_eq!(e.name, "tiny.txt");
        assert_eq!(e.size, 7);
    }

    #[test]
    fn mlsd_large_size_and_octal_perm_variant() {
        let e = parse("type=file;size=4294967296;UNIX.mode=644;huge.dat");
        assert_eq!(e.size, 4_294_967_296);
        assert_eq!(e.perms.as_deref(), Some("rw-r--r--"));
    }

    #[test]
    fn mlsd_rejects_non_mlsd_lines() {
        assert!(parse_mlsd("drwxr-xr-x   2 ftp      ftp          4096 Jan 12  2024 pub").is_none());
        assert!(parse_mlsd("03-15-24  10:32AM       <DIR>          pub").is_none());
        assert!(parse_mlsd("type=dir;").is_none());
    }

    #[test]
    fn sort_dirs_first_case_insensitive() {
        let mut v = vec![
            FileEntry {
                name: "zeta".into(),
                is_dir: false,
                ..Default::default()
            },
            FileEntry {
                name: "Beta".into(),
                is_dir: false,
                ..Default::default()
            },
            FileEntry {
                name: "adir".into(),
                is_dir: true,
                ..Default::default()
            },
        ];
        sort_entries(&mut v);
        assert_eq!(v[0].name, "adir");
        assert_eq!(v[1].name, "Beta");
        assert_eq!(v[2].name, "zeta");
    }
}
