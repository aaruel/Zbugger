//! Path and file helpers shared by the project and solution loaders.

use std::path::{Component, Path, PathBuf};

use super::ProjectError;

/// Project files use `\`; use the platform's separator.
pub fn native_separators(path: &str) -> String {
    if std::path::MAIN_SEPARATOR == '\\' { path.to_owned() } else { path.replace('\\', "/") }
}

/// Resolve `.` and `..` without touching the disk (files may not exist).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(out.components().next_back(), Some(Component::Normal(_))) || !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Read a project file, handling the UTF-8 and UTF-16 byte-order marks
/// Visual Studio may write.
pub fn read_text(path: &Path) -> Result<String, ProjectError> {
    let bytes = std::fs::read(path).map_err(|e| ProjectError::Io(path.to_path_buf(), e))?;
    if let Some(utf16) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = utf16.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return Ok(String::from_utf16_lossy(&units));
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
}
