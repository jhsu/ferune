//! GIO/GVfs access through the `gio` command-line tool that ships with GLib, so Ferune can
//! browse sftp://, smb://, mtp:// and other GVfs locations without linking GLib. Every call
//! here blocks on a subprocess (and possibly the network); run them off the UI thread.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::location::decode_lossy;

const LIST_TIMEOUT: Duration = Duration::from_secs(45);
const MOUNT_TIMEOUT: Duration = Duration::from_secs(60);
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const MOUNTS_TIMEOUT: Duration = Duration::from_secs(15);
/// Typing a batch of local files reads up to a few KiB of each.
const INFO_TIMEOUT: Duration = Duration::from_secs(20);

const LIST_ATTRIBUTES: [&str; 5] = [
    "standard::display-name",
    "standard::is-hidden",
    "standard::content-type",
    "standard::target-uri",
    "time::modified",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    pub uri: String,
    pub name: String,
    /// GIO's file type: `directory`, `regular`, `symbolic-link`, `shortcut`, `mountable`, …
    pub file_type: String,
    pub size: u64,
    /// Seconds since the Unix epoch, when the backend reports it.
    pub modified: Option<u64>,
    pub content_type: Option<String>,
    /// Where a shortcut or mountable (network:///, computer:///) leads.
    pub target_uri: Option<String>,
    pub hidden: bool,
}

impl RemoteEntry {
    /// Whether opening the entry should browse into it rather than launch an application.
    pub fn is_folder(&self) -> bool {
        self.file_type == "directory"
            || (matches!(self.file_type.as_str(), "shortcut" | "mountable")
                && self.target_uri.is_some())
    }
}

/// A mounted GVfs location (network share, phone, …) as `gio mount -l` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub name: String,
    pub uri: String,
}

/// Lists a folder, mounting its location first when GVfs says it isn't mounted yet. Mounting
/// is non-interactive: locations that need a password or a host-key confirmation fail with an
/// error that says so.
pub fn list_directory(uri: &str) -> Result<Vec<RemoteEntry>, String> {
    match list_once(uri) {
        Err(error) if error.contains("not mounted") => {
            mount(uri).map_err(|mount_error| {
                format!(
                    "{mount_error}. If it needs a password or host-key confirmation, run \
                     `gio mount {uri}` in a terminal once, then try again"
                )
            })?;
            list_once(uri)
        }
        result => result,
    }
}

fn list_once(uri: &str) -> Result<Vec<RemoteEntry>, String> {
    let attributes = LIST_ATTRIBUTES.join(",");
    let output = run_gio(
        &[
            "list",
            "--hidden",
            "--long",
            "--print-uris",
            "--attributes",
            &attributes,
            "--",
            uri,
        ],
        LIST_TIMEOUT,
    )?;
    Ok(parse_list(&output))
}

pub fn mount(uri: &str) -> Result<(), String> {
    run_gio(&["mount", "--", uri], MOUNT_TIMEOUT).map(|_| ())
}

/// Opens a file in its default application. GIO-aware applications get the URI; others get
/// a local path when GVfs's FUSE bridge is running.
pub fn open(uri: &str) -> Result<(), String> {
    run_gio(&["open", "--", uri], OPEN_TIMEOUT).map(|_| ())
}

/// Mounted remote locations; local disks (`file://` mounts) are left out. Empty when `gio`
/// or the session bus isn't available.
pub fn list_mounts() -> Vec<Mount> {
    run_gio(&["mount", "--list", "--detail"], MOUNTS_TIMEOUT)
        .map(|output| parse_mounts(&output))
        .unwrap_or_default()
}

/// One file's type as `gio info` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoRecord {
    pub uri: String,
    /// The shared-mime-info type: globs first, the file's contents when the name is ambiguous
    /// or has no known extension.
    pub content_type: Option<String>,
    /// Seconds since the Unix epoch.
    pub modified: Option<u64>,
}

/// The content type of each file, with one `gio info` process for all of them. Files `gio`
/// can't read are left out rather than failing the rest.
pub fn content_types(uris: &[String]) -> Result<Vec<InfoRecord>, RunError> {
    if uris.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![
        "info",
        "--attributes",
        "standard::content-type,time::modified",
        "--",
    ];
    args.extend(uris.iter().map(String::as_str));
    Ok(parse_info(&run_gio_raw(&args, INFO_TIMEOUT)?.stdout))
}

/// Parses `gio info --attributes …` output for one or more files: each file starts with a
/// `uri: …` line, and its attributes follow indented as `  key: value`. File names can't break
/// records: the URI is percent-encoded and `gio` escapes control characters elsewhere.
pub fn parse_info(output: &str) -> Vec<InfoRecord> {
    let mut records: Vec<InfoRecord> = Vec::new();
    for line in output.lines() {
        if let Some(uri) = line.strip_prefix("uri: ") {
            records.push(InfoRecord {
                uri: uri.trim_end().to_owned(),
                content_type: None,
                modified: None,
            });
            continue;
        }
        let Some(record) = records.last_mut() else {
            continue;
        };
        let Some(attribute) = line.strip_prefix("  ") else {
            continue;
        };
        if let Some(value) = attribute.strip_prefix("standard::content-type: ") {
            let value = value.trim();
            record.content_type = (!value.is_empty()).then(|| value.to_owned());
        } else if let Some(value) = attribute.strip_prefix("time::modified: ") {
            record.modified = value.trim().parse().ok();
        }
    }
    records
}

/// Parses `gio list --long --print-uris --attributes …` output: one
/// `URI<TAB>SIZE<TAB>(TYPE)<TAB>key=value key=value…` record per file. A display name that
/// contains a newline spills onto the next line, so lines that don't start a record continue
/// the previous one.
pub fn parse_list(output: &str) -> Vec<RemoteEntry> {
    let mut records: Vec<String> = Vec::new();
    for line in output.strip_suffix('\n').unwrap_or(output).split('\n') {
        if is_record_start(line) {
            records.push(line.to_owned());
        } else if let Some(record) = records.last_mut() {
            record.push('\n');
            record.push_str(line);
        }
    }
    records
        .iter()
        .filter_map(|record| parse_record(record))
        .collect()
}

fn is_record_start(line: &str) -> bool {
    let mut fields = line.splitn(4, '\t');
    let (Some(uri), Some(size), Some(kind)) = (fields.next(), fields.next(), fields.next()) else {
        return false;
    };
    crate::location::uri_scheme(uri).is_some()
        && !uri.contains(' ')
        && size.parse::<u64>().is_ok()
        && kind.starts_with('(')
        && kind.ends_with(')')
}

fn parse_record(record: &str) -> Option<RemoteEntry> {
    let mut fields = record.splitn(4, '\t');
    let uri = fields.next()?.to_owned();
    let size = fields.next()?.parse().ok()?;
    let file_type = fields
        .next()?
        .trim_start_matches('(')
        .trim_end_matches(')')
        .to_owned();
    let attributes = parse_attributes(fields.next().unwrap_or(""));
    let name = attributes
        .get("standard::display-name")
        .filter(|name| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| {
            let segment = uri.trim_end_matches('/').rsplit('/').next().unwrap_or(&uri);
            decode_lossy(segment)
        });
    Some(RemoteEntry {
        hidden: name.starts_with('.')
            || attributes.get("standard::is-hidden").map(String::as_str) == Some("TRUE"),
        modified: attributes
            .get("time::modified")
            .and_then(|value| value.parse().ok()),
        content_type: attributes.get("standard::content-type").cloned(),
        target_uri: attributes.get("standard::target-uri").cloned(),
        uri,
        name,
        file_type,
        size,
    })
}

/// Splits `key=value key=value…` where values may contain spaces, by locating the attribute
/// names that were asked for.
fn parse_attributes(text: &str) -> HashMap<&'static str, String> {
    let mut starts = LIST_ATTRIBUTES
        .iter()
        .filter_map(|key| {
            let needle = format!("{key}=");
            let at = if text.starts_with(&needle) {
                Some(0)
            } else {
                text.find(&format!(" {needle}")).map(|index| index + 1)
            }?;
            Some((at, *key))
        })
        .collect::<Vec<_>>();
    starts.sort();
    starts
        .iter()
        .enumerate()
        .map(|(index, (at, key))| {
            let value_start = at + key.len() + 1;
            let value_end = starts
                .get(index + 1)
                .map_or(text.len(), |(next, _)| next - 1);
            (
                *key,
                text[value_start..value_end.max(value_start)].to_owned(),
            )
        })
        .collect()
}

/// Parses `gio mount --list --detail`, keeping mounts that aren't plain local disks and that
/// another mount doesn't shadow.
pub fn parse_mounts(output: &str) -> Vec<Mount> {
    let mut mounts: Vec<(Mount, bool)> = Vec::new();
    for line in output.lines().map(str::trim) {
        if line.starts_with("Mount(") {
            let Some((_, description)) = line.split_once("): ") else {
                continue;
            };
            let Some((name, uri)) = description.rsplit_once(" -> ") else {
                continue;
            };
            mounts.push((
                Mount {
                    name: name.to_owned(),
                    uri: uri.to_owned(),
                },
                false,
            ));
        } else if line == "is_shadowed=1"
            && let Some((_, shadowed)) = mounts.last_mut()
        {
            *shadowed = true;
        }
    }
    let mut seen = Vec::new();
    mounts
        .into_iter()
        .filter(|(mount, shadowed)| {
            !shadowed
                && !mount.uri.starts_with("file://")
                && crate::location::uri_scheme(&mount.uri).is_some()
        })
        .map(|(mount, _)| mount)
        .filter(|mount| {
            let new = !seen.contains(&mount.uri);
            seen.push(mount.uri.clone());
            new
        })
        .collect()
}

/// Runs `gio` with a deadline and returns its stdout, or its error message. Messages are kept
/// in English (only `LC_MESSAGES` changes, so file names keep the user's character set) because
/// `list_directory` recognizes the "not mounted" error by its text.
fn run_gio(args: &[&str], timeout: Duration) -> Result<String, String> {
    let output = run_gio_raw(args, timeout).map_err(|error| match error {
        RunError::Unavailable(message) | RunError::Failed(message) => message,
    })?;
    if output.success {
        Ok(output.stdout)
    } else {
        Err(clean_error(&output.stderr))
    }
}

/// Why `gio` produced no usable output.
#[derive(Debug, PartialEq, Eq)]
pub enum RunError {
    /// The tool isn't installed or can't be started.
    Unavailable(String),
    /// It ran but timed out, or couldn't be waited for.
    Failed(String),
}

struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Runs `gio` directly (no shell) with a deadline, returning whatever it printed whether or
/// not it succeeded: `gio info` with several files prints every file it could read and exits
/// non-zero when any one of them failed.
fn run_gio_raw(args: &[&str], timeout: Duration) -> Result<Output, RunError> {
    let mut command = Command::new("gio");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(all) = std::env::var_os("LC_ALL") {
        command.env_remove("LC_ALL").env("LC_CTYPE", all);
    }
    command.env("LC_MESSAGES", "C");
    let mut child = command.spawn().map_err(|error| {
        RunError::Unavailable(if error.kind() == std::io::ErrorKind::NotFound {
            "The `gio` tool isn't installed (it ships with GLib; GVfs provides network and \
             phone locations)"
                .to_owned()
        } else {
            format!("Couldn't run gio: {error}")
        })
    })?;
    // Readers run on their own threads so a large listing can't fill the pipe and stall gio,
    // and so an application `gio open` launched (which inherits the pipes) can't block us.
    let read = |mut pipe: Box<dyn Read + Send>| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = sender.send(bytes);
        });
        receiver
    };
    let stdout = child.stdout.take().map(|pipe| read(Box::new(pipe)));
    let stderr = child.stderr.take().map(|pipe| read(Box::new(pipe)));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(15)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Failed(format!(
                    "Timed out after {} seconds waiting for the location",
                    timeout.as_secs()
                )));
            }
            Err(error) => return Err(RunError::Failed(format!("Couldn't wait for gio: {error}"))),
        }
    };
    let collect = |receiver: Option<mpsc::Receiver<Vec<u8>>>| {
        receiver
            .and_then(|receiver| receiver.recv_timeout(Duration::from_secs(2)).ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    };
    Ok(Output {
        success: status.success(),
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

/// `gio: sftp://host/: Hostname not known` → `Hostname not known`.
fn clean_error(stderr: &str) -> String {
    let message = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("");
    let message = message.strip_prefix("gio: ").unwrap_or(message);
    let message = match message.split_once(": ") {
        Some((uri, rest)) if uri.contains("://") && !uri.contains(' ') => rest,
        _ => message,
    };
    if message.is_empty() {
        "gio failed without an error message".into()
    } else {
        message.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_long_uri_listings_with_spaces_and_missing_attributes() {
        let output = "sftp://h/srv/a%20b\t4096\t(directory)\tstandard::display-name=a b standard::content-type=inode/directory time::modified=1700000000\n\
sftp://h/srv/na%C3%AFve%20%231.txt\t3\t(regular)\tstandard::content-type=text/plain standard::display-name=naïve #1.txt\n\
sftp://h/srv/.profile\t12\t(regular)\n\
network:///smb-root\t0\t(mountable)\tstandard::display-name=Windows Network standard::target-uri=smb:///\n";
        let entries = parse_list(output);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name, "a b");
        assert!(entries[0].is_folder());
        assert_eq!(entries[0].modified, Some(1_700_000_000));
        assert_eq!(entries[0].content_type.as_deref(), Some("inode/directory"));
        assert_eq!(entries[1].name, "naïve #1.txt");
        assert_eq!(entries[1].size, 3);
        assert!(!entries[1].is_folder());
        assert_eq!(entries[1].modified, None);
        // No display name: the decoded last URI segment stands in, and dotfiles are hidden.
        assert_eq!(entries[2].name, ".profile");
        assert!(entries[2].hidden);
        assert_eq!(entries[3].name, "Windows Network");
        assert_eq!(entries[3].target_uri.as_deref(), Some("smb:///"));
        assert!(entries[3].is_folder());
    }

    #[test]
    fn keeps_newlines_inside_display_names() {
        let output = "smb://h/s/odd%0Aname\t1\t(regular)\tstandard::display-name=odd\nname\n\
smb://h/s/next\t2\t(regular)\tstandard::display-name=next\n";
        let names = parse_list(output)
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["odd\nname", "next"]);
    }

    #[test]
    fn lists_only_unshadowed_remote_mounts() {
        let output = "Drive(0): Disk\n  Type: GProxyDrive (GProxyVolumeMonitorUDisks2)\n  Volume(0): Data\n    Mount(0): Data -> file:///run/media/me/Data\n      is_shadowed=0\n\
Mount(1): me on host -> sftp://me@host/\n  Type: GDaemonMount\n  is_shadowed=0\n\
Mount(2): Pixel 8 -> mtp://Google_Pixel_8_123/\n  is_shadowed=0\n\
Mount(3): hidden -> smb://nas/old/\n  is_shadowed=1\n";
        assert_eq!(
            parse_mounts(output),
            vec![
                Mount {
                    name: "me on host".into(),
                    uri: "sftp://me@host/".into()
                },
                Mount {
                    name: "Pixel 8".into(),
                    uri: "mtp://Google_Pixel_8_123/".into()
                },
            ]
        );
    }

    #[test]
    fn trims_gio_error_prefixes() {
        assert_eq!(
            clean_error("gio: sftp://nohost.invalid/: Hostname not known\n"),
            "Hostname not known"
        );
        assert_eq!(
            clean_error("gio: file:///x: Error opening directory '/x': No such file or directory"),
            "Error opening directory '/x': No such file or directory"
        );
        assert_eq!(clean_error(""), "gio failed without an error message");
    }

    #[test]
    fn parses_info_for_several_files() {
        let output = "uri: file:///tmp/m/noext\n\
local path: /tmp/m/noext\n\
unix mount: tmpfs /tmp tmpfs rw\n\
attributes:\n  standard::content-type: image/png\n  time::modified: 1794730665\n\
uri: file:///tmp/m/odd%0Aname\n\
local path: /tmp/m/odd ↵ name\n\
attributes:\n  time::modified: 1794730666\n  standard::content-type: text/plain\n\
uri: file:///tmp/m/unreadable\n\
attributes:\n";
        assert_eq!(
            parse_info(output),
            vec![
                InfoRecord {
                    uri: "file:///tmp/m/noext".into(),
                    content_type: Some("image/png".into()),
                    modified: Some(1_794_730_665),
                },
                InfoRecord {
                    uri: "file:///tmp/m/odd%0Aname".into(),
                    content_type: Some("text/plain".into()),
                    modified: Some(1_794_730_666),
                },
                InfoRecord {
                    uri: "file:///tmp/m/unreadable".into(),
                    content_type: None,
                    modified: None,
                },
            ]
        );
        assert_eq!(parse_info(""), Vec::new());
    }

    /// `gio info` exits non-zero when one file of several is missing; the rest still count.
    #[test]
    fn types_real_files_through_gio_despite_a_missing_one() {
        if !gio_available() {
            eprintln!("skipping: gio isn't installed");
            return;
        }
        let dir = temp_dir("gio-info");
        let uris = [
            crate::file_uri(&dir.join("naïve #1.txt")),
            crate::file_uri(&dir.join("missing")),
            crate::file_uri(&dir.join("sub folder")),
        ];
        let records = content_types(&uris).unwrap();
        assert_eq!(records.len(), 2, "{records:?}");
        assert_eq!(records[0].uri, uris[0]);
        assert_eq!(records[0].content_type.as_deref(), Some("text/plain"));
        assert!(records[0].modified.is_some());
        assert_eq!(records[1].content_type.as_deref(), Some("inode/directory"));
        assert_eq!(content_types(&[]), Ok(Vec::new()));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn gio_available() -> bool {
        Command::new("gio")
            .arg("version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ferune-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub folder")).unwrap();
        std::fs::write(dir.join("naïve #1.txt"), "hi").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        dir
    }

    /// End to end through the real `gio` tool, using a file:// URI so it needs no network.
    #[test]
    fn lists_a_real_folder_through_gio() {
        if !gio_available() {
            eprintln!("skipping: gio isn't installed");
            return;
        }
        let dir = temp_dir("gio-list");
        let mut entries = list_directory(&crate::file_uri(&dir)).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let summary = entries
            .iter()
            .map(|entry| {
                (
                    entry.name.as_str(),
                    entry.is_folder(),
                    entry.hidden,
                    entry.size,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(summary[0], (".hidden", false, true, 0));
        assert_eq!(summary[1], ("naïve #1.txt", false, false, 2));
        assert_eq!((summary[2].0, summary[2].1), ("sub folder", true));
        assert!(entries.iter().all(|entry| entry.modified.is_some()));
        let missing = list_directory(&crate::file_uri(&dir.join("missing"))).unwrap_err();
        assert!(missing.contains("No such file"), "{missing}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Mounts GVfs's `localtest` backend (a non-file scheme served by a GVfs daemon) through
    /// the "not mounted" path. It changes session mount state, so it only runs on request:
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn mounts_and_lists_a_gvfs_backend_on_demand() {
        let dir = temp_dir("gio-localtest");
        let _ = run_gio(&["mount", "--unmount", "localtest:///"], MOUNT_TIMEOUT);
        let uri = format!(
            "localtest://{}",
            crate::file_uri(&dir).trim_start_matches("file://")
        );
        let entries = list_directory(&uri).unwrap();
        assert!(
            entries
                .iter()
                .any(|entry| entry.name == "sub folder" && entry.is_folder())
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry.uri.starts_with("localtest://"))
        );
        assert!(
            list_mounts()
                .iter()
                .any(|mount| mount.uri == "localtest:///")
        );
        let _ = run_gio(&["mount", "--unmount", "localtest:///"], MOUNT_TIMEOUT);
        let _ = std::fs::remove_dir_all(dir);
    }
}
