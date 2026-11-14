//! Debugging with LLDB, through its Debug Adapter Protocol server `lldb-dap`.

mod adapter;
mod dap;
mod session;

pub use adapter::find_adapter;
pub use session::{long_path, DebugCommand, DebugEvent, DebugSession, DebugState, Frame, Launch};

/// Split a command line into arguments the way Windows programs do
/// (`CommandLineToArgvW`): spaces separate, double quotes group, and
/// backslashes only escape quotes.
pub fn split_arguments(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let mut count = 1;
                while chars.peek() == Some(&'\\') {
                    chars.next();
                    count += 1;
                }
                if chars.peek() == Some(&'"') {
                    current.extend(std::iter::repeat_n('\\', count / 2));
                    if count % 2 == 1 {
                        chars.next();
                        current.push('"');
                    }
                } else {
                    current.extend(std::iter::repeat_n('\\', count));
                }
                in_arg = true;
            }
            '"' => {
                if quoted && chars.peek() == Some(&'"') {
                    chars.next(); // "" inside quotes is a literal quote
                    current.push('"');
                } else {
                    quoted = !quoted;
                }
                in_arg = true;
            }
            ' ' | '\t' if !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(current);
    }
    args
}

#[cfg(test)]
pub(crate) mod tests;
