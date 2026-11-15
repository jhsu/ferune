//! Freedesktop thumbnails (the XDG Thumbnail Managing Standard): look up
//! `$XDG_CACHE_HOME/thumbnails/<size>/<md5 of URI>.png`, trust it only while its `Thumb::URI`
//! and `Thumb::MTime` match the file, and otherwise generate one with the system's
//! `*.thumbnailer` programs. Failures are recorded under `fail/ferune-<version>/` so a file
//! that can't be thumbnailed isn't retried until it changes.
//!
//! Everything that touches the disk or runs a program happens on [`Service`]'s worker threads;
//! the UI thread only hands over a folder's files and drains finished thumbnails in
//! [`Thumbs::poll`]. Only local files are thumbnailed: remote GVfs files keep their icons.

mod md5;
mod png;
mod thumbnailer;

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, UNIX_EPOCH};

use thumbnailer::Thumbnailer;

const WORKERS: usize = 2;
const THUMBNAILER_TIMEOUT: Duration = Duration::from_secs(30);
/// Thumbnails are small; anything bigger than this in the cache is ignored.
const MAX_THUMBNAIL_BYTES: u64 = 16 << 20;
/// Decoded pixels the UI keeps for one folder, counting mips.
const DECODED_BUDGET: u64 = 192 << 20;
const MAX_LOADED: usize = 4000;

/// A thumbnail size directory and its largest edge in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Normal,
    Large,
    XLarge,
    XxLarge,
}

impl Flavor {
    const ALL: [Flavor; 4] = [
        Flavor::Normal,
        Flavor::Large,
        Flavor::XLarge,
        Flavor::XxLarge,
    ];

    pub fn pixels(self) -> u32 {
        match self {
            Flavor::Normal => 128,
            Flavor::Large => 256,
            Flavor::XLarge => 512,
            Flavor::XxLarge => 1024,
        }
    }

    fn dir_name(self) -> &'static str {
        match self {
            Flavor::Normal => "normal",
            Flavor::Large => "large",
            Flavor::XLarge => "x-large",
            Flavor::XxLarge => "xx-large",
        }
    }

    /// The smallest size that covers `pixels`.
    pub fn for_pixels(pixels: u32) -> Flavor {
        Flavor::ALL
            .into_iter()
            .find(|flavor| flavor.pixels() >= pixels)
            .unwrap_or(Flavor::XxLarge)
    }

    /// Where to look, best first: this size, then larger ones, which scale down cleanly.
    fn lookup_order(self) -> impl Iterator<Item = Flavor> {
        Flavor::ALL
            .into_iter()
            .filter(move |flavor| flavor.pixels() >= self.pixels())
    }
}

/// `$XDG_CACHE_HOME/thumbnails`, or `~/.cache/thumbnails`.
pub fn cache_root() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|cache| cache.join("thumbnails"))
}

/// The file's URI as the specification hashes it: GLib's `g_filename_to_uri`, which keeps
/// `!$&'()*+,-./:=@_~` and ASCII letters and digits, and percent-encodes every other byte with
/// uppercase hex. Other thumbnail readers and writers only agree on cache names if this
/// matches byte for byte.
pub fn thumbnail_uri(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"!$&'()*+,-./:=@_~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Some(uri)
}

/// `<md5 of the URI>.png`.
pub fn cache_name(uri: &str) -> String {
    format!("{}.png", md5::hex_digest(uri.as_bytes()))
}

/// Whether a cached thumbnail's metadata still describes the file: `Thumb::URI` and
/// `Thumb::MTime` must match, and `Thumb::Size` too when the thumbnail records one.
pub fn is_fresh(text: &[(String, String)], uri: &str, mtime: u64, size: u64) -> bool {
    let value = |key: &str| {
        text.iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.trim())
    };
    // Some writers store fractional seconds; the specification compares whole seconds.
    let thumb_mtime = value("Thumb::MTime")
        .and_then(|value| value.split('.').next())
        .and_then(|seconds| seconds.parse::<u64>().ok());
    value("Thumb::URI") == Some(uri)
        && thumb_mtime == Some(mtime)
        && value("Thumb::Size").is_none_or(|value| value.parse::<u64>().ok() == Some(size))
}

/// The MIME type to pick a thumbnailer by: the type GIO detected from shared-mime-info when it
/// has one, otherwise a guess from the extension.
pub fn mime_for(detected: Option<&str>, path: &Path) -> Option<String> {
    crate::mime::effective(detected, path).map(crate::mime::canonical)
}

/// Whether a file of this type could have a thumbnail. Plain text and other non-visual types
/// are skipped up front, so a folder of source code doesn't queue a cache lookup per file;
/// what's left is decided by the installed thumbnailers on the workers.
fn worth_trying(mime: &str) -> bool {
    let major = mime.split('/').next().unwrap_or("");
    match major {
        "image" | "video" | "audio" | "model" | "font" | "application" => {
            !crate::mime::is_text(mime) || mime == "image/svg+xml"
        }
        _ => false,
    }
}

/// Files that must not be thumbnailed: the thumbnail cache itself (a thumbnail of a
/// thumbnail), and GVfs's FUSE bridge, where reading a "local" path goes over the network.
fn excluded(path: &Path, cache_root: &Path) -> bool {
    path.starts_with(cache_root)
        || path
            .components()
            .collect::<Vec<_>>()
            .windows(4)
            .any(|parts| {
                parts[0].as_os_str() == "run"
                    && parts[1].as_os_str() == "user"
                    && parts[3].as_os_str() == "gvfs"
            })
        || std::env::var_os("XDG_RUNTIME_DIR")
            .is_some_and(|runtime| path.starts_with(Path::new(&runtime).join("gvfs")))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub path: PathBuf,
    /// The modification time the UI listed; echoed back so a stale answer is ignored.
    pub mtime: u64,
    /// The type GIO detected, when it has answered; the extension is used otherwise.
    pub mime: Option<String>,
}

pub enum Outcome {
    /// A fresh thumbnail's PNG bytes and size.
    Ready(Arc<[u8]>, (u32, u32)),
    /// No thumbnail: unsupported type, no thumbnailer installed, or it failed.
    Unavailable,
}

pub struct Done {
    pub path: PathBuf,
    pub mtime: u64,
    pub mime: Option<String>,
    pub outcome: Outcome,
}

#[derive(Clone)]
struct Job {
    request: Request,
    flavor: Flavor,
    epoch: u64,
}

#[derive(Default)]
struct Queue {
    /// Cache lookups go first so cached thumbnails show before any slow generation.
    lookups: VecDeque<Job>,
    generate: VecDeque<Job>,
    shutdown: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    wake_workers: Condvar,
    /// Bumped per folder, so a worker drops jobs from a folder the user has left.
    epoch: AtomicU64,
}

/// Background lookup and generation. Dropping it stops the workers after their current job.
pub struct Service {
    shared: Arc<Shared>,
}

impl Service {
    pub fn start(sender: mpsc::Sender<Done>, wake: fn()) -> Service {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake_workers: Condvar::new(),
            epoch: AtomicU64::new(0),
        });
        for index in 0..WORKERS {
            let shared = shared.clone();
            let sender = sender.clone();
            let _ = std::thread::Builder::new()
                .name(format!("ferune-thumbnails-{index}"))
                .spawn(move || work(&shared, &sender, wake, index));
        }
        Service { shared }
    }

    /// Replaces every queued job with `requests`.
    pub fn replace(&self, requests: Vec<Request>, flavor: Flavor) {
        let epoch = self.shared.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let mut queue = lock(&self.shared.queue);
        queue.generate.clear();
        queue.lookups = requests
            .into_iter()
            .map(|request| Job {
                request,
                flavor,
                epoch,
            })
            .collect();
        drop(queue);
        self.shared.wake_workers.notify_all();
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        lock(&self.shared.queue).shutdown = true;
        self.shared.wake_workers.notify_all();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn work(shared: &Shared, sender: &mpsc::Sender<Done>, wake: fn(), index: usize) {
    // Reads the thumbnailer folders here, on the worker, not on the UI thread.
    let cache = Cache::from_env(index);
    loop {
        let (job, generate) = {
            let mut queue = lock(&shared.queue);
            loop {
                if queue.shutdown {
                    return;
                }
                if let Some(job) = queue.lookups.pop_front() {
                    break (job, false);
                }
                if let Some(job) = queue.generate.pop_front() {
                    break (job, true);
                }
                queue = shared
                    .wake_workers
                    .wait(queue)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };
        if job.epoch != shared.epoch.load(Ordering::SeqCst) {
            continue;
        }
        let outcome = match (&cache, generate) {
            (None, _) => Some(Outcome::Unavailable),
            (Some(cache), false) => {
                match cache.lookup(&job.request.path, job.request.mime.as_deref(), job.flavor) {
                    Lookup::Found(outcome) => Some(outcome),
                    Lookup::Generate => {
                        let mut queue = lock(&shared.queue);
                        if job.epoch == shared.epoch.load(Ordering::SeqCst) {
                            queue.generate.push_back(job.clone());
                        }
                        None
                    }
                }
            }
            (Some(cache), true) => {
                Some(cache.generate(&job.request.path, job.request.mime.as_deref(), job.flavor))
            }
        };
        if let Some(outcome) = outcome {
            let done = Done {
                path: job.request.path,
                mtime: job.request.mtime,
                mime: job.request.mime,
                outcome,
            };
            if sender.send(done).is_err() {
                return;
            }
            wake();
        }
    }
}

enum Lookup {
    Found(Outcome),
    Generate,
}

/// The on-disk cache, as seen from a worker thread.
struct Cache {
    root: PathBuf,
    /// Ferune's own failure folder.
    fail_dir: PathBuf,
    /// Where thumbnailers write before Ferune adds the metadata and moves the file in.
    scratch: PathBuf,
    thumbnailers: Vec<Thumbnailer>,
}

/// The file facts a thumbnail is checked against.
struct Source {
    uri: String,
    name: String,
    mtime: u64,
    size: u64,
    mime: String,
}

impl Cache {
    fn from_env(worker: usize) -> Option<Cache> {
        let root = cache_root()?;
        let scratch = root
            .parent()?
            .join("ferune")
            .join(format!("thumbnail-scratch-{}-{worker}", std::process::id()));
        if worker == 0 {
            remove_stale_scratch(scratch.parent()?);
        }
        Some(Cache::new(root, scratch, thumbnailer::discover()))
    }

    fn new(root: PathBuf, scratch: PathBuf, thumbnailers: Vec<Thumbnailer>) -> Cache {
        Cache {
            fail_dir: root
                .join("fail")
                .join(concat!("ferune-", env!("CARGO_PKG_VERSION"))),
            root,
            scratch,
            thumbnailers,
        }
    }

    fn thumbnailer_for(&self, mime: &str) -> Option<&Thumbnailer> {
        thumbnailer::select(&self.thumbnailers, mime)
    }

    fn source(&self, path: &Path, detected: Option<&str>) -> Option<Source> {
        if excluded(path, &self.root) {
            return None;
        }
        let mime = mime_for(detected, path)?;
        let metadata = fs::metadata(path).ok().filter(fs::Metadata::is_file)?;
        let mtime = metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs();
        let uri = thumbnail_uri(path)?;
        Some(Source {
            name: cache_name(&uri),
            uri,
            mtime,
            size: metadata.len(),
            mime,
        })
    }

    fn lookup(&self, path: &Path, detected: Option<&str>, flavor: Flavor) -> Lookup {
        let Some(source) = self.source(path, detected) else {
            return Lookup::Found(Outcome::Unavailable);
        };
        for flavor in flavor.lookup_order() {
            let candidate = self.root.join(flavor.dir_name()).join(&source.name);
            if let Some(found) = read_fresh(&candidate, &source) {
                return Lookup::Found(found);
            }
        }
        if read_fresh(&self.fail_dir.join(&source.name), &source).is_some()
            || self.thumbnailer_for(&source.mime).is_none()
        {
            return Lookup::Found(Outcome::Unavailable);
        }
        Lookup::Generate
    }

    fn generate(&self, path: &Path, detected: Option<&str>, flavor: Flavor) -> Outcome {
        let Some(source) = self.source(path, detected) else {
            return Outcome::Unavailable;
        };
        match self.run_thumbnailer(path, &source, flavor) {
            Ok(outcome) => outcome,
            Err(_) => {
                let marker = png::marker(&as_pairs(&metadata(&source)));
                let _ = write_atomically(&self.fail_dir, &source.name, &marker);
                Outcome::Unavailable
            }
        }
    }

    fn run_thumbnailer(
        &self,
        path: &Path,
        source: &Source,
        flavor: Flavor,
    ) -> Result<Outcome, String> {
        let thumbnailer = self.thumbnailer_for(&source.mime).ok_or("no thumbnailer")?;
        create_private_dir(&self.scratch).map_err(|error| error.to_string())?;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let output = self.scratch.join(format!(
            "{}-{}.png",
            source.name.trim_end_matches(".png"),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = thumbnailer.run(
            &thumbnailer::Inputs {
                uri: &source.uri,
                path,
                output: &output,
                size: flavor.pixels(),
            },
            THUMBNAILER_TIMEOUT,
        );
        let bytes = read_limited(&output);
        let _ = fs::remove_file(&output);
        // Workers are never joined, so nothing would clean up a scratch folder at exit; drop it
        // after every run instead (this only succeeds while it's empty).
        let _ = fs::remove_dir(&self.scratch);
        result?;
        let bytes = bytes.ok_or("no output")?;
        let dimensions = png::dimensions(&bytes).ok_or("output isn't a PNG")?;
        let tagged =
            png::with_text(&bytes, &as_pairs(&metadata(source))).ok_or("output isn't a PNG")?;
        write_atomically(&self.root.join(flavor.dir_name()), &source.name, &tagged)
            .map_err(|error| error.to_string())?;
        Ok(Outcome::Ready(tagged.into(), dimensions))
    }
}

/// The specification's required and recommended keys for a thumbnail of `source`.
fn metadata(source: &Source) -> Vec<(&'static str, String)> {
    vec![
        ("Thumb::URI", source.uri.clone()),
        ("Thumb::MTime", source.mtime.to_string()),
        ("Thumb::Size", source.size.to_string()),
        ("Thumb::Mimetype", source.mime.clone()),
        (
            "Software",
            concat!("Ferune ", env!("CARGO_PKG_VERSION")).to_owned(),
        ),
    ]
}

fn as_pairs<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    pairs
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect()
}

fn read_limited(path: &Path) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_THUMBNAIL_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_THUMBNAIL_BYTES).then_some(bytes)
}

fn read_fresh(path: &Path, source: &Source) -> Option<Outcome> {
    let bytes = read_limited(path)?;
    let text = png::text(&bytes)?;
    if !is_fresh(&text, &source.uri, source.mtime, source.size) {
        return None;
    }
    let dimensions = png::dimensions(&bytes)?;
    Some(Outcome::Ready(bytes.into(), dimensions))
}

/// Scratch folders left by Ferune processes that were killed mid-thumbnail.
fn remove_stale_scratch(dir: &Path) {
    let Ok(items) = fs::read_dir(dir) else {
        return;
    };
    for item in items.filter_map(Result::ok) {
        let name = item.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix("thumbnail-scratch-"))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !Path::new("/proc").join(pid.to_string()).exists() {
            let _ = fs::remove_dir_all(item.path());
        }
    }
}

/// Folders the specification asks to keep private to the user.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Writes `name` in `dir` through a temporary file and a rename, so readers never see a
/// partial thumbnail. Files are readable by the user only.
fn write_atomically(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    create_private_dir(dir)?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let temp = dir.join(format!(
        ".{name}.ferune-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&temp, dir.join(name)));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

enum Slot {
    Pending,
    /// The image and its decoded size in bytes.
    Ready(rune::Image, u64),
    Unavailable,
}

/// The UI thread's side: which of the current folder's files have thumbnails, as decoded
/// images bounded by [`DECODED_BUDGET`].
pub struct Thumbs {
    service: Service,
    receiver: mpsc::Receiver<Done>,
    /// Each file's listed modification time, the type it was asked about, and its thumbnail;
    /// an edited file, or one whose detected type arrives later, asks again.
    slots: HashMap<PathBuf, (u64, Option<String>, Slot)>,
    flavor: Flavor,
    decoded_bytes: u64,
    loaded: usize,
    /// Images handed to the decoder that haven't finished, to redraw when they do.
    decoding: Vec<rune::Image>,
}

impl Thumbs {
    pub fn new(wake: fn()) -> Thumbs {
        let (sender, receiver) = mpsc::channel();
        Thumbs {
            service: Service::start(sender, wake),
            receiver,
            slots: HashMap::new(),
            flavor: Flavor::Normal,
            decoded_bytes: 0,
            loaded: 0,
            decoding: Vec::new(),
        }
    }

    /// The local files now on screen. Thumbnails already resolved for them are kept;
    /// everything else is dropped and the rest queued. Files with no known or guessable type
    /// are skipped without touching the disk.
    pub fn show(&mut self, files: Vec<Request>, flavor: Flavor) {
        if flavor != self.flavor {
            self.slots.clear();
            self.flavor = flavor;
        }
        let mut old = std::mem::take(&mut self.slots);
        let mut requests = Vec::new();
        let mut considered = 0;
        for request in files {
            let Some(mime) = mime_for(request.mime.as_deref(), &request.path) else {
                continue;
            };
            if !worth_trying(&mime) {
                continue;
            }
            if considered >= MAX_LOADED {
                break;
            }
            considered += 1;
            // Keyed by the type actually used, so a detection that confirms the extension's
            // guess keeps the thumbnail already shown.
            let Request { path, mtime, .. } = request;
            let mime = Some(mime);
            match old.remove(&path) {
                Some((listed, asked, slot @ (Slot::Ready(..) | Slot::Unavailable)))
                    if listed == mtime && asked == mime =>
                {
                    self.slots.insert(path, (mtime, mime, slot));
                }
                _ => {
                    requests.push(Request {
                        path: path.clone(),
                        mtime,
                        mime: mime.clone(),
                    });
                    self.slots.insert(path, (mtime, mime, Slot::Pending));
                }
            }
        }
        self.recount();
        self.decoding.retain(|image| !image.is_ready());
        self.service.replace(requests, flavor);
    }

    fn recount(&mut self) {
        self.loaded = 0;
        self.decoded_bytes = 0;
        for (_, _, slot) in self.slots.values() {
            if let Slot::Ready(_, bytes) = slot {
                self.loaded += 1;
                self.decoded_bytes += bytes;
            }
        }
    }

    /// Takes finished thumbnails. True when one became visible.
    pub fn poll(&mut self, images: Option<&rune::ImageCache>) -> bool {
        let mut changed = false;
        while let Ok(done) = self.receiver.try_recv() {
            let Some((listed, asked, slot)) = self.slots.get_mut(&done.path) else {
                continue;
            };
            if *listed != done.mtime || *asked != done.mime || !matches!(slot, Slot::Pending) {
                continue;
            }
            *slot = match (done.outcome, images) {
                (Outcome::Ready(bytes, (width, height)), Some(images))
                    if self.loaded < MAX_LOADED
                        && self.decoded_bytes + decoded_size(width, height) <= DECODED_BUDGET =>
                {
                    let size = decoded_size(width, height);
                    self.loaded += 1;
                    self.decoded_bytes += size;
                    // Decoding runs on Rune's image threads; the cache's waker redraws.
                    let image = images.load(bytes);
                    if image.is_ready() {
                        changed = true;
                    } else {
                        self.decoding.push(image.clone());
                    }
                    Slot::Ready(image, size)
                }
                _ => Slot::Unavailable,
            };
        }
        let before = self.decoding.len();
        self.decoding.retain(|image| !image.is_ready());
        changed || self.decoding.len() != before
    }

    /// A decoded thumbnail for `path`, if it has one.
    pub fn get(&self, path: &Path, mtime: u64) -> Option<&rune::Image> {
        match self.slots.get(path)? {
            (listed, _, Slot::Ready(image, _))
                if *listed == mtime && image.is_ready() && image.error().is_none() =>
            {
                Some(image)
            }
            _ => None,
        }
    }
}

/// RGBA bytes with a full mip chain.
fn decoded_size(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * 4 * 4 / 3
}

#[cfg(test)]
mod tests;
