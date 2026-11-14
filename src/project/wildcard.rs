//! MSBuild-style wildcards in item paths: `*` and `?` within a folder or
//! file name, and `**` for any number of folders. Matching is case-insensitive,
//! as on Windows.

use std::path::{Path, PathBuf};

pub fn has_wildcard(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

/// Files under `base` matching `pattern` (relative to `base`), sorted.
pub fn expand(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let segments = segments(pattern);
    // Walk from the deepest folder that has no wildcard in it.
    let fixed = segments.iter().take_while(|s| !has_wildcard(s)).count();
    let mut dir = base.to_path_buf();
    for segment in &segments[..fixed] {
        dir.push(segment);
    }
    let rest = &segments[fixed..];
    let max_depth = if rest.contains(&"**") { usize::MAX } else { rest.len() };

    let mut found = Vec::new();
    walk(&dir, &mut Vec::new(), max_depth, &mut |relative| {
        if match_segments(rest, relative) {
            found.push(relative.iter().fold(dir.clone(), |path, part| path.join(part)));
        }
    });
    found.sort();
    found
}

/// Whether `path` matches `pattern`; both should be absolute and normalized.
pub fn matches(pattern: &str, path: &Path) -> bool {
    let path = path.to_string_lossy();
    match_segments(&segments(pattern), &segments(&path))
}

fn segments(path: &str) -> Vec<&str> {
    path.split(['\\', '/']).filter(|s| !s.is_empty() && *s != ".").collect()
}

/// Visit every file under `dir`, passing its path relative to the walk's start.
fn walk(dir: &Path, relative: &mut Vec<String>, depth_left: usize, visit: &mut dyn FnMut(&[String])) {
    if depth_left == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else { continue };
        relative.push(entry.file_name().to_string_lossy().into_owned());
        if file_type.is_dir() {
            walk(&entry.path(), relative, depth_left - 1, visit);
        } else {
            visit(relative);
        }
        relative.pop();
    }
}

fn match_segments<S: AsRef<str>>(pattern: &[&str], path: &[S]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| match_segments(rest, &path[skip..])),
        Some((first, rest)) => match path.split_first() {
            Some((name, path_rest)) => match_name(first, name.as_ref()) && match_segments(rest, path_rest),
            None => false,
        },
    }
}

/// Match one name against a pattern with `*` (any run) and `?` (one character).
fn match_name(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let name: Vec<char> = name.to_lowercase().chars().collect();
    let (mut p, mut n) = (0, 0);
    // Where to resume after the most recent `*`: (pattern index, name index).
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p + 1, n));
            p += 1;
        } else if let Some((star_p, star_n)) = star {
            // Let the last `*` swallow one more character and retry.
            p = star_p;
            n = star_n + 1;
            star = Some((star_p, star_n + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(match_name("*.cpp", "main.cpp"));
        assert!(match_name("*.CPP", "Main.cpp"));
        assert!(!match_name("*.cpp", "main.h"));
        assert!(match_name("a?c*", "abcdef"));
        assert!(match_name("*a*b*", "xxaxxbxx"));
        assert!(!match_name("*a*b", "xxaxxbxx"));
        assert!(match_name("*", ""));
    }

    #[test]
    fn segments_and_globstar() {
        let path = ["src", "net", "socket.cpp"];
        assert!(match_segments(&["src", "**", "*.cpp"], &path));
        assert!(match_segments(&["**", "*.cpp"], &path));
        assert!(match_segments(&["src", "*", "*.cpp"], &path));
        assert!(!match_segments(&["src", "*.cpp"], &path));
        assert!(match_segments(&["src", "**", "net", "**", "socket.cpp"], &path));
    }

    #[test]
    fn expands_files_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["a.cpp", "b.h", "sub/c.cpp", "sub/deeper/d.cpp"] {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let names = |pattern: &str| -> Vec<String> {
            expand(dir.path(), pattern)
                .iter()
                .map(|p| p.strip_prefix(dir.path()).unwrap().to_string_lossy().replace('\\', "/"))
                .collect()
        };
        assert_eq!(names("*.cpp"), ["a.cpp"]);
        assert_eq!(names("**\\*.cpp"), ["a.cpp", "sub/c.cpp", "sub/deeper/d.cpp"]);
        assert_eq!(names("sub\\*.cpp"), ["sub/c.cpp"]);
        assert_eq!(names("sub/**/*.cpp"), ["sub/c.cpp", "sub/deeper/d.cpp"]);
        assert!(names("missing\\*.cpp").is_empty());
    }
}
