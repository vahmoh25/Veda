//! Absolute, `/`-separated paths as the VFS uses them.
//!
//! Paths are plain strings. [`normalize`] and [`resolve`] produce the
//! canonical form used everywhere else: absolute, no `.` or `..`
//! components, no repeated or trailing slashes (except the root, `/`).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;

/// The user's home directory.
pub const HOME: &str = "/home/user";
/// The read-only system image.
pub const SYSTEM: &str = "/system";

/// Normalises an absolute path: collapses `.`, `..` (never above the root)
/// and repeated or trailing slashes. An empty path becomes `/`.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut out = String::new();
    for p in parts {
        out.push('/');
        out.push_str(p);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Resolves `path` against the directory `cwd`: absolute paths stand on
/// their own, `~` and `~/...` start at the home directory, anything else is
/// relative to `cwd`. The result is normalised.
pub fn resolve(cwd: &str, path: &str) -> String {
    if path == "~" {
        return HOME.to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return normalize(&format!("{HOME}/{rest}"));
    }
    if path.starts_with('/') { normalize(path) } else { normalize(&format!("{cwd}/{path}")) }
}

/// `dir/name`, with exactly one slash between them.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// The directory containing `path` (`/a/b` → `/a`, `/a` → `/`, and `/` for
/// the root or a bare name). A trailing slash is ignored.
pub fn parent(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &trimmed[..i],
    }
}

/// The last component of `path` (`/a/b.txt` → `b.txt`); empty for `/`. A
/// trailing slash is ignored.
pub fn file_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// The file name of `path` without its extension (`/a/b.tar.gz` →
/// `b.tar`). A leading dot does not start an extension (`.profile` stays).
pub fn file_stem(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// The lower-case extension of a file name, without the dot (`Photo.JPG` →
/// `jpg`); empty if there is none. A leading dot does not start an
/// extension.
pub fn extension(name: &str) -> String {
    let name = file_name(name);
    match name.rfind('.') {
        Some(i) if i > 0 => name[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// `path` with the home directory written as `~` (for prompts and titles).
pub fn display_path(path: &str) -> String {
    if path == HOME {
        "~".to_string()
    } else if let Some(rest) = path.strip_prefix(HOME).filter(|r| r.starts_with('/')) {
        format!("~{rest}")
    } else {
        path.to_string()
    }
}

/// True if `path` equals `dir` or lies below it (`/a/bc` is not below `/a/b`).
pub fn is_within(path: &str, dir: &str) -> bool {
    path == dir || (path.starts_with(dir) && (dir == "/" || path.as_bytes().get(dir.len()) == Some(&b'/')))
}

/// True for paths inside the read-only system image.
pub fn is_read_only(path: &str) -> bool {
    is_within(path, SYSTEM)
}

/// True if both paths are on the same file system, so that a move between
/// them is a rename (moving between `/system` and the rest copies).
pub fn same_volume(a: &str, b: &str) -> bool {
    is_read_only(a) == is_read_only(b)
}

/// Matches `name` against a shell pattern: `*` matches any run of
/// characters, `?` any single character, everything else itself.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    // Where the last `*` was, and the name position it currently covers to.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// True if a word contains shell wildcards (`*` or `?`).
pub fn has_wildcards(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// A free name based on `base`: `base` itself, else `stem (2).ext`,
/// `stem (3).ext`, ... — the first that `taken` says is not in use.
pub fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 => (&base[..i], &base[i..]),
        _ => (base, ""),
    };
    let mut n = 2u64;
    loop {
        let candidate = format!("{stem} ({n}){ext}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Compares file names in natural order: case-insensitively, with runs of
/// digits compared by value (`file2` < `file10`, `007` = `7`). Names that
/// compare equal this way are ordered by their bytes, so the order is total.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a, b);
    loop {
        let (Some(cx), Some(cy)) = (x.chars().next(), y.chars().next()) else {
            return match (x.is_empty(), y.is_empty()) {
                (true, true) => a.cmp(b),
                (true, false) => Ordering::Less,
                _ => Ordering::Greater,
            };
        };
        if cx.is_ascii_digit() && cy.is_ascii_digit() {
            let nx = x.bytes().take_while(u8::is_ascii_digit).count();
            let ny = y.bytes().take_while(u8::is_ascii_digit).count();
            let (dx, dy) = (x[..nx].trim_start_matches('0'), y[..ny].trim_start_matches('0'));
            // Without leading zeros, a longer run of digits is a larger number.
            let ord = dx.len().cmp(&dy.len()).then_with(|| dx.cmp(dy));
            if ord != Ordering::Equal {
                return ord;
            }
            x = &x[nx..];
            y = &y[ny..];
        } else {
            let (lx, ly) = (cx.to_lowercase().next().unwrap_or(cx), cy.to_lowercase().next().unwrap_or(cy));
            if lx != ly {
                return lx.cmp(&ly);
            }
            x = &x[cx.len_utf8()..];
            y = &y[cy.len_utf8()..];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn normalize_collapses_dots_and_slashes() {
        assert_eq!(normalize("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(normalize("//a///b//"), "/a/b");
        assert_eq!(normalize("/a/b/"), "/a/b");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize(""), "/");
        // Never above the root.
        assert_eq!(normalize("/../.."), "/");
        assert_eq!(normalize("/a/../../b"), "/b");
        assert_eq!(normalize("/a/.."), "/");
    }

    #[test]
    fn resolve_handles_relative_absolute_and_home() {
        assert_eq!(resolve("/home/user", "Documents"), "/home/user/Documents");
        assert_eq!(resolve("/home/user/Documents", ".."), "/home/user");
        assert_eq!(resolve("/home/user", "../../.."), "/");
        assert_eq!(resolve("/tmp", "/system/bin/"), "/system/bin");
        assert_eq!(resolve("/tmp", "~"), HOME);
        assert_eq!(resolve("/tmp", "~/Music/../Pictures"), "/home/user/Pictures");
        assert_eq!(resolve("/", "a"), "/a");
        assert_eq!(resolve("/a", "."), "/a");
        // `~` only means home at the start of a path.
        assert_eq!(resolve("/tmp", "a~b"), "/tmp/a~b");
    }

    #[test]
    fn joining_and_splitting() {
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(join("/", "b"), "/b");
        assert_eq!(join("/a/", "b"), "/a/b");
        assert_eq!(parent("/a/b/c.txt"), "/a/b");
        assert_eq!(parent("/a"), "/");
        assert_eq!(parent("/"), "/");
        assert_eq!(parent("/a/b/"), "/a");
        assert_eq!(parent("name"), "/");
        assert_eq!(file_name("/a/b.txt"), "b.txt");
        assert_eq!(file_name("/a/b/"), "b");
        assert_eq!(file_name("/"), "");
        assert_eq!(file_name("c"), "c");
        assert_eq!(file_stem("/x/Song.Final.qoa"), "Song.Final");
        assert_eq!(file_stem("/x/.profile"), ".profile");
        assert_eq!(file_stem("/x/README"), "README");
    }

    #[test]
    fn extensions() {
        assert_eq!(extension("Photo.JPG"), "jpg");
        assert_eq!(extension("/a/b.tar.gz"), "gz");
        assert_eq!(extension(".bashrc"), "");
        assert_eq!(extension("Makefile"), "");
        assert_eq!(extension("/a.dir/file"), "");
        assert_eq!(extension("trailing."), "");
    }

    #[test]
    fn display_and_containment() {
        assert_eq!(display_path(HOME), "~");
        assert_eq!(display_path("/home/user/Music"), "~/Music");
        assert_eq!(display_path("/home/username"), "/home/username");
        assert_eq!(display_path("/system"), "/system");
        assert!(is_within("/a/b", "/a"));
        assert!(is_within("/a", "/a"));
        assert!(!is_within("/ab", "/a"));
        assert!(is_within("/anything", "/"));
        assert!(is_read_only("/system"));
        assert!(is_read_only("/system/bin/init.exe"));
        assert!(!is_read_only("/systemd"));
        assert!(!is_read_only("/home/user"));
        assert!(same_volume("/home/user/a", "/tmp/b"));
        assert!(!same_volume("/system/a", "/tmp/b"));
    }

    #[test]
    fn globbing() {
        assert!(glob_match("*.txt", "notes.txt"));
        assert!(!glob_match("*.txt", "notes.txt.bak"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*", ""));
        assert!(glob_match("**a*", "banana"));
        assert!(glob_match("b*n*a", "banana"));
        assert!(!glob_match("b*x", "banana"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "a"));
        assert!(glob_match("fïlé?", "fïléé"));
        assert!(has_wildcards("*.rs") && has_wildcards("a?") && !has_wildcards("plain"));
    }

    #[test]
    fn unique_names_count_up_before_the_extension() {
        let taken = ["a.txt", "a (2).txt", "New folder", ".bashrc"];
        let is_taken = |n: &str| taken.contains(&n);
        assert_eq!(unique_name("b.txt", is_taken), "b.txt");
        assert_eq!(unique_name("a.txt", is_taken), "a (3).txt");
        assert_eq!(unique_name("New folder", is_taken), "New folder (2)");
        assert_eq!(unique_name(".bashrc", is_taken), ".bashrc (2)");
    }

    #[test]
    fn natural_order() {
        let mut names = vec!["file10", "File2", "file1", "file02b", "a", "B", "file2a"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["a", "B", "file1", "File2", "file2a", "file02b", "file10"]);
        // Equal by value and case: the bytes decide, so the order is total.
        assert_eq!(natural_cmp("x007", "x7"), "x007".cmp("x7"));
        assert_eq!(natural_cmp("abc", "ABC"), "abc".cmp("ABC"));
        // Digit runs of any length.
        assert_eq!(natural_cmp("img99999999999999999999", "img100000000000000000000"), Ordering::Less);
        // Case folding beyond ASCII.
        assert_eq!(natural_cmp("Ébc", "ébd"), Ordering::Less);
        assert_eq!(natural_cmp("", "a"), Ordering::Less);
        assert_eq!(natural_cmp("a", ""), Ordering::Greater);
    }
}
