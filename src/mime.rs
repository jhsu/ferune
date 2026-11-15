//! File types from the desktop's shared-mime-info database. Local files are typed through GIO
//! (`gio info`'s `standard::content-type`, which matches globs and, for extensionless or
//! ambiguous names, sniffs the contents), on one background worker that handles a folder's files
//! in batches; remote GVfs entries already carry GIO's content type from their listing.
//!
//! The shared-mime-info tables GIO itself doesn't expose on the command line (aliases, the
//! subclass hierarchy and generic icons) are read once on the worker, so "is this text?" follows
//! the database (`application/x-yaml` and `text/x-shellscript` are both kinds of `text/plain`).
//! When `gio` or the database is missing, everything falls back to [`from_extension`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};

/// Files per `gio info` call: enough to amortize a process start, few enough that leaving a
/// folder abandons the rest quickly.
const BATCH: usize = 128;
/// Files typed per folder; past this, the rest keep their extension-based guesses.
const MAX_FILES: usize = 20_000;

/// The type of a file from its extension alone: the fallback when GIO can't say, and the
/// immediate guess while it hasn't answered yet.
pub fn from_extension(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" | "jpe" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "svgz" => "image/svg+xml-compressed",
        "avif" => "image/avif",
        "heic" | "heif" => "image/heif",
        "jxl" => "image/jxl",
        "ico" => "image/vnd.microsoft.icon",
        "tga" => "image/x-tga",
        "qoi" => "image/qoi",
        "pdf" => "application/pdf",
        "epub" => "application/epub+zip",
        "djvu" | "djv" => "image/vnd.djvu",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mpeg" | "mpg" => "video/mpeg",
        "ogv" => "video/ogg",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/x-wav",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "opus" => "audio/x-opus+ogg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "md" | "markdown" | "mdx" => "text/markdown",
        "txt" | "text" | "log" => "text/plain",
        "rtf" => "application/rtf",
        "csv" => "text/csv",
        "rs" => "text/rust",
        "c" => "text/x-csrc",
        "h" => "text/x-chdr",
        "cpp" => "text/x-c++src",
        "hpp" => "text/x-c++hdr",
        "js" | "jsx" => "text/javascript",
        "ts" | "tsx" => "application/x-typescript",
        "py" => "text/x-python3",
        "go" => "text/x-go",
        "java" => "text/x-java",
        "kt" => "text/x-kotlin",
        "swift" => "text/x-swift",
        "sh" | "bash" => "application/x-shellscript",
        "html" => "text/html",
        "css" => "text/css",
        "sql" => "application/sql",
        "lua" => "text/x-lua",
        "json" => "application/json",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "xml" => "application/xml",
        "ini" | "conf" => "text/plain",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "zst" => "application/zstd",
        "deb" => "application/vnd.debian.binary-package",
        "rpm" => "application/x-rpm",
        _ => return None,
    })
}

/// "Don't know" answers that say nothing about a file's contents; the extension guess wins over
/// them. GLib reports `application/octet-stream` for everything when the database is missing.
pub fn is_unknown(mime: &str) -> bool {
    // `application/x-zerosize` is what an empty file with no recognized name sniffs as.
    mime.is_empty() || mime == "application/octet-stream" || mime == "application/x-zerosize"
}

/// A detected type when it's informative, otherwise the extension guess.
pub fn effective<'a>(detected: Option<&'a str>, path: &Path) -> Option<&'a str> {
    detected
        .filter(|mime| !is_unknown(mime))
        .or_else(|| from_extension(path))
}

/// The parts of shared-mime-info that GIO's command-line tool doesn't expose.
#[derive(Debug, Default)]
pub struct Database {
    aliases: HashMap<String, String>,
    parents: HashMap<String, Vec<String>>,
    generic_icons: HashMap<String, String>,
}

static DATABASE: OnceLock<Database> = OnceLock::new();

/// The database once the worker has loaded it; never blocks.
pub fn database() -> Option<&'static Database> {
    DATABASE.get()
}

/// `$XDG_DATA_HOME/mime` first, then each `$XDG_DATA_DIRS` entry's `mime`.
fn mime_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(std::env::split_paths(&system).filter(|dir| dir.is_absolute()))
        .map(|dir| dir.join("mime"))
        .collect()
}

impl Database {
    /// Reads the tables from every MIME directory; reads files, so call it off the UI thread.
    pub fn load() -> Database {
        let mut database = Database::default();
        for dir in mime_dirs() {
            let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
            database.add(
                &read("aliases"),
                &read("subclasses"),
                &read("generic-icons"),
            );
        }
        database
    }

    /// Adds one directory's tables. Directories come most important first, so an entry
    /// already present wins (parents accumulate).
    pub fn add(&mut self, aliases: &str, subclasses: &str, generic_icons: &str) {
        for (alias, canonical) in pairs(aliases, ' ') {
            self.aliases.entry(alias).or_insert(canonical);
        }
        for (child, parent) in pairs(subclasses, ' ') {
            let parents = self.parents.entry(child).or_default();
            if !parents.contains(&parent) {
                parents.push(parent);
            }
        }
        for (mime, icon) in pairs(generic_icons, ':') {
            self.generic_icons.entry(mime).or_insert(icon);
        }
    }

    pub fn canonical<'a>(&'a self, mime: &'a str) -> &'a str {
        self.aliases.get(mime).map_or(mime, String::as_str)
    }

    /// Whether `mime` is `ancestor` or a subclass of it, by the database's hierarchy plus the
    /// specification's implicit rules (every `text/*` is `text/plain`, and `*+xml` types are
    /// `application/xml`).
    pub fn is_a(&self, mime: &str, ancestor: &str) -> bool {
        let ancestor = self.canonical(ancestor);
        let mut seen = HashSet::new();
        let mut stack = vec![self.canonical(mime).to_owned()];
        while let Some(current) = stack.pop() {
            if current == ancestor {
                return true;
            }
            if !seen.insert(current.clone()) {
                continue;
            }
            if let Some(parents) = self.parents.get(&current) {
                stack.extend(
                    parents
                        .iter()
                        .map(|parent| self.canonical(parent).to_owned()),
                );
            }
            stack.extend(implicit_parent(&current).map(str::to_owned));
        }
        false
    }

    /// The `generic-icons` entry, like `package-x-generic` for archives.
    pub fn generic_icon(&self, mime: &str) -> Option<&str> {
        self.generic_icons
            .get(self.canonical(mime))
            .or_else(|| self.generic_icons.get(mime))
            .map(String::as_str)
    }
}

/// `key<separator>value` lines, skipping comments and blank lines.
fn pairs(text: &str, separator: char) -> impl Iterator<Item = (String, String)> + '_ {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(move |line| {
            let (key, value) = line.split_once(separator)?;
            let (key, value) = (key.trim(), value.trim());
            (!key.is_empty() && !value.is_empty()).then(|| (key.to_owned(), value.to_owned()))
        })
}

fn implicit_parent(mime: &str) -> Option<&'static str> {
    if mime.ends_with("+xml") && mime != "application/xml" {
        Some("application/xml")
    } else if mime.starts_with("text/") && mime != "text/plain" {
        Some("text/plain")
    } else {
        None
    }
}

/// Text-based types that the hierarchy would reveal, for when the database isn't loaded.
const KNOWN_TEXT: &[&str] = &[
    "application/json",
    "application/xml",
    "application/javascript",
    "application/x-shellscript",
    "application/toml",
    "application/yaml",
    "application/x-yaml",
    "application/sql",
    "application/x-desktop",
    "application/x-typescript",
];

/// Whether `mime` is `ancestor` or a kind of it.
pub fn is_a(mime: &str, ancestor: &str) -> bool {
    match database() {
        Some(database) => database.is_a(mime, ancestor),
        None => {
            let mut current = Some(mime);
            while let Some(mime) = current {
                if mime == ancestor {
                    return true;
                }
                current = implicit_parent(mime);
            }
            false
        }
    }
}

/// Whether a file of this type reads as text. Empty files count: their preview is just empty.
pub fn is_text(mime: &str) -> bool {
    is_a(mime, "text/plain") || KNOWN_TEXT.contains(&mime) || is_a(mime, "application/xml")
}

pub fn generic_icon(mime: &str) -> Option<&'static str> {
    database()?.generic_icon(mime)
}

/// The canonical name for an alias (`application/x-pdf` → `application/pdf`).
pub fn canonical(mime: &str) -> String {
    database()
        .map_or(mime, |database| database.canonical(mime))
        .to_owned()
}

/// One file's answer from the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Typed {
    pub path: PathBuf,
    /// The modification time GIO saw, so an answer about an older version is ignored.
    pub mtime: Option<u64>,
    pub content_type: String,
}

/// Why a batch has no answer.
#[derive(Debug)]
pub enum Failure {
    /// `gio` can't run at all (not installed); detection is switched off.
    Unavailable,
    /// This batch failed (timed out, say); its files keep their extension guesses.
    Batch,
}

pub struct Answer {
    generation: u64,
    /// The files asked about, answered or not.
    batch: Vec<PathBuf>,
    result: Result<Vec<Typed>, Failure>,
}

struct Job {
    generation: u64,
    files: Vec<PathBuf>,
}

#[derive(Default)]
struct Slot {
    job: Option<Job>,
    shutdown: bool,
}

struct Shared {
    slot: Mutex<Slot>,
    wake_worker: Condvar,
    /// The newest folder's generation; the worker abandons a job as soon as it's superseded.
    generation: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Where a file of the current folder stands with the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    /// Queued or in flight.
    Waiting,
    /// GIO had no answer (removed since, a broken link, a failed batch); not asked again
    /// until the file changes.
    NoAnswer,
}

/// The UI thread's side of MIME detection for the current local folder.
pub struct Detector {
    shared: Arc<Shared>,
    receiver: mpsc::Receiver<Answer>,
    generation: u64,
    folder: Option<PathBuf>,
    /// Detected types in the current folder: path → (modification time GIO saw, type).
    known: HashMap<PathBuf, (Option<u64>, String)>,
    /// Files asked about, at the modification time they were listed with.
    asked: HashMap<PathBuf, (u64, Asked)>,
    /// Set when `gio` can't run at all; detection stops and extensions are used.
    unavailable: bool,
}

impl Detector {
    /// Starts the worker thread; `wake` nudges the UI when an answer is queued.
    pub fn new(wake: fn()) -> Detector {
        Detector::start(wake, detect)
    }

    /// With a stand-in for `gio`, for tests.
    fn start(wake: fn(), detect: Detect) -> Detector {
        let (sender, receiver) = mpsc::channel();
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            wake_worker: Condvar::new(),
            generation: AtomicU64::new(0),
        });
        let worker = shared.clone();
        let _ = std::thread::Builder::new()
            .name("ferune-mime".into())
            .spawn(move || work(&worker, &sender, wake, detect));
        Detector {
            shared,
            receiver,
            generation: 0,
            folder: None,
            known: HashMap::new(),
            asked: HashMap::new(),
            unavailable: false,
        }
    }

    /// The type detected for `path` at modification time `mtime`, if any.
    pub fn get(&self, path: &Path, mtime: u64) -> Option<&str> {
        match self.known.get(path)? {
            (Some(seen), mime) if *seen == mtime => Some(mime),
            (None, mime) => Some(mime),
            _ => None,
        }
    }

    /// The files of the local folder `folder` now listed, as (path, modification time), in
    /// display order. Answers already known are kept; files not typed yet (or changed since)
    /// are queued for the worker, replacing whatever it was doing for a previous folder.
    pub fn show(&mut self, folder: &Path, files: &[(PathBuf, u64)]) {
        if self.folder.as_deref() != Some(folder) {
            self.folder = Some(folder.to_path_buf());
            self.known.clear();
            self.asked.clear();
            self.supersede();
        }
        if self.unavailable {
            return;
        }
        let files = &files[..files.len().min(MAX_FILES)];
        let listed: HashMap<&Path, u64> = files
            .iter()
            .map(|(path, mtime)| (path.as_path(), *mtime))
            .collect();
        self.asked
            .retain(|path, (mtime, _)| listed.get(path.as_path()) == Some(mtime));
        let mut new = false;
        for (path, mtime) in files {
            if self.get(path, *mtime).is_none() && !self.asked.contains_key(path) {
                self.asked.insert(path.clone(), (*mtime, Asked::Waiting));
                new = true;
            }
        }
        if !new {
            return;
        }
        // Everything still outstanding goes into one fresh job, so a re-read (sorting, a file
        // operation) neither duplicates nor drops work already queued.
        let job = files
            .iter()
            .filter(|(path, _)| {
                self.asked
                    .get(path)
                    .is_some_and(|(_, state)| *state == Asked::Waiting)
            })
            .map(|(path, _)| path.clone())
            .collect();
        self.supersede();
        lock(&self.shared.slot).job = Some(Job {
            generation: self.generation,
            files: job,
        });
        self.shared.wake_worker.notify_one();
    }

    fn supersede(&mut self) {
        self.generation += 1;
        self.shared
            .generation
            .store(self.generation, Ordering::SeqCst);
        lock(&self.shared.slot).job = None;
    }

    /// Takes finished answers. Returns the files whose type is newly known.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        while let Ok(answer) = self.receiver.try_recv() {
            if answer.generation != self.generation {
                continue;
            }
            let typed = match answer.result {
                Ok(typed) => typed,
                Err(Failure::Unavailable) => {
                    self.unavailable = true;
                    self.asked.clear();
                    continue;
                }
                Err(Failure::Batch) => Vec::new(),
            };
            for item in typed {
                if !answer.batch.contains(&item.path) {
                    continue;
                }
                self.asked.remove(&item.path);
                let value = (item.mtime, item.content_type);
                if self.known.get(&item.path) != Some(&value) {
                    self.known.insert(item.path.clone(), value);
                    changed.push(item.path);
                }
            }
            for path in &answer.batch {
                if let Some((_, state)) = self.asked.get_mut(path) {
                    *state = Asked::NoAnswer;
                }
            }
        }
        changed
    }
}

impl Drop for Detector {
    fn drop(&mut self) {
        lock(&self.shared.slot).shutdown = true;
        self.shared.wake_worker.notify_all();
    }
}

type Detect = fn(&[PathBuf]) -> Result<Vec<Typed>, Failure>;

fn work(shared: &Shared, sender: &mpsc::Sender<Answer>, wake: fn(), detect: Detect) {
    let _ = DATABASE.get_or_init(Database::load);
    loop {
        let job = {
            let mut slot = lock(&shared.slot);
            loop {
                if slot.shutdown {
                    return;
                }
                if let Some(job) = slot.job.take() {
                    break job;
                }
                slot = shared
                    .wake_worker
                    .wait(slot)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };
        for batch in job.files.chunks(BATCH) {
            if shared.generation.load(Ordering::SeqCst) != job.generation {
                break;
            }
            let result = detect(batch);
            let fatal = matches!(result, Err(Failure::Unavailable));
            if sender
                .send(Answer {
                    generation: job.generation,
                    batch: batch.to_vec(),
                    result,
                })
                .is_err()
            {
                return;
            }
            wake();
            if fatal {
                break;
            }
        }
    }
}

/// Types local files with one `gio info` call. Files GIO can't read (removed since, broken
/// links) are simply missing from the answer; an error means `gio` itself couldn't run.
pub fn detect(paths: &[PathBuf]) -> Result<Vec<Typed>, Failure> {
    let uris = paths
        .iter()
        .map(|path| crate::file_uri(path))
        .collect::<Vec<_>>();
    let records = crate::gio::content_types(&uris).map_err(|error| match error {
        crate::gio::RunError::Unavailable(_) => Failure::Unavailable,
        crate::gio::RunError::Failed(_) => Failure::Batch,
    })?;
    Ok(records
        .into_iter()
        .filter_map(|record| {
            Some(Typed {
                path: crate::local_file_uri(&record.uri)?,
                mtime: record.modified,
                content_type: record.content_type?,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests;
