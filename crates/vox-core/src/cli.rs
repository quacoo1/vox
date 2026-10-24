//! Command-line parsing (shell.md §2, reader-playback.md §2.8, contracts.md §1.8).

use std::path::{Path, PathBuf};

/// Parsed switches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliArgs {
    /// `--hidden`: start in the tray.
    pub hidden: bool,
    /// `--read-clipboard`.
    pub read_clipboard: bool,
    /// `--read-file <path>` / `--read-file=<path>` (empty → `None`).
    pub read_file: Option<String>,
    /// `--read-text <text>` / `--read-text=<text>` (empty → `None`).
    pub read_text: Option<String>,
}

/// The one action an invocation performs (priority `--read-text` > `--read-clipboard` > `--read-file`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    ReadText(String),
    ReadClipboard,
    ReadFile(String),
}

impl CliArgs {
    pub fn action(&self) -> Option<CliAction> {
        if let Some(t) = &self.read_text {
            Some(CliAction::ReadText(t.clone()))
        } else if self.read_clipboard {
            Some(CliAction::ReadClipboard)
        } else {
            self.read_file.as_ref().map(|f| CliAction::ReadFile(f.clone()))
        }
    }
    pub fn has_read_action(&self) -> bool {
        self.read_text.is_some() || self.read_clipboard || self.read_file.is_some()
    }
}

/// `handleArgs` (shell.md §2.2): the one action to run, and whether to show the main window — only when nothing is
/// acted on **and** the call came from a second instance **and** `--hidden` was not given (launching Vox again brings
/// the window forward; a second `vox.exe --hidden` does nothing). A `--read-file` counts as acted on even when the
/// file later fails to open.
pub fn cli_outcome(args: &CliArgs, from_second_instance: bool) -> (Option<CliAction>, bool) {
    let action = args.action();
    let show_main = action.is_none() && from_second_instance && !args.hidden;
    (action, show_main)
}

/// `parseCliArgs` over the full argv (exe path included; non-`--` tokens and unknown switches are ignored).
/// `--name=value` splits at the first `=` (only when `=` is not the first character after `--`… i.e. at index > 0);
/// otherwise a value-taking switch consumes the **next** token whatever it is. Names are case-sensitive.
pub fn parse_cli_args<S: AsRef<str>>(argv: &[S]) -> CliArgs {
    let mut out = CliArgs::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_ref();
        i += 1;
        if !arg.starts_with("--") {
            continue;
        }
        let (name, inline) = match arg.find('=') {
            Some(eq) if eq > 0 => (&arg[..eq], Some(&arg[eq + 1..])),
            _ => (arg, None),
        };
        let value = |i: &mut usize| -> Option<String> {
            match inline {
                Some(v) => Some(v.to_string()),
                None => {
                    if *i < argv.len() {
                        let v = argv[*i].as_ref().to_string();
                        *i += 1;
                        Some(v)
                    } else {
                        None
                    }
                }
            }
        };
        match name {
            "--hidden" => out.hidden = true,
            "--read-clipboard" => out.read_clipboard = true,
            "--read-file" => out.read_file = value(&mut i).filter(|v| !v.is_empty()),
            "--read-text" => out.read_text = value(&mut i).filter(|v| !v.is_empty()),
            _ => {}
        }
    }
    out
}

/// Resolves a `--read-file` path against the invoking process's working directory (`path.resolve(cwd, p)`).
pub fn resolve_read_file(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome() {
        let none = CliArgs::default();
        assert_eq!(cli_outcome(&none, true), (None, true));
        assert_eq!(cli_outcome(&none, false), (None, false));
        let hidden = CliArgs { hidden: true, ..CliArgs::default() };
        assert_eq!(cli_outcome(&hidden, true), (None, false));
        let file = CliArgs { read_file: Some("a.txt".into()), ..CliArgs::default() };
        assert_eq!(cli_outcome(&file, true), (Some(CliAction::ReadFile("a.txt".into())), false));
    }

    #[test]
    fn parsing_edge_cases() {
        let a = parse_cli_args(&["vox.exe", "--read-text", "--hidden"]);
        assert_eq!(a.read_text.as_deref(), Some("--hidden"));
        assert!(!a.hidden);
        let a = parse_cli_args(&["vox.exe", "--read-text=", "x"]);
        assert_eq!(a.read_text, None);
        let a = parse_cli_args(&["vox.exe", "--read-file"]);
        assert_eq!(a.read_file, None);
        let a = parse_cli_args(&["vox.exe", "--hidden=x", "--read-clipboard=1", "--updated", "pos", "--READ-TEXT", "y"]);
        assert!(a.hidden && a.read_clipboard);
        assert_eq!(a.read_text, None);
        let a = parse_cli_args(&["--read-file=C:\\a b.txt", "--read-clipboard", "--read-text", "hi"]);
        assert_eq!(a.action(), Some(CliAction::ReadText("hi".into())));
        let a = parse_cli_args(&["--read-file", "x.txt", "--read-clipboard"]);
        assert_eq!(a.action(), Some(CliAction::ReadClipboard));
        assert!(parse_cli_args(&["--read-file", "x.txt"]).has_read_action());
        assert_eq!(parse_cli_args::<&str>(&[]).action(), None);
    }

    #[test]
    fn resolve() {
        assert_eq!(resolve_read_file(Path::new("C:\\work"), "a.txt"), PathBuf::from("C:\\work\\a.txt"));
        assert_eq!(resolve_read_file(Path::new("C:\\work"), "D:\\b.txt"), PathBuf::from("D:\\b.txt"));
    }
}
