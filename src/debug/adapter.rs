//! Finding `lldb-dap`, LLVM's debug adapter for LLDB.
//!
//! Visual Studio ships `lldb-dap.exe` with its Clang tools, but not the
//! `liblldb.dll` it needs, so it crashes on start. Any LLDB build's
//! `liblldb.dll` works with it (LLDB's API is ABI-stable), so we also look
//! for one (an LLVM install, or the CodeLLDB extension for VS Code) and put
//! its folder on the adapter's `PATH`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// How to start the debug adapter.
#[derive(Clone, Debug)]
pub struct Adapter {
    pub program: PathBuf,
    /// Folder holding `liblldb.dll`, when it isn't next to the program.
    pub library_dir: Option<PathBuf>,
}

impl Adapter {
    /// A command that starts the adapter, talking DAP on stdin/stdout.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        if let Some(dir) = &self.library_dir {
            let mut path = std::ffi::OsString::from(dir);
            if let Some(old) = std::env::var_os("PATH") {
                path.push(";");
                path.push(old);
            }
            command.env("PATH", path);
            // A borrowed liblldb crashes lldb-dap when `~/.lldbinit` loads
            // Python scripts (Unreal's data formatters do), so skip it.
            command.arg("--no-lldbinit");
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(crate::build::CREATE_NO_WINDOW);
        }
        command
    }
}

const LIBRARY: &str = if cfg!(windows) { "liblldb.dll" } else { "liblldb.so" };
const PROGRAM: &str = if cfg!(windows) { "lldb-dap.exe" } else { "lldb-dap" };

/// Find `lldb-dap` and a `liblldb` for it. `ZBUGGER_LLDB_DAP` overrides the search.
pub fn find_adapter() -> Result<Adapter, String> {
    let mut programs: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("ZBUGGER_LLDB_DAP") {
        programs.push(path.into());
    }
    if let Some(dir) = std::env::var_os("ProgramFiles") {
        programs.push(Path::new(&dir).join("LLVM\\bin").join(PROGRAM));
    }
    programs.extend(visual_studio_dirs().into_iter().map(|dir| dir.join("VC\\Tools\\Llvm\\x64\\bin").join(PROGRAM)));
    if let Some(path) = std::env::var_os("PATH") {
        programs.extend(std::env::split_paths(&path).map(|dir| dir.join(PROGRAM)));
    }
    let program = programs.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        "lldb-dap wasn't found. Install LLVM (winget install LLVM.LLVM) or Visual Studio's \
         \"C++ Clang tools\", or set ZBUGGER_LLDB_DAP to lldb-dap's path."
            .to_owned()
    })?;
    let dir = program.parent().unwrap_or(Path::new("")).to_path_buf();
    if dir.join(LIBRARY).is_file() || !cfg!(windows) {
        return Ok(Adapter { program, library_dir: None });
    }
    let library_dir = library_dirs().into_iter().find(|d| d.join(LIBRARY).is_file()).ok_or_else(|| {
        format!(
            "{} needs {LIBRARY}, which isn't next to it. Install LLVM (winget install LLVM.LLVM) \
             or the CodeLLDB extension for VS Code, which include one.",
            program.display()
        )
    })?;
    Ok(Adapter { program, library_dir: Some(library_dir) })
}

/// Visual Studio installations, newest first.
fn visual_studio_dirs() -> Vec<PathBuf> {
    let Some(program_files) = std::env::var_os("ProgramFiles(x86)") else { return Vec::new() };
    let vswhere = PathBuf::from(program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
    let mut command = Command::new(vswhere);
    command.args(["-all", "-products", "*", "-sort", "-property", "installationPath"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::build::CREATE_NO_WINDOW);
    }
    let Ok(output) = command.output() else { return Vec::new() };
    String::from_utf8_lossy(&output.stdout).lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect()
}

/// Folders that may hold a `liblldb.dll`.
fn library_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("ProgramFiles") {
        dirs.push(Path::new(&dir).join("LLVM\\bin"));
    }
    // CodeLLDB, newest version first.
    if let Some(home) = std::env::var_os("USERPROFILE") {
        for editor in [".vscode", ".vscode-insiders", ".cursor"] {
            let Ok(entries) = std::fs::read_dir(Path::new(&home).join(editor).join("extensions")) else { continue };
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("vadimcn.vscode-lldb-"))
                .map(|e| e.path().join("lldb\\bin"))
                .collect();
            found.sort_by(|a, b| version_of(b).cmp(&version_of(a)));
            dirs.extend(found);
        }
    }
    dirs
}

/// `[1, 12, 3]` from `...\vadimcn.vscode-lldb-1.12.3\lldb\bin`.
fn version_of(dir: &Path) -> Vec<u32> {
    let name = dir.parent().and_then(Path::parent).and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned());
    let name = name.unwrap_or_default();
    let version = name.rsplit('-').next().unwrap_or("");
    version.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_versions_sort_numerically() {
        let dir = |v: &str| PathBuf::from(format!(r"C:\x\vadimcn.vscode-lldb-{v}\lldb\bin"));
        assert_eq!(version_of(&dir("1.12.3")), vec![1, 12, 3]);
        assert!(version_of(&dir("1.12.3")) > version_of(&dir("1.9.10")));
    }
}
