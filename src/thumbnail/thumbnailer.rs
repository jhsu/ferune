//! The system's thumbnailers: `*.thumbnailer` files under `$XDG_DATA_HOME/thumbnailers` and
//! each `$XDG_DATA_DIRS/thumbnailers`, which GNOME, Xfce and others share. Each names a
//! program, its arguments with `%u`/`%i`/`%o`/`%s` field codes, and the MIME types it handles.
//! Programs run directly from the parsed argument list, never through a shell.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnailer {
    /// The `Exec` line split into arguments, field codes still in place.
    pub exec: Vec<String>,
    pub mime_types: Vec<String>,
    pub try_exec: Option<String>,
}

/// What to put in place of the field codes.
pub struct Inputs<'a> {
    pub uri: &'a str,
    pub path: &'a Path,
    pub output: &'a Path,
    pub size: u32,
}

impl Thumbnailer {
    /// Parses the `[Thumbnailer Entry]` group of a `.thumbnailer` file.
    pub fn parse(contents: &str) -> Option<Thumbnailer> {
        let mut in_group = false;
        let (mut exec, mut mime_types, mut try_exec) = (None, Vec::new(), None);
        for line in contents.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                in_group = line == "[Thumbnailer Entry]";
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if !in_group {
                continue;
            }
            let value = unescape_value(value.trim());
            match key.trim() {
                "Exec" => exec = split_exec(&value),
                "TryExec" => try_exec = Some(value).filter(|value| !value.is_empty()),
                "MimeType" => {
                    mime_types = value
                        .split(';')
                        .map(str::trim)
                        .filter(|mime| !mime.is_empty())
                        .map(str::to_ascii_lowercase)
                        .collect()
                }
                _ => {}
            }
        }
        let exec = exec.filter(|exec: &Vec<String>| !exec.is_empty())?;
        (!mime_types.is_empty()).then_some(Thumbnailer {
            exec,
            mime_types,
            try_exec,
        })
    }

    /// How well it handles `mime`: 2 for an exact match, 1 for a `type/*` wildcard.
    fn score(&self, mime: &str) -> u8 {
        let mime = mime.to_ascii_lowercase();
        let major = mime.split('/').next().unwrap_or("");
        self.mime_types
            .iter()
            .map(|candidate| {
                if *candidate == mime {
                    2
                } else if candidate.strip_suffix("/*") == Some(major) {
                    1
                } else {
                    0
                }
            })
            .max()
            .unwrap_or(0)
    }

    /// The command line with field codes filled in. Unknown codes are dropped, as the desktop
    /// entry specification asks.
    pub fn command_line(&self, inputs: &Inputs) -> Vec<String> {
        self.exec
            .iter()
            .filter_map(|argument| {
                let mut out = String::new();
                let mut chars = argument.chars();
                let mut had_code = false;
                while let Some(c) = chars.next() {
                    if c != '%' {
                        out.push(c);
                        continue;
                    }
                    had_code = true;
                    match chars.next() {
                        Some('u') => out.push_str(inputs.uri),
                        Some('i') => out.push_str(&inputs.path.to_string_lossy()),
                        Some('o') => out.push_str(&inputs.output.to_string_lossy()),
                        Some('s') => out.push_str(&inputs.size.to_string()),
                        Some('%') => out.push('%'),
                        _ => {}
                    }
                }
                // An argument that was only an unknown code disappears entirely.
                (!(had_code && out.is_empty())).then_some(out)
            })
            .collect()
    }

    /// Whether its program (and `TryExec`, when given) can run here.
    fn installed(&self) -> bool {
        self.try_exec
            .as_deref()
            .is_none_or(|program| find_program(program).is_some())
            && find_program(&self.exec[0]).is_some()
    }

    /// Runs it, killing it after `timeout`. Its output goes nowhere; success means exit 0.
    pub fn run(&self, inputs: &Inputs, timeout: Duration) -> Result<(), String> {
        let argv = self.command_line(inputs);
        let (name, arguments) = argv.split_first().ok_or("empty Exec line")?;
        let program = find_program(name).ok_or_else(|| format!("{name} not found"))?;
        let mut child = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{name}: {error}"))?;
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => return Err(format!("{name} exited with {status}")),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{name} timed out"));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => return Err(error.to_string()),
            }
        }
    }
}

/// Every installed thumbnailer, in lookup order. A file in an earlier directory hides one
/// with the same name in a later directory.
pub fn discover() -> Vec<Thumbnailer> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for directory in data_dirs().into_iter().map(|dir| dir.join("thumbnailers")) {
        let Ok(items) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = items
            .filter_map(Result::ok)
            .map(|item| item.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "thumbnailer"))
            .collect();
        paths.sort();
        for path in paths {
            let Some(name) = path.file_name().map(|name| name.to_os_string()) else {
                continue;
            };
            if !seen.insert(name) {
                continue;
            }
            if let Some(thumbnailer) = std::fs::read_to_string(&path)
                .ok()
                .as_deref()
                .and_then(Thumbnailer::parse)
                .filter(Thumbnailer::installed)
            {
                found.push(thumbnailer);
            }
        }
    }
    found
}

/// The best thumbnailer for `mime`: exact matches beat wildcards, earlier beats later.
pub fn select<'a>(thumbnailers: &'a [Thumbnailer], mime: &str) -> Option<&'a Thumbnailer> {
    let mut best: Option<(&Thumbnailer, u8)> = None;
    for thumbnailer in thumbnailers {
        let score = thumbnailer.score(mime);
        if score > best.map_or(0, |(_, best)| best) {
            best = Some((thumbnailer, score));
        }
    }
    best.map(|(thumbnailer, _)| thumbnailer)
}

fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => dirs.push(PathBuf::from(dir)),
        None => dirs.extend(home.map(|home| home.join(".local/share"))),
    }
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(std::env::split_paths(&system).filter(|dir| dir.is_absolute()));
    dirs
}

/// An absolute program path, or a name looked up on `PATH`, that is an executable file.
fn find_program(program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |path: &Path| {
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let path = PathBuf::from(program);
        return (path.is_absolute() && executable(&path)).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| executable(path))
}

/// Desktop-entry string escapes: `\s`, `\n`, `\t`, `\r`, `\\`.
fn unescape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                // Not a value escape; leave it for the Exec quoting rules.
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Splits an `Exec` value into arguments: spaces separate them, double quotes group them, and
/// inside quotes a backslash escapes `"`, `` ` ``, `$` and `\`. `None` for an unclosed quote.
pub fn split_exec(exec: &str) -> Option<Vec<String>> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut in_argument = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_argument {
                    arguments.push(std::mem::take(&mut current));
                    in_argument = false;
                }
            }
            '"' => {
                in_argument = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            escaped @ ('"' | '`' | '$' | '\\') => current.push(escaped),
                            other => {
                                current.push('\\');
                                current.push(other);
                            }
                        },
                        other => current.push(other),
                    }
                }
            }
            other => {
                in_argument = true;
                current.push(other);
            }
        }
    }
    if in_argument {
        arguments.push(current);
    }
    Some(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GLYCIN: &str = "[Thumbnailer Entry]
TryExec=/usr/bin/glycin-thumbnailer
Exec=/usr/bin/glycin-thumbnailer --input %u --output %o --size %s
MimeType=image/png;image/JPEG;
";

    #[test]
    fn parses_thumbnailer_entries() {
        let thumbnailer = Thumbnailer::parse(GLYCIN).unwrap();
        assert_eq!(
            thumbnailer.exec,
            [
                "/usr/bin/glycin-thumbnailer",
                "--input",
                "%u",
                "--output",
                "%o",
                "--size",
                "%s"
            ]
        );
        assert_eq!(thumbnailer.mime_types, ["image/png", "image/jpeg"]);
        assert_eq!(
            thumbnailer.try_exec.as_deref(),
            Some("/usr/bin/glycin-thumbnailer")
        );
        // Keys outside the group, missing Exec or missing MimeType don't count.
        assert_eq!(
            Thumbnailer::parse("[Other]\nExec=x %u %o\nMimeType=a/b;\n"),
            None
        );
        assert_eq!(Thumbnailer::parse("[Thumbnailer Entry]\nExec=x\n"), None);
        assert_eq!(
            Thumbnailer::parse("[Thumbnailer Entry]\nMimeType=a/b;\n"),
            None
        );
    }

    #[test]
    fn splits_exec_lines_with_quotes_and_escapes() {
        assert_eq!(
            split_exec(r#"thumb  -s %s "%i" "a \"b\" \$c \\d" e\sf"#).unwrap(),
            ["thumb", "-s", "%s", "%i", r#"a "b" $c \d"#, r"e\sf"]
        );
        assert_eq!(split_exec(r#""" x"#).unwrap(), ["", "x"]);
        assert_eq!(split_exec(r#"x "unclosed"#), None);
        // Value escapes apply before Exec splitting: \s is a space inside one argument.
        let parsed =
            Thumbnailer::parse("[Thumbnailer Entry]\nExec=\"my\\sthumb\" %u\nMimeType=a/b\n")
                .unwrap();
        assert_eq!(parsed.exec, ["my thumb", "%u"]);
    }

    #[test]
    fn fills_in_field_codes_without_a_shell() {
        let thumbnailer = Thumbnailer {
            exec: split_exec("t -i %i -u %u -o %o -s %s 100%% %f %k --x=%s").unwrap(),
            mime_types: vec!["image/png".into()],
            try_exec: None,
        };
        let line = thumbnailer.command_line(&Inputs {
            uri: "file:///a%20b;rm",
            path: Path::new("/a b;rm"),
            output: Path::new("/tmp/out.png"),
            size: 256,
        });
        assert_eq!(
            line,
            [
                "t",
                "-i",
                "/a b;rm",
                "-u",
                "file:///a%20b;rm",
                "-o",
                "/tmp/out.png",
                "-s",
                "256",
                "100%",
                "--x=256"
            ]
        );
    }

    #[test]
    fn selects_exact_matches_before_wildcards_and_earlier_before_later() {
        let entry = |name: &str, mimes: &[&str]| Thumbnailer {
            exec: vec![name.into()],
            mime_types: mimes.iter().map(|mime| mime.to_string()).collect(),
            try_exec: None,
        };
        let all = [
            entry("videos", &["video/*"]),
            entry("first", &["image/png", "application/pdf"]),
            entry("second", &["image/png", "video/mp4"]),
        ];
        let pick = |mime| select(&all, mime).map(|t| t.exec[0].as_str());
        assert_eq!(pick("image/png"), Some("first"));
        assert_eq!(pick("IMAGE/PNG"), Some("first"));
        assert_eq!(pick("video/mp4"), Some("second"));
        assert_eq!(pick("video/webm"), Some("videos"));
        assert_eq!(pick("text/plain"), None);
    }
}
