use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use rune::tok::{self, radius, type_};
use rune::{
    AppSpec, Color, Cursor, Cx, Drag, El, Key, KeyEvent, Phase, PointerPress, Transition, Tx, col,
    el, row, text,
};

mod exif;
mod file_manager1;
mod gio;
mod location;
mod mime;
mod notify;
mod thumbnail;
mod udisks;

use location::Location;

#[derive(Clone)]
struct Entry {
    path: Location,
    name: String,
    dir: bool,
    hidden: bool,
    size: u64,
    modified_secs: u64,
    modified: String,
    /// The shared-mime-info type: from the listing for remote entries, from the background
    /// detector for local files (`None` until it answers, or when `gio` isn't available).
    content_type: Option<String>,
    /// Where a GVfs shortcut or mountable entry (network:///, computer:///) leads.
    target: Option<Location>,
}

struct State {
    cwd: Location,
    entries: Vec<Entry>,
    selected: Option<Location>,
    selected_paths: Vec<Location>,
    selection_anchor: Option<Location>,
    preview: String,
    search: rune::TextEdit,
    favorites: Vec<Location>,
    status: String,
    show_hidden: bool,
    history: Vec<Location>,
    history_index: usize,
    image_cache: Option<rune::ImageCache>,
    selected_image: Option<rune::Image>,
    image_expanded: bool,
    context_menu: Option<(f32, f32)>,
    zoom: f32,
    preview_width: f32,
    icon_paths: HashMap<IconKind, PathBuf>,
    icons: HashMap<IconKind, rune::Image>,
    clipboard: Vec<PathBuf>,
    clipboard_cut: bool,
    sort: SortBy,
    ascending: bool,
    grid_view: bool,
    tabs: Vec<Location>,
    active_tab: usize,
    location_edit: rune::TextEdit,
    editing_location: bool,
    renaming: Option<PathBuf>,
    rename_edit: rune::TextEdit,
    operation: Option<String>,
    operation_rx: Option<mpsc::Receiver<OperationEvent>>,
    operation_cancel: Option<Arc<AtomicBool>>,
    /// What the running file operation is, for its end-of-operation status and notification.
    operation_info: Option<OperationInfo>,
    /// Whether the window has keyboard focus (from `on_focus`); operations that end while it
    /// doesn't send a desktop notification.
    window_focused: bool,
    notifier: notify::Notifier,
    /// Each entry is one "Move to Trash" action, so Undo restores the whole batch.
    undo_trash: Vec<Vec<PathBuf>>,
    /// Remote listings, mounts and `gio open` results arrive here from background threads.
    gio_tx: mpsc::Sender<GioEvent>,
    gio_rx: mpsc::Receiver<GioEvent>,
    /// The unfiltered, unsorted listing of a remote folder, kept so sorting and hidden-file
    /// toggles don't go back to the network.
    remote_listing: Vec<Entry>,
    /// Bumped per remote listing so a slow, superseded one can't replace a newer folder.
    listing_generation: u64,
    /// Shown in the file list while it's empty: loading progress or why a folder failed.
    list_message: Option<String>,
    mounts: Vec<gio::Mount>,
    mounts_refreshing: bool,
    /// Removable drives' file systems from UDisks2; listings and udisksctl results arrive on
    /// `device_rx` from background threads.
    volumes: Vec<udisks::Volume>,
    /// Why the last device listing failed; shown under DEVICES unless udisksctl is missing.
    volumes_error: Option<String>,
    volumes_refreshing: bool,
    device_tx: mpsc::Sender<DeviceEvent>,
    device_rx: mpsc::Receiver<DeviceEvent>,
    /// Volume objects and drives with a mount, unmount or removal in flight, and its verb.
    device_busy: HashMap<String, &'static str>,
    /// Right-click menu for a sidebar device: anchor and the volume's UDisks object path.
    device_menu: Option<(f32, f32, String)>,
    /// Freedesktop thumbnails for the current local folder, looked up and generated on
    /// background threads; started in `setup` once the window's image cache exists.
    thumbs: Option<thumbnail::Thumbs>,
    /// Detects local files' MIME types through GIO on a background thread; started in `setup`.
    mime: Option<mime::Detector>,
    /// `org.freedesktop.FileManager1` requests, translated on the D-Bus thread (started in
    /// `setup`) and applied in `poll`; that thread never sees `State`.
    file_manager_tx: mpsc::Sender<file_manager1::Event>,
    file_manager_rx: mpsc::Receiver<file_manager1::Event>,
    exif_tx: mpsc::Sender<ExifEvent>,
    exif_rx: mpsc::Receiver<ExifEvent>,
    exif_generation: u64,
    exif_details: Option<(PathBuf, u64, String)>,
    /// Items a FileManager1 request asked to select once their folder is listed (at once for
    /// local folders, when `gio` answers for remote ones).
    pending_reveal: Option<file_manager1::Reveal>,
    /// Left for `poll`, which has the `Tx` and the app handle, once a request is applied: the
    /// row to scroll to, whether to show the selection's properties, and the activation token
    /// to raise the window with.
    reveal_scroll: Option<String>,
    reveal_properties: bool,
    activation: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceOp {
    /// Mount, then open the volume in the current tab.
    Open,
    Mount,
    Unmount,
    Remove(udisks::Removal),
}

enum DeviceEvent {
    Volumes(Result<Vec<udisks::Volume>, String>),
    Done {
        /// The `device_busy` key: a volume object, or a drive for removals.
        key: String,
        name: String,
        op: DeviceOp,
        /// Where the volume was mounted before an unmount or removal.
        released: Vec<PathBuf>,
        /// The mount point, for mounts.
        result: Result<Option<PathBuf>, String>,
    },
}

enum GioEvent {
    Listed {
        generation: u64,
        result: Result<Vec<gio::RemoteEntry>, String>,
    },
    Mounts(Vec<gio::Mount>),
    OpenFailed(String, String),
}

struct ExifEvent {
    generation: u64,
    path: PathBuf,
    mtime: u64,
    details: Option<String>,
}

enum OperationEvent {
    Progress(usize, usize, String, &'static str),
    /// Time the worker spent waiting on the user (a "Replace file?" prompt), which doesn't
    /// make an operation "long" for notifications.
    Waited(Duration),
    Finished(usize),
    Cancelled(usize),
    Failed(usize, String),
}

/// A running file operation's description, set when it starts.
struct OperationInfo {
    kind: notify::Kind,
    total: usize,
    /// The item's name when there's only one.
    item: Option<String>,
    /// Where pasted items go; `None` for duplicates.
    destination: Option<String>,
    started: Instant,
    waited: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Name,
    Modified,
    Size,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum IconKind {
    Folder,
    Pdf,
    Image,
    Markdown,
    Code,
    Text,
    Archive,
    Audio,
    Video,
    Generic,
}

static START_DIRECTORY: OnceLock<Location> = OnceLock::new();
/// Wakes the event loop so `poll` drains events that background threads queued.
static WAKER: OnceLock<rune::Waker> = OnceLock::new();
/// Raises the window for FileManager1 requests.
static APP: OnceLock<rune::AppHandle<State>> = OnceLock::new();

fn wake() {
    if let Some(waker) = WAKER.get() {
        waker();
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn init_at(cwd: Location) -> State {
    let cwd = match cwd {
        Location::Local(cwd) if cwd.is_dir() => Location::Local(cwd),
        Location::Local(cwd) => {
            Location::Local(cwd.parent().map(Path::to_path_buf).unwrap_or_else(home))
        }
        remote => remote,
    };
    let settings = load_settings();
    let (gio_tx, gio_rx) = mpsc::channel();
    let (device_tx, device_rx) = mpsc::channel();
    let (file_manager_tx, file_manager_rx) = mpsc::channel();
    let (exif_tx, exif_rx) = mpsc::channel();
    let mut state = State {
        cwd: cwd.clone(),
        entries: Vec::new(),
        selected: None,
        selected_paths: Vec::new(),
        selection_anchor: None,
        preview: "Choose a file to preview it here.".into(),
        search: rune::TextEdit::new(""),
        favorites: load_favorites(),
        status: "Ready".into(),
        show_hidden: settings.5,
        history: vec![cwd.clone()],
        history_index: 0,
        image_cache: None,
        selected_image: None,
        image_expanded: false,
        context_menu: None,
        zoom: settings.0,
        preview_width: settings.1,
        icon_paths: find_system_icons(),
        icons: HashMap::new(),
        clipboard: Vec::new(),
        clipboard_cut: false,
        sort: settings.2,
        ascending: settings.3,
        grid_view: settings.4,
        tabs: vec![cwd.clone()],
        active_tab: 0,
        location_edit: rune::TextEdit::new(""),
        editing_location: false,
        renaming: None,
        rename_edit: rune::TextEdit::new(""),
        operation: None,
        operation_rx: None,
        operation_cancel: None,
        operation_info: None,
        window_focused: true,
        notifier: new_notifier(),
        undo_trash: Vec::new(),
        gio_tx,
        gio_rx,
        remote_listing: Vec::new(),
        listing_generation: 0,
        list_message: None,
        mounts: Vec::new(),
        mounts_refreshing: false,
        volumes: Vec::new(),
        volumes_error: None,
        volumes_refreshing: false,
        device_tx,
        device_rx,
        device_busy: HashMap::new(),
        device_menu: None,
        thumbs: None,
        mime: None,
        file_manager_tx,
        file_manager_rx,
        exif_tx,
        exif_rx,
        exif_generation: 0,
        exif_details: None,
        pending_reveal: None,
        reveal_scroll: None,
        reveal_properties: false,
        activation: None,
    };
    read_directory(&mut state);
    refresh_mounts(&mut state);
    refresh_volumes(&mut state);
    state
}

fn load_settings() -> (f32, f32, SortBy, bool, bool, bool) {
    let contents = fs::read_to_string(home().join(".config/ferune/settings")).unwrap_or_default();
    let value = |name: &str| {
        contents
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
    };
    (
        value("zoom")
            .and_then(|value| value.parse().ok())
            .unwrap_or(1.0f32)
            .clamp(0.75, 1.6),
        value("preview_width")
            .and_then(|value| value.parse().ok())
            .unwrap_or(300.0f32)
            .clamp(120.0, 520.0),
        match value("sort") {
            Some("modified") => SortBy::Modified,
            Some("size") => SortBy::Size,
            _ => SortBy::Name,
        },
        value("ascending") != Some("false"),
        value("grid") == Some("true"),
        value("hidden") == Some("true"),
    )
}

fn load_favorites() -> Vec<Location> {
    // Defaults apply only before the user has ever saved favorites, so removing every
    // favorite sticks across restarts.
    let config = home().join(".config/ferune/favorites");
    match fs::read_to_string(config) {
        Ok(contents) => contents
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(Location::from_config_line)
            .collect(),
        Err(_) => {
            let home = home();
            ["Documents", "Downloads", "Pictures"]
                .into_iter()
                .map(|name| Location::Local(home.join(name)))
                .collect()
        }
    }
}

/// Delivers to the session bus; tests record notices instead, so they never reach the desktop.
fn new_notifier() -> notify::Notifier {
    #[cfg(test)]
    return notify::Notifier::recording();
    #[cfg(not(test))]
    notify::Notifier::new(None)
}

fn init() -> State {
    init_at(
        START_DIRECTORY
            .get()
            .cloned()
            .unwrap_or_else(|| Location::Local(home())),
    )
}

fn local_file_uri(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    if !encoded.starts_with('/') {
        return None;
    }
    let bytes = location::percent_decode(encoded)?;
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// The first argument that names a local folder or a GIO URI (sftp://, smb://, mtp://, …).
fn startup_directory() -> Location {
    std::env::args_os()
        .skip(1)
        .filter_map(|arg| {
            let text = arg.to_str();
            if let Some(remote) = text
                .filter(|text| location::uri_scheme(text).is_some_and(|scheme| scheme != "file"))
                .and_then(Location::from_uri)
            {
                return Some(remote);
            }
            text.and_then(local_file_uri)
                .or_else(|| Some(PathBuf::from(arg)))
                .map(Location::Local)
        })
        .find(|location| location.as_local().is_none_or(Path::is_dir))
        .unwrap_or_else(|| Location::Local(home()))
}

fn read_directory(s: &mut State) {
    let cwd = match &s.cwd {
        Location::Local(cwd) => cwd.clone(),
        Location::Remote(uri) => {
            let uri = uri.clone();
            start_remote_listing(s, uri);
            return;
        }
    };
    s.list_message = None;
    s.remote_listing.clear();
    // Drop a remote listing still in flight from the folder we just left.
    s.listing_generation += 1;
    let entries = fs::read_dir(&cwd)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|item| {
            let path = item.path();
            let metadata = item.metadata().ok()?;
            let name = item.file_name().to_string_lossy().into_owned();
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| std::time::SystemTime::now().duration_since(time).ok())
                .map(|d| match d.as_secs() {
                    secs if secs < 3_600 => "Just now".into(),
                    secs if secs < 86_400 => format!("{}h ago", secs / 3_600),
                    secs => format!("{}d ago", secs / 86_400),
                })
                .unwrap_or_else(|| "—".into());
            Some(Entry {
                dir: metadata.is_dir(),
                hidden: name.starts_with('.'),
                name,
                path: Location::Local(path),
                size: metadata.len(),
                modified_secs: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_secs()),
                modified,
                content_type: None,
                target: None,
            })
        })
        .collect::<Vec<_>>();
    let entries = with_known_types(s, entries);
    show_entries(s, entries);
    request_mime_types(s);
}

/// Fills in the types the detector already knows, before the entries are first shown.
fn with_known_types(s: &State, mut entries: Vec<Entry>) -> Vec<Entry> {
    if let Some(detector) = &s.mime {
        for entry in entries.iter_mut().filter(|entry| !entry.dir) {
            if let Some(path) = entry.path.as_local() {
                entry.content_type = detector.get(path, entry.modified_secs).map(str::to_owned);
            }
        }
    }
    entries
}

/// Hands the listed local files to the MIME detector, and fills in the types it already
/// knows. Remote entries keep the content type their GIO listing carried.
fn request_mime_types(s: &mut State) {
    let (Some(detector), Location::Local(cwd)) = (&mut s.mime, &s.cwd) else {
        return;
    };
    let files = s
        .entries
        .iter()
        .filter(|entry| !entry.dir)
        .filter_map(|entry| Some((entry.path.as_local()?.to_path_buf(), entry.modified_secs)))
        .collect::<Vec<_>>();
    detector.show(cwd, &files);
    let mut changed = false;
    for entry in &mut s.entries {
        if let Some(path) = entry.path.as_local().filter(|_| !entry.dir) {
            let detected = detector.get(path, entry.modified_secs).map(str::to_owned);
            changed |= entry.content_type != detected;
            entry.content_type = detected;
        }
    }
    if changed {
        request_thumbnails(s);
    }
}

/// Takes detected types for the current folder: updates their entries, asks for thumbnails
/// that the new types allow, and refreshes the preview when the selected file's type changed.
fn poll_mime_types(s: &mut State) -> bool {
    let Some(detector) = &mut s.mime else {
        return false;
    };
    let typed = detector.poll();
    if typed.is_empty() {
        return false;
    }
    let typed = typed.into_iter().collect::<std::collections::HashSet<_>>();
    let mut changed = false;
    let mut selected_changed = false;
    for entry in &mut s.entries {
        let Some(path) = entry.path.as_local().filter(|_| !entry.dir) else {
            continue;
        };
        if !typed.contains(path) {
            continue;
        }
        let detected = detector.get(path, entry.modified_secs).map(str::to_owned);
        if entry.content_type != detected {
            // Only a different kind of file needs its preview rebuilt.
            selected_changed |= s.selected.as_ref() == Some(&entry.path)
                && FileType::of(entry.content_type.as_deref(), path)
                    != FileType::of(detected.as_deref(), path);
            entry.content_type = detected;
            changed = true;
        }
    }
    if changed {
        request_thumbnails(s);
    }
    if selected_changed
        && s.selected_paths.len() == 1
        && let Some(selected) = s.selected.clone()
    {
        refresh_preview(s, &selected);
    }
    changed
}

/// Applies the hidden-file filter and sort order, folders first.
fn show_entries(s: &mut State, entries: Vec<Entry>) {
    let mut entries = entries
        .into_iter()
        .filter(|entry| s.show_hidden || !entry.hidden)
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| {
        let by_value = match s.sort {
            SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortBy::Modified => a.modified_secs.cmp(&b.modified_secs),
            SortBy::Size => a.size.cmp(&b.size),
        };
        b.dir.cmp(&a.dir).then(if s.ascending {
            by_value
        } else {
            by_value.reverse()
        })
    });
    s.entries = entries;
    request_thumbnails(s);
}

/// Thumbnails drawn at this many points in the grid; list rows draw them at icon size.
const GRID_THUMBNAIL: f32 = 56.0;

/// Hands the shown local files to the thumbnail workers. Remote GVfs files keep their icons:
/// the thumbnail cache is keyed by local `file://` URIs, and generating one would read the
/// whole file over the network.
fn request_thumbnails(s: &mut State) {
    let Some(thumbs) = &mut s.thumbs else {
        return;
    };
    let files = if s.cwd.is_remote() {
        Vec::new()
    } else {
        s.entries
            .iter()
            .filter(|entry| !entry.dir)
            .filter_map(|entry| {
                Some(thumbnail::Request {
                    path: entry.path.as_local()?.to_path_buf(),
                    mtime: entry.modified_secs,
                    mime: entry.content_type.clone(),
                })
            })
            .collect()
    };
    // Twice the drawn size covers 2× displays.
    let flavor = thumbnail::Flavor::for_pixels((GRID_THUMBNAIL * s.zoom * 2.0) as u32);
    thumbs.show(files, flavor);
}

fn entry_thumbnail<'a>(s: &'a State, entry: &Entry) -> Option<&'a rune::Image> {
    if entry.dir {
        return None;
    }
    s.thumbs
        .as_ref()?
        .get(entry.path.as_local()?, entry.modified_secs)
}

/// The file's thumbnail when it has one, otherwise its themed icon.
fn entry_icon(s: &State, entry: &Entry, kind: IconKind, size: f32, accent: Color) -> El {
    match entry_thumbnail(s, entry) {
        Some(image) => el()
            .size(size, size)
            .min_w(size)
            .center()
            .radius(radius::SM)
            .pixels_fit(image.pixels(), rune::SurfaceFit::Fit),
        None => file_icon(s, kind, size, accent),
    }
}

/// Re-sorts or re-filters the current folder: local folders are re-read as before; remote
/// ones reuse the listing already fetched.
fn refresh_view(s: &mut State) {
    if s.cwd.is_remote() {
        if s.list_message.is_none() {
            show_entries(s, s.remote_listing.clone());
        }
    } else {
        read_directory(s);
    }
}

fn start_remote_listing(s: &mut State, uri: String) {
    s.listing_generation += 1;
    let generation = s.listing_generation;
    s.entries.clear();
    s.remote_listing.clear();
    s.list_message = Some(format!("Opening {}…", decode_display(&uri)));
    s.status = format!("Opening {}…", decode_display(&uri));
    let sender = s.gio_tx.clone();
    std::thread::spawn(move || {
        let result = gio::list_directory(&uri);
        let _ = sender.send(GioEvent::Listed { generation, result });
        wake();
    });
}

fn decode_display(uri: &str) -> String {
    location::decode_lossy(uri)
}

fn refresh_mounts(s: &mut State) {
    if s.mounts_refreshing {
        return;
    }
    s.mounts_refreshing = true;
    let sender = s.gio_tx.clone();
    std::thread::spawn(move || {
        let _ = sender.send(GioEvent::Mounts(gio::list_mounts()));
        wake();
    });
}

fn refresh_volumes(s: &mut State) {
    if s.volumes_refreshing {
        return;
    }
    s.volumes_refreshing = true;
    let sender = s.device_tx.clone();
    std::thread::spawn(move || {
        let _ = sender.send(DeviceEvent::Volumes(udisks::list_volumes()));
        wake();
    });
}

/// Re-lists devices whenever UDisks reports a change, for as long as Ferune runs.
fn watch_volumes(s: &State) {
    let sender = s.device_tx.clone();
    std::thread::spawn(move || {
        udisks::watch(|volumes| {
            let _ = sender.send(DeviceEvent::Volumes(volumes));
            wake();
        });
    });
}

/// The label, or a size-based name like GNOME's "16 GB Volume".
fn volume_name(volume: &udisks::Volume) -> String {
    volume
        .label
        .clone()
        .unwrap_or_else(|| format!("{} Volume", human_size(volume.size)))
}

fn volume_busy(s: &State, volume: &udisks::Volume) -> Option<&'static str> {
    s.device_busy
        .get(&volume.object)
        .or_else(|| s.device_busy.get(&volume.drive))
        .copied()
}

/// Mount, unmount or safely remove a sidebar device in the background. Opening a mounted
/// volume navigates straight to it. Nothing is ever mounted without one of these actions.
fn device_action(s: &mut State, object: &str, op: DeviceOp) {
    let Some(volume) = s.volumes.iter().find(|v| v.object == object).cloned() else {
        s.status = "That device is no longer connected".into();
        return;
    };
    if let Some(verb) = volume_busy(s, &volume) {
        s.status = format!("{} is busy: {verb}", volume_name(&volume));
        return;
    }
    let name = volume_name(&volume);
    let (key, name, released, verb) = match op {
        DeviceOp::Open if volume.is_mounted() => {
            navigate(s, Location::Local(volume.mount_points[0].clone()));
            return;
        }
        DeviceOp::Mount if volume.is_mounted() => {
            s.status = format!("{name} is already mounted");
            return;
        }
        DeviceOp::Unmount if !volume.is_mounted() => {
            s.status = format!("{name} isn't mounted");
            return;
        }
        DeviceOp::Open | DeviceOp::Mount => (volume.object.clone(), name, Vec::new(), "Mounting"),
        DeviceOp::Unmount => (
            volume.object.clone(),
            name,
            volume.mount_points.clone(),
            "Unmounting",
        ),
        DeviceOp::Remove(removal) => (
            volume.drive.clone(),
            volume.drive_name.clone(),
            s.volumes
                .iter()
                .filter(|other| other.drive == volume.drive)
                .flat_map(|other| other.mount_points.clone())
                .collect(),
            match removal {
                udisks::Removal::PowerOff => "Powering off",
                udisks::Removal::Eject => "Ejecting",
            },
        ),
    };
    s.status = format!("{verb} {name}…");
    s.device_busy.insert(key.clone(), verb);
    let sender = s.device_tx.clone();
    std::thread::spawn(move || {
        let result = match op {
            DeviceOp::Open | DeviceOp::Mount => udisks::mount(&volume).map(Some),
            DeviceOp::Unmount => udisks::unmount(&volume).map(|()| None),
            DeviceOp::Remove(removal) => {
                udisks::remove_drive(&volume.drive, removal).map(|()| None)
            }
        };
        let _ = sender.send(DeviceEvent::Done {
            key,
            name,
            op,
            released,
            result,
        });
        wake();
    });
}

/// Drains device listings and udisksctl results from background threads.
fn drain_device_events(s: &mut State) -> bool {
    let mut changed = false;
    while let Ok(event) = s.device_rx.try_recv() {
        changed = true;
        match event {
            DeviceEvent::Volumes(result) => {
                s.volumes_refreshing = false;
                match result {
                    Ok(volumes) => {
                        s.volumes = volumes;
                        s.volumes_error = None;
                    }
                    Err(error) => {
                        s.volumes.clear();
                        s.volumes_error = Some(error);
                    }
                }
                if let Some((_, _, object)) = &s.device_menu
                    && !s.volumes.iter().any(|volume| &volume.object == object)
                {
                    s.device_menu = None;
                }
            }
            DeviceEvent::Done {
                key,
                name,
                op,
                released,
                result,
            } => {
                s.device_busy.remove(&key);
                match (op, result) {
                    (DeviceOp::Open, Ok(Some(point))) => {
                        s.status = format!("Mounted {name} at {}", point.display());
                        navigate(s, Location::Local(point));
                    }
                    (DeviceOp::Mount, Ok(point)) => {
                        s.status = match point {
                            Some(point) => format!("Mounted {name} at {}", point.display()),
                            None => format!("Mounted {name}"),
                        };
                    }
                    (DeviceOp::Unmount | DeviceOp::Remove(_), Ok(_)) => {
                        // Don't leave a tab showing the empty mount point it just released.
                        if s.cwd
                            .as_local()
                            .is_some_and(|cwd| released.iter().any(|point| cwd.starts_with(point)))
                        {
                            navigate(s, Location::Local(home()));
                        }
                        s.status = match op {
                            DeviceOp::Remove(udisks::Removal::PowerOff) => {
                                format!("{name} is powered off; it's safe to unplug")
                            }
                            DeviceOp::Remove(udisks::Removal::Eject) => {
                                format!("Ejected {name}; it's safe to remove")
                            }
                            _ => format!("Unmounted {name}"),
                        };
                    }
                    (_, Ok(_)) => s.status = format!("Mounted {name}"),
                    (op, Err(error)) => {
                        let verb = match op {
                            DeviceOp::Open | DeviceOp::Mount => "mount",
                            DeviceOp::Unmount => "unmount",
                            DeviceOp::Remove(udisks::Removal::PowerOff) => "power off",
                            DeviceOp::Remove(udisks::Removal::Eject) => "eject",
                        };
                        s.status = format!("Couldn't {verb} {name}: {error}");
                    }
                }
                // The watcher normally catches up on its own; this covers a missing monitor.
                refresh_volumes(s);
            }
        }
    }
    changed
}

/// Device-menu entries: id, label, enabled.
fn device_items(s: &State, object: &str) -> Vec<(&'static str, &'static str, bool)> {
    let Some(volume) = s.volumes.iter().find(|volume| volume.object == object) else {
        return Vec::new();
    };
    let idle = volume_busy(s, volume).is_none();
    let mounted = volume.is_mounted();
    vec![
        ("open", "Open", idle),
        ("mount", "Mount", idle && !mounted),
        ("unmount", "Unmount", idle && mounted),
        ("", "", false),
        ("eject", "Eject", idle && volume.ejectable),
        (
            "power-off",
            "Safely Remove (Power Off)",
            idle && volume.can_power_off,
        ),
    ]
}

fn device_menu_select(s: &mut State, id: &'static str) {
    let Some((_, _, object)) = s.device_menu.take() else {
        return;
    };
    let op = match id {
        "open" => DeviceOp::Open,
        "mount" => DeviceOp::Mount,
        "unmount" => DeviceOp::Unmount,
        "eject" => DeviceOp::Remove(udisks::Removal::Eject),
        "power-off" => DeviceOp::Remove(udisks::Removal::PowerOff),
        _ => return,
    };
    device_action(s, &object, op);
}

fn context_menu_select(s: &mut State, id: &'static str) {
    s.context_menu = None;
    context_action(s, Some(id));
}

fn relative_age(modified_secs: Option<u64>) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    match modified_secs.and_then(|modified| now.checked_sub(modified)) {
        Some(secs) if secs < 3_600 => "Just now".into(),
        Some(secs) if secs < 86_400 => format!("{}h ago", secs / 3_600),
        Some(secs) => format!("{}d ago", secs / 86_400),
        None => "—".into(),
    }
}

fn remote_entry(item: gio::RemoteEntry) -> Option<Entry> {
    Some(Entry {
        path: Location::from_uri(&item.uri)?,
        dir: item.is_folder(),
        hidden: item.hidden,
        size: item.size,
        modified_secs: item.modified.unwrap_or(0),
        modified: relative_age(item.modified),
        content_type: item.content_type.clone(),
        target: item.target_uri.as_deref().and_then(Location::from_uri),
        name: item.name,
    })
}

/// Drains results from background `gio` work.
fn drain_gio_events(s: &mut State) -> bool {
    let mut changed = false;
    while let Ok(event) = s.gio_rx.try_recv() {
        changed = true;
        match event {
            GioEvent::Listed { generation, .. } if generation != s.listing_generation => {}
            GioEvent::Listed { result, .. } if s.cwd.is_remote() => {
                match result {
                    Ok(items) => {
                        s.remote_listing = items.into_iter().filter_map(remote_entry).collect();
                        s.list_message = s
                            .remote_listing
                            .is_empty()
                            .then(|| "This folder is empty.".into());
                        show_entries(s, s.remote_listing.clone());
                        s.status = format!("Connected · {}", s.cwd.display_uri());
                        apply_pending_reveal(s);
                    }
                    Err(error) => {
                        s.pending_reveal = None;
                        s.remote_listing.clear();
                        s.entries.clear();
                        s.list_message = Some(format!("Couldn't open this location.\n\n{error}"));
                        s.status = format!("Couldn't open {}: {error}", s.cwd.display_uri());
                    }
                }
                // Listing may have mounted the location; show it in the sidebar.
                refresh_mounts(s);
            }
            GioEvent::Listed { .. } => {}
            GioEvent::Mounts(mounts) => {
                s.mounts_refreshing = false;
                s.mounts = mounts;
            }
            GioEvent::OpenFailed(name, error) => {
                s.status = format!("Couldn't open {name}: {error}");
            }
        }
    }
    changed
}

fn human_size(size: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut n = size as f64;
    let mut unit = 0;
    while n >= 1024.0 && unit < UNITS.len() - 1 {
        n /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{n:.1} {}", UNITS[unit])
    }
}

fn scaled_font(style: rune::TextStyle, scale: f32) -> rune::TextStyle {
    rune::TextStyle {
        size: style.size * scale,
        line_height: style.line_height * scale,
        tracking: style.tracking * scale,
        ..style
    }
}

/// What a file is, for its icon, the preview pane's label and which preview to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileType {
    Pdf,
    Markdown,
    /// An image Rune can decode for the preview.
    Image,
    /// Any other image format (thumbnailers may still handle it).
    OtherImage,
    /// Prose: plain text, logs, READMEs.
    Text,
    /// Text that's source code or configuration.
    Code,
    Archive,
    Audio,
    Video,
    /// A type shared-mime-info knows that isn't any of the above (not text).
    Other,
    /// Neither GIO nor the extension says.
    Unknown,
}

impl FileType {
    /// From a detected MIME type when it's informative, otherwise from the extension, which
    /// is also how files are typed when `gio` or shared-mime-info is missing.
    fn of(content_type: Option<&str>, path: &Path) -> FileType {
        match mime::effective(content_type, path) {
            Some(mime) => FileType::from_mime(mime, path),
            None => FileType::Unknown,
        }
    }

    fn from_mime(mime: &str, path: &Path) -> FileType {
        let mime = mime::canonical(&mime.to_ascii_lowercase());
        let mime = mime.as_str();
        let major = mime.split('/').next().unwrap_or("");
        if mime == "application/pdf" {
            FileType::Pdf
        } else if mime::is_a(mime, "text/markdown") {
            FileType::Markdown
        } else if matches!(
            mime,
            "image/png"
                | "image/jpeg"
                | "image/gif"
                | "image/webp"
                | "image/bmp"
                | "image/vnd.microsoft.icon"
                | "image/svg+xml"
                | "image/svg+xml-compressed"
        ) {
            FileType::Image
        } else if major == "image" {
            FileType::OtherImage
        } else if major == "audio" {
            FileType::Audio
        } else if major == "video" {
            FileType::Video
        } else if mime::generic_icon(mime) == Some("package-x-generic")
            || ARCHIVE_TYPES.contains(&mime)
        {
            FileType::Archive
        } else if mime::is_text(mime) {
            match mime {
                "text/plain" | "text/x-log" | "text/x-readme" | "text/rtf" => {
                    // Configuration files are typed text/plain; keep their code icon.
                    match path.extension().and_then(|ext| ext.to_str()) {
                        Some(ext) if ["ini", "conf", "cfg"].contains(&ext) => FileType::Code,
                        _ => FileType::Text,
                    }
                }
                _ => FileType::Code,
            }
        } else if mime == "application/rtf" {
            FileType::Text
        } else {
            FileType::Other
        }
    }

    /// The preview pane's label.
    fn label(self) -> &'static str {
        match self {
            FileType::Pdf => "PDF DOCUMENT",
            FileType::Markdown => "MARKDOWN",
            FileType::Image | FileType::OtherImage => "IMAGE",
            FileType::Text | FileType::Code => "TEXT FILE",
            _ => "FILE",
        }
    }

    fn icon(self) -> IconKind {
        match self {
            FileType::Pdf => IconKind::Pdf,
            FileType::Markdown => IconKind::Markdown,
            FileType::Image | FileType::OtherImage => IconKind::Image,
            FileType::Text => IconKind::Text,
            FileType::Code => IconKind::Code,
            FileType::Archive => IconKind::Archive,
            FileType::Audio => IconKind::Audio,
            FileType::Video => IconKind::Video,
            FileType::Other | FileType::Unknown => IconKind::Generic,
        }
    }
}

/// Archives whose shared-mime-info entry may lack the `package-x-generic` generic icon, or
/// for when the database isn't loaded.
const ARCHIVE_TYPES: &[&str] = &[
    "application/zip",
    "application/x-7z-compressed",
    "application/vnd.rar",
    "application/x-rar",
    "application/x-tar",
    "application/gzip",
    "application/x-bzip2",
    "application/x-xz",
    "application/zstd",
    "application/x-compressed-tar",
    "application/vnd.debian.binary-package",
    "application/x-rpm",
];

/// A local file's type from its listing entry (or its name, without one).
fn local_file_type(s: &State, path: &Path) -> FileType {
    let content_type = s
        .entries
        .iter()
        .find(|entry| entry.path.as_local() == Some(path))
        .and_then(|entry| entry.content_type.as_deref());
    FileType::of(content_type, path)
}

fn icon_fallback_glyph(kind: IconKind) -> &'static str {
    match kind {
        IconKind::Folder => "▰",
        IconKind::Pdf => "▤",
        IconKind::Image => "▧",
        IconKind::Markdown | IconKind::Text => "≡",
        IconKind::Code => "‹›",
        IconKind::Archive => "▣",
        IconKind::Audio => "♫",
        IconKind::Video => "▷",
        IconKind::Generic => "◇",
    }
}

fn file_icon(s: &State, kind: IconKind, size: f32, accent: Color) -> El {
    if let Some(pixels) = s.icons.get(&kind).map(rune::Image::pixels) {
        el().size(size, size)
            .center()
            .pixels_fit(pixels, rune::SurfaceFit::Fit)
    } else {
        text(icon_fallback_glyph(kind))
            .font(rune::TextStyle::new(size * 0.8, 500.0))
            .color(accent)
    }
}

/// The icon for an entry: folders by kind, files by their MIME type (detected, or guessed
/// from the extension).
fn entry_icon_kind(entry: &Entry) -> IconKind {
    if entry.dir {
        return IconKind::Folder;
    }
    let name = Path::new(&entry.name);
    FileType::of(entry.content_type.as_deref(), name).icon()
}

impl IconKind {
    fn themed_names(self) -> &'static [&'static str] {
        match self {
            Self::Folder => &["folder"],
            Self::Pdf => &["application-pdf", "application-x-pdf"],
            Self::Image => &["image-x-generic", "image-png"],
            Self::Markdown => &["text-markdown", "text-x-markdown", "text-x-generic"],
            Self::Code => &["text-x-script", "text-x-source", "text-x-generic"],
            Self::Text => &["text-plain", "text-x-generic"],
            Self::Archive => &[
                "package-x-generic",
                "application-x-archive",
                "application-zip",
            ],
            Self::Audio => &["audio-x-generic"],
            Self::Video => &["video-x-generic"],
            Self::Generic => &["application-x-generic", "unknown"],
        }
    }

    fn fallback_svg(self) -> String {
        let body = match self {
            Self::Folder => {
                r#"<path d="M3 7.5a2 2 0 0 1 2-2h5l2 2h7a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/><path d="M3 9h18"/>"#
            }
            Self::Pdf => {
                r#"<path d="M6 2.8h8l4 4v14.4H6z"/><path d="M14 2.8v4h4M8.5 12h7M8.5 15h7"/><path d="M8.5 18h4"/>"#
            }
            Self::Image => {
                r#"<rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="8.5" cy="9" r="1.5"/><path d="m4 17 5-5 3 3 3-4 5 6"/>"#
            }
            Self::Markdown => {
                r#"<path d="M6 2.8h8l4 4v14.4H6z"/><path d="M14 2.8v4h4M8.5 15l2-5 2 5M9.2 13h2.6M14.5 11v5m-2-2 2 2 2-2"/>"#
            }
            Self::Code => {
                r#"<path d="M6 2.8h8l4 4v14.4H6z"/><path d="M14 2.8v4h4M10 11l-2 2 2 2m4-4 2 2-2 2"/>"#
            }
            Self::Text => {
                r#"<path d="M6 2.8h8l4 4v14.4H6z"/><path d="M14 2.8v4h4M8.5 11h7m-7 3h7m-7 3h5"/>"#
            }
            Self::Archive => {
                r#"<path d="M4 7h16v13H4zM3 4h18v3H3z"/><path d="M10 10h4v2h-4zm0 2v3h4v-3"/>"#
            }
            Self::Audio => {
                r#"<path d="M14 18V5l6-1v13"/><circle cx="10" cy="18" r="3"/><circle cx="17" cy="17" r="3"/>"#
            }
            Self::Video => {
                r#"<rect x="3" y="5" width="18" height="14" rx="2"/><path d="m10 9 5 3-5 3z"/>"#
            }
            Self::Generic => r#"<path d="M6 2.8h8l4 4v14.4H6z"/><path d="M14 2.8v4h4"/>"#,
        };
        let color = if self == Self::Pdf {
            "#d96a63"
        } else {
            "#9aa5b1"
        };
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="{color}" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">{body}</svg>"##
        )
    }
}

fn preferred_icon_theme() -> Option<String> {
    [
        home().join(".config/gtk-4.0/settings.ini"),
        home().join(".config/gtk-3.0/settings.ini"),
    ]
    .iter()
    .filter_map(|path| fs::read_to_string(path).ok())
    .flat_map(|contents| contents.lines().map(str::to_owned).collect::<Vec<_>>())
    .find_map(|line| {
        line.trim()
            .strip_prefix("gtk-icon-theme-name=")
            .map(|name| name.trim_matches(['\'', '"']).to_owned())
    })
}

fn icon_theme_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("icons"),
        home().join(".icons"),
    ];
    roots.extend(
        std::env::var_os("XDG_DATA_DIRS")
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into())
            .to_string_lossy()
            .split(':')
            .map(|directory| PathBuf::from(directory).join("icons")),
    );
    roots
}

fn find_system_icon(kind: IconKind, roots: &[PathBuf], themes: &[String]) -> Option<PathBuf> {
    let contexts: &[&str] = if kind == IconKind::Folder {
        &["places", "mimetypes"]
    } else {
        &["mimetypes"]
    };
    let sizes = [
        "scalable", "symbolic", "48x48", "32x32", "24x24", "22x22", "16x16",
    ];
    let formats = ["svg", "png"];
    for root in roots {
        for theme in themes {
            for size in sizes {
                for context in contexts {
                    for name in kind.themed_names() {
                        for format in formats {
                            let candidate = root
                                .join(theme)
                                .join(size)
                                .join(context)
                                .join(format!("{name}.{format}"));
                            if candidate.is_file() {
                                return Some(candidate);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn find_system_icons() -> HashMap<IconKind, PathBuf> {
    let roots = icon_theme_roots();
    let mut themes = Vec::new();
    if let Some(preferred) = preferred_icon_theme() {
        themes.push(preferred);
    }
    for name in ["Adwaita", "Papirus", "hicolor"] {
        if !themes.iter().any(|theme| theme == name) {
            themes.push(name.into());
        }
    }
    [
        IconKind::Folder,
        IconKind::Pdf,
        IconKind::Image,
        IconKind::Markdown,
        IconKind::Code,
        IconKind::Text,
        IconKind::Archive,
        IconKind::Audio,
        IconKind::Video,
        IconKind::Generic,
    ]
    .into_iter()
    .filter_map(|kind| find_system_icon(kind, &roots, &themes).map(|path| (kind, path)))
    .collect()
}

fn preview_file(path: &Path, kind: FileType) -> String {
    match kind {
        FileType::Pdf => return Command::new("pdftotext").args(["-f", "1", "-l", "1", "-layout"])
            .arg(path).arg("-").output().ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).chars().take(5000).collect())
            .filter(|s: &String| !s.trim().is_empty())
            .unwrap_or_else(|| "PDF preview\n\nInstall poppler-utils to show the first page's text. Use Open to view the full document in your PDF reader.".into()),
        FileType::Image => {
            let dims = fs::read(path).ok().and_then(|bytes| image_dimensions(&bytes));
            return format!(
                "IMAGE PREVIEW\n\n{}\n\n{}\n\nImage preview · {}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                dims.map(|(w, h)| format!("{w} × {h} pixels"))
                    .unwrap_or_else(|| "Image file".into()),
                human_size(fs::metadata(path).map(|m| m.len()).unwrap_or_default())
            );
        }
        FileType::Markdown => {
            let source =
                fs::read_to_string(path).unwrap_or_else(|_| "Unable to read this file.".into());
            return markdown_preview(&source);
        }
        // Known not to be text (archives, media, executables): don't read it as text.
        FileType::OtherImage
        | FileType::Archive
        | FileType::Audio
        | FileType::Video
        | FileType::Other => {
            return "This file can't be previewed as text.".into();
        }
        FileType::Text | FileType::Code | FileType::Unknown => {}
    }
    fs::read_to_string(path)
        .map(|s| s.chars().take(6000).collect())
        .unwrap_or_else(|_| "This file can't be previewed as text.".into())
}

/// PNG and GIF dimensions from their headers, whatever the file is named.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 24 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
        Some((
            u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            u32::from_be_bytes(bytes[20..24].try_into().ok()?),
        ))
    } else if bytes.len() >= 10 && &bytes[..3] == b"GIF" {
        Some((
            u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32,
            u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32,
        ))
    } else {
        None
    }
}

fn markdown_preview(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            let line = line.trim();
            if let Some(heading) = line.strip_prefix("### ") {
                heading.to_uppercase()
            } else if let Some(heading) = line.strip_prefix("## ") {
                format!("▰  {}", heading.to_uppercase())
            } else if let Some(heading) = line.strip_prefix("# ") {
                format!("◆  {}", heading.to_uppercase())
            } else if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                format!("  •  {item}")
            } else if line.starts_with("```") {
                String::new()
            } else {
                line.replace("**", "").replace("__", "").replace('`', "")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn open(path: &Path) {
    let _ = Command::new("xdg-open").arg(path).spawn();
}

/// Opens a file in its default application: local files through `xdg-open` as before,
/// remote ones through `gio open`, whose failure is reported in the status bar.
fn open_location(s: &mut State, location: &Location) {
    match location {
        Location::Local(path) => open(path),
        Location::Remote(uri) => {
            let name = location.name();
            s.status = format!("Opening {name}…");
            let uri = uri.clone();
            let sender = s.gio_tx.clone();
            std::thread::spawn(move || {
                if let Err(error) = gio::open(&uri) {
                    let _ = sender.send(GioEvent::OpenFailed(name, error));
                    wake();
                }
            });
        }
    }
}

fn format_location(location: &Location) -> String {
    match location {
        Location::Local(path) => format_path(path),
        Location::Remote(_) => location.display_uri(),
    }
}

fn entry_for<'a>(s: &'a State, location: &Location) -> Option<&'a Entry> {
    s.entries
        .iter()
        .chain(&s.remote_listing)
        .find(|entry| &entry.path == location)
}

/// Whether a location is a folder. Remote locations answer from their listing entry; ones
/// without an entry (the current folder, sidebar items) are folders.
fn location_is_dir(s: &State, location: &Location) -> bool {
    match location {
        Location::Local(path) => path.is_dir(),
        Location::Remote(_) => entry_for(s, location).is_none_or(|entry| entry.dir),
    }
}

/// Where browsing into `location` leads, or `None` when it's a file to open instead.
fn folder_target(s: &State, location: &Location) -> Option<Location> {
    match location {
        Location::Local(path) => path.is_dir().then(|| location.clone()),
        Location::Remote(_) => match entry_for(s, location) {
            Some(entry) if entry.target.is_some() => entry.target.clone(),
            Some(entry) => entry.dir.then(|| location.clone()),
            None => Some(location.clone()),
        },
    }
}

fn location_icon_kind(s: &State, location: &Location) -> IconKind {
    match location {
        Location::Local(path) if path.is_dir() => IconKind::Folder,
        Location::Local(path) => local_file_type(s, path).icon(),
        Location::Remote(_) if location_is_dir(s, location) => IconKind::Folder,
        Location::Remote(_) => FileType::of(
            entry_for(s, location).and_then(|entry| entry.content_type.as_deref()),
            Path::new(&location.name()),
        )
        .icon(),
    }
}

fn is_trash(location: &Location) -> bool {
    location.as_local() == Some(trash_path().as_path())
}

fn local_paths(locations: &[Location]) -> Vec<PathBuf> {
    locations
        .iter()
        .filter_map(Location::as_local)
        .map(Path::to_path_buf)
        .collect()
}

/// Copying, moving, renaming and deleting go through `std::fs`, so they stay off remote
/// folders; returns false (with a status message) there.
fn ensure_local_cwd(s: &mut State) -> bool {
    if s.cwd.is_remote() {
        s.status = "File operations aren't available in remote locations yet".into();
        false
    } else {
        true
    }
}

fn format_path(path: &Path) -> String {
    let home = home();
    path.strip_prefix(&home)
        .ok()
        .map(|tail| format!("~{}{}", std::path::MAIN_SEPARATOR, tail.display()))
        .unwrap_or_else(|| path.display().to_string())
}

/// Writes a small config file atomically on the calling thread, so rapid saves (zoom keys,
/// favorite toggles) can't land out of order the way detached writer threads could.
fn write_config(name: &str, contents: &str) {
    let config = home().join(".config/ferune").join(name);
    if let Some(parent) = config.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let temp = config.with_extension("tmp");
    if fs::write(&temp, contents).is_ok() {
        let _ = fs::rename(&temp, &config);
    }
}

fn save_favorites(favorites: &[Location]) {
    let contents = favorites
        .iter()
        .map(Location::to_config_line)
        .collect::<Vec<_>>()
        .join("\n");
    write_config("favorites", &contents);
}

fn save_settings(s: &State) {
    let sort = match s.sort {
        SortBy::Name => "name",
        SortBy::Modified => "modified",
        SortBy::Size => "size",
    };
    let contents = format!(
        "zoom={}\npreview_width={}\nsort={sort}\nascending={}\ngrid={}\nhidden={}\n",
        s.zoom, s.preview_width, s.ascending, s.grid_view, s.show_hidden
    );
    write_config("settings", &contents);
}

fn toggle_favorite(s: &mut State, path: &Location) {
    if let Some(index) = s.favorites.iter().position(|favorite| favorite == path) {
        s.favorites.remove(index);
        s.status = "Removed from Favorites".into();
    } else {
        s.favorites.push(path.clone());
        s.status = "Added to Favorites".into();
    }
    save_favorites(&s.favorites);
}

fn copy_path(source: &Path, destination: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            let target = fs::read_link(source)?;
            std::os::unix::fs::symlink(target, destination)?;
        }
        #[cfg(not(unix))]
        fs::copy(source, destination)?;
    } else if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_path(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        fs::copy(source, destination)?;
    }
    Ok(())
}

fn file_uri(path: &Path) -> String {
    let mut encoded = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn write_file_clipboard(paths: &[PathBuf], cut: bool) {
    if paths.is_empty() {
        return;
    }
    let mut content = String::from(if cut { "cut\n" } else { "copy\n" });
    for path in paths {
        content.push_str(&file_uri(path));
        content.push('\n');
    }
    if let Ok(mut child) = Command::new("wl-copy")
        .args(["--type", "x-special/gnome-copied-files"])
        .stdin(Stdio::piped())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(content.as_bytes());
        }
        let _ = child.wait();
    }
}

fn read_file_clipboard() -> Option<(bool, Vec<PathBuf>)> {
    let output = Command::new("wl-paste")
        .args(["--no-newline", "--type", "x-special/gnome-copied-files"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let cut = lines.next()? == "cut";
    // A cut that another paste already completed leaves stale paths on the clipboard.
    let paths = lines
        .filter_map(local_file_uri)
        .filter(|path| path.symlink_metadata().is_ok())
        .collect::<Vec<_>>();
    (!paths.is_empty()).then_some((cut, paths))
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn copy_path_interruptible(
    source: &Path,
    destination: &Path,
    cancel: &AtomicBool,
) -> std::io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "operation cancelled",
        ));
    }
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(fs::read_link(source)?, destination)?;
        #[cfg(not(unix))]
        fs::copy(source, destination)?;
    } else if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        for item in fs::read_dir(source)? {
            let item = item?;
            copy_path_interruptible(&item.path(), &destination.join(item.file_name()), cancel)?;
        }
    } else {
        fs::copy(source, destination)?;
    }
    Ok(())
}

fn unique_copy_name(source: &Path) -> PathBuf {
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let stem = source.file_stem().unwrap_or_default().to_string_lossy();
    let extension = source.extension().map(|value| value.to_string_lossy());
    (1..1000)
        .map(|number| {
            let name = match &extension {
                Some(extension) => format!("{stem} copy {number}.{extension}"),
                None => format!("{stem} copy {number}"),
            };
            parent.join(name)
        })
        .find(|path| !path.exists())
        .unwrap_or_else(|| parent.join(format!("{stem} copy")))
}

fn transfer_clipboard(s: &mut State) {
    if s.clipboard.is_empty() {
        if let Some((cut, paths)) = read_file_clipboard() {
            s.clipboard_cut = cut;
            s.clipboard = paths;
        } else {
            s.status = "Clipboard is empty".into();
            return;
        }
    }
    if s.operation_rx.is_some() {
        s.status = "A file operation is already running".into();
        return;
    }
    let Location::Local(destination_dir) = s.cwd.clone() else {
        ensure_local_cwd(s);
        return;
    };
    let items = s.clipboard.clone();
    let cut = s.clipboard_cut;
    let kind = if cut {
        notify::Kind::Move
    } else {
        notify::Kind::Copy
    };
    let (thread_cancel, sender) = begin_operation(
        s,
        kind,
        &items,
        Some(format_path(&destination_dir)),
        "Starting paste…",
    );
    std::thread::spawn(move || {
        let total = items.len();
        let mut completed = 0;
        let verb = if cut { "Moving" } else { "Copying" };
        for source in items {
            if thread_cancel.load(Ordering::Relaxed) {
                let _ = sender.send(OperationEvent::Cancelled(completed));
                return;
            }
            let Some(name) = source.file_name() else {
                continue;
            };
            if source.symlink_metadata().is_err() {
                completed += 1;
                continue;
            }
            let mut destination = destination_dir.join(name);
            if destination == source {
                if cut {
                    // Moving an item onto itself is a no-op; never offer to "replace" it,
                    // which would delete the source.
                    completed += 1;
                    continue;
                }
                destination = unique_copy_name(&source);
            }
            if source.is_dir() && destination_dir.starts_with(&source) {
                let _ = sender.send(OperationEvent::Failed(
                    completed,
                    format!("Can't paste {} into itself", name.to_string_lossy()),
                ));
                return;
            }
            if destination.symlink_metadata().is_ok() {
                let asked = Instant::now();
                let overwrite = Command::new("zenity")
                    .args([
                        "--question",
                        "--title=Replace file?",
                        &format!(
                            "--text=“{}” already exists here. Replace it?",
                            name.to_string_lossy()
                        ),
                        "--ok-label=Replace",
                        "--cancel-label=Skip",
                    ])
                    .status()
                    .is_ok_and(|status| status.success());
                let _ = sender.send(OperationEvent::Waited(asked.elapsed()));
                if !overwrite {
                    // Skipped items count as processed so a partial cut drains them too.
                    completed += 1;
                    let _ = sender.send(OperationEvent::Progress(
                        completed,
                        total,
                        format!("skipped {}", name.to_string_lossy()),
                        verb,
                    ));
                    continue;
                }
                if let Err(error) = remove_path(&destination) {
                    let _ = sender.send(OperationEvent::Failed(completed, error.to_string()));
                    return;
                }
            }
            let result = if cut {
                fs::rename(&source, &destination).or_else(|_| {
                    copy_path_interruptible(&source, &destination, &thread_cancel)?;
                    remove_path(&source)
                })
            } else {
                copy_path_interruptible(&source, &destination, &thread_cancel)
            };
            match result {
                Ok(()) => {
                    completed += 1;
                    let _ = sender.send(OperationEvent::Progress(
                        completed,
                        total,
                        name.to_string_lossy().into_owned(),
                        verb,
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    let _ = remove_path(&destination);
                    let _ = sender.send(OperationEvent::Cancelled(completed));
                    return;
                }
                Err(error) => {
                    let _ = sender.send(OperationEvent::Failed(completed, error.to_string()));
                    return;
                }
            }
        }
        let _ = sender.send(OperationEvent::Finished(completed));
    });
}

/// Marks a file operation as running (one at a time) and returns the worker's cancel flag and
/// event sender.
fn begin_operation(
    s: &mut State,
    kind: notify::Kind,
    items: &[PathBuf],
    destination: Option<String>,
    starting: &str,
) -> (Arc<AtomicBool>, mpsc::Sender<OperationEvent>) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();
    s.operation = Some(starting.into());
    s.operation_cancel = Some(cancel.clone());
    s.operation_rx = Some(receiver);
    s.operation_info = Some(OperationInfo {
        kind,
        total: items.len(),
        item: match items {
            [only] => only
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            _ => None,
        },
        destination,
        started: Instant::now(),
        waited: Duration::ZERO,
    });
    (cancel, sender)
}

/// Copies each selected item next to itself as “name copy N”, in the background like a paste.
fn duplicate_selection(s: &mut State) {
    if s.operation_rx.is_some() {
        s.status = "A file operation is already running".into();
        return;
    }
    let items = local_paths(&s.selected_paths);
    if items.is_empty() {
        s.status = "Nothing to duplicate".into();
        return;
    }
    let (cancel, sender) = begin_operation(
        s,
        notify::Kind::Duplicate,
        &items,
        None,
        "Starting duplicate…",
    );
    std::thread::spawn(move || {
        let total = items.len();
        for (done, source) in items.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                let _ = sender.send(OperationEvent::Cancelled(done));
                return;
            }
            let destination = unique_copy_name(source);
            match copy_path_interruptible(source, &destination, &cancel) {
                Ok(()) => {
                    let name = source.file_name().unwrap_or_default().to_string_lossy();
                    let _ = sender.send(OperationEvent::Progress(
                        done + 1,
                        total,
                        name.into_owned(),
                        "Duplicating",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    let _ = remove_path(&destination);
                    let _ = sender.send(OperationEvent::Cancelled(done));
                    return;
                }
                Err(error) => {
                    let _ = sender.send(OperationEvent::Failed(done, error.to_string()));
                    return;
                }
            }
        }
        let _ = sender.send(OperationEvent::Finished(total));
    });
}

fn cancel_operation(s: &mut State) {
    if let Some(cancel) = &s.operation_cancel {
        cancel.store(true, Ordering::Relaxed);
        s.status = "Cancelling file operation…".into();
    }
}

fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    let reveal_changed = drain_file_manager_events(s);
    let gio_changed = drain_gio_events(s);
    let devices_changed = drain_device_events(s);
    let exif_changed = poll_exif(s);
    let thumbs_changed = s
        .thumbs
        .as_mut()
        .is_some_and(|thumbs| thumbs.poll(s.image_cache.as_ref()));
    let mime_changed = poll_mime_types(s);
    finish_reveal(s, tx);
    poll_operation(s, tx)
        || reveal_changed
        || gio_changed
        || devices_changed
        || exif_changed
        || thumbs_changed
        || mime_changed
}

fn request_exif(s: &mut State, path: &Path, mtime: u64) {
    s.exif_generation = s.exif_generation.wrapping_add(1);
    s.exif_details = None;
    let generation = s.exif_generation;
    let path = path.to_path_buf();
    let sender = s.exif_tx.clone();
    std::thread::spawn(move || {
        let details = exif::read(&path);
        let _ = sender.send(ExifEvent {
            generation,
            path,
            mtime,
            details,
        });
        wake();
    });
}

fn poll_exif(s: &mut State) -> bool {
    let mut changed = false;
    while let Ok(event) = s.exif_rx.try_recv() {
        if event.generation != s.exif_generation
            || s.selected != Some(Location::Local(event.path.clone()))
        {
            continue;
        }
        let still_current = s.entries.iter().any(|entry| {
            entry.path.as_local() == Some(event.path.as_path())
                && entry.modified_secs == event.mtime
        });
        if !still_current {
            continue;
        }
        if let Some(details) = event.details {
            s.exif_details = Some((event.path, event.mtime, details));
            changed = true;
        }
    }
    changed
}

fn drain_file_manager_events(s: &mut State) -> bool {
    let mut changed = false;
    while let Ok(event) = s.file_manager_rx.try_recv() {
        match event {
            file_manager1::Event::Registered => {}
            // Usually another file manager owns the name; Ferune works the same without it.
            file_manager1::Event::Unavailable(why) => {
                eprintln!("ferune: not serving {}: {why}", file_manager1::BUS_NAME);
            }
            file_manager1::Event::Request(request) => {
                apply_file_manager_request(s, request);
                changed = true;
            }
        }
    }
    changed
}

/// Shows what another application asked for over FileManager1: each folder in a tab (an
/// existing tab showing it, the untouched startup tab, or a new one), the last in front,
/// with its requested items selected.
fn apply_file_manager_request(s: &mut State, request: file_manager1::Request) {
    s.activation = Some(request.startup_id);
    cancel_rename(s);
    s.context_menu = None;
    s.device_menu = None;
    s.image_expanded = false;
    // A window nobody has used yet (just started, or started by D-Bus activation for this
    // very call) shows the first folder instead of keeping an extra tab around.
    let mut reuse_tab = s.tabs.len() == 1 && s.history.len() == 1 && s.selected.is_none();
    let mut problems = request.problems;
    let mut shown = 0;
    for reveal in request.reveals {
        if !show_reveal_folder(s, &reveal.folder, std::mem::take(&mut reuse_tab)) {
            problems.push(format!(
                "{} isn't a folder",
                format_location(&reveal.folder)
            ));
            continue;
        }
        shown += 1;
        // Selection belongs to the window, not a tab: only the folder left in front keeps it.
        s.pending_reveal = Some(reveal);
        if !s.cwd.is_remote() {
            apply_pending_reveal(s);
        }
    }
    if !problems.is_empty() {
        let problems = problems.join("; ");
        s.status = if shown == 0 {
            format!("Couldn't show the requested items: {problems}")
        } else {
            format!("{} · {problems}", s.status)
        };
    }
}

/// Brings `folder` to the front; false (changing nothing) when a local one isn't a folder.
fn show_reveal_folder(s: &mut State, folder: &Location, reuse_tab: bool) -> bool {
    if let Location::Local(path) = folder
        && !path.is_dir()
        && *path != trash_path()
    {
        return false;
    }
    match s.tabs.iter().position(|tab| tab == folder) {
        // Re-read even when it's in front: the caller may have just made the file (a finished
        // download).
        Some(index) if index == s.active_tab => show_directory(s, folder.clone()),
        Some(index) => switch_tab(s, index),
        None => {
            if !reuse_tab {
                s.tabs.push(folder.clone());
                s.active_tab = s.tabs.len() - 1;
            }
            navigate(s, folder.clone());
        }
    }
    true
}

/// Selects the pending reveal's items once its folder is the one listed. Items that only
/// hidden files would show turn hidden files on (for this session; the saved preference
/// changes only when the user toggles it).
fn apply_pending_reveal(s: &mut State) {
    let Some(reveal) = s.pending_reveal.take() else {
        return;
    };
    if reveal.folder != s.cwd {
        return;
    }
    let listed =
        |s: &State, item: &Location| *item == s.cwd || s.entries.iter().any(|e| &e.path == item);
    let mut showed_hidden = false;
    if !s.show_hidden
        && reveal
            .select
            .iter()
            .any(|item| !listed(s, item) && item.name().starts_with('.'))
    {
        s.show_hidden = true;
        showed_hidden = true;
        refresh_view(s);
    }
    let items: Vec<Location> = reveal
        .select
        .iter()
        .filter(|item| listed(s, item))
        .cloned()
        .collect();
    if let Some(first) = items.first() {
        select_path(s, first);
        if items.len() > 1 {
            s.selected_paths = items.clone();
            s.status = format!("{} items selected", items.len());
        }
        if *first != s.cwd {
            s.reveal_scroll = Some(first.key());
        }
        s.reveal_properties = reveal.properties;
    } else {
        s.status = format!("Showing {}", format_location(&s.cwd));
    }
    if showed_hidden {
        s.status = format!("{} · showing hidden files", s.status);
    }
    let missing = reveal.select.len() - items.len();
    if missing > 0 {
        s.status = format!("{} · {missing} item(s) not found", s.status);
    }
}

/// The parts of a FileManager1 request that need the window: raise it, scroll the selection
/// into view, then show properties.
fn finish_reveal(s: &mut State, tx: &mut Tx) {
    if let Some(token) = s.activation.take()
        && let Some(app) = APP.get()
    {
        app.activate(token);
    }
    if let Some(key) = s.reveal_scroll.take() {
        tx.scroll_into_view(key);
    }
    if std::mem::take(&mut s.reveal_properties) {
        context_action(s, Some("properties"));
    }
}

fn poll_operation(s: &mut State, tx: &mut Tx) -> bool {
    let Some(receiver) = &s.operation_rx else {
        return false;
    };
    let mut changed = false;
    // How it ended and how many items it got through.
    let mut ended: Option<(notify::Outcome, usize)> = None;
    while ended.is_none() {
        match receiver.try_recv() {
            Ok(OperationEvent::Progress(done, total, name, verb)) => {
                s.operation = Some(format!("{verb} {done}/{total}: {name}"));
                s.status = s.operation.clone().unwrap_or_default();
                changed = true;
            }
            Ok(OperationEvent::Waited(waited)) => {
                if let Some(info) = &mut s.operation_info {
                    info.waited += waited;
                }
            }
            Ok(OperationEvent::Finished(done)) => ended = Some((notify::Outcome::Finished, done)),
            Ok(OperationEvent::Cancelled(done)) => ended = Some((notify::Outcome::Cancelled, done)),
            Ok(OperationEvent::Failed(done, error)) => {
                ended = Some((notify::Outcome::Failed(error), done));
            }
            Err(mpsc::TryRecvError::Empty) => break,
            // The worker died without saying how it ended.
            Err(mpsc::TryRecvError::Disconnected) => {
                let error = "the operation stopped unexpectedly".to_owned();
                ended = Some((notify::Outcome::Failed(error), 0));
            }
        }
    }
    let Some((outcome, done)) = ended else {
        tx.wake_after(Duration::from_millis(100));
        return changed;
    };
    let info = s.operation_info.take();
    let kind = info.as_ref().map_or(notify::Kind::Copy, |info| info.kind);
    s.status = match (&outcome, kind) {
        (notify::Outcome::Finished, notify::Kind::Duplicate) => {
            format!("Duplicated {done} item(s)")
        }
        (notify::Outcome::Finished, _) => format!("Pasted {done} item(s)"),
        (notify::Outcome::Cancelled, _) => format!("Operation cancelled after {done} item(s)"),
        (notify::Outcome::Failed(error), notify::Kind::Duplicate) => {
            format!("Duplicate failed: {error}")
        }
        (notify::Outcome::Failed(error), _) => format!("File operation failed: {error}"),
    };
    s.operation = None;
    s.operation_rx = None;
    s.operation_cancel = None;
    // Only a cut's paste consumes the clipboard.
    if kind == notify::Kind::Move && s.clipboard_cut {
        if outcome == notify::Outcome::Finished {
            s.clipboard.clear();
            s.clipboard_cut = false;
        } else if done > 0 {
            s.clipboard.drain(..done.min(s.clipboard.len()));
        }
    }
    if let Some(info) = info {
        let report = notify::Report {
            kind: info.kind,
            outcome,
            done,
            total: info.total,
            item: info.item,
            destination: info.destination,
            active: info.started.elapsed().saturating_sub(info.waited),
            focused: s.window_focused,
        };
        if let Some(notice) = notify::notice(&report) {
            s.notifier.show(notice);
        }
    }
    read_directory(s);
    true
}

fn prompt_name(title: &str, initial: &str) -> Option<String> {
    let output = Command::new("zenity")
        .args([
            "--entry",
            &format!("--title={title}"),
            "--text=Name:",
            "--entry-text",
        ])
        .arg(initial)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|name| !name.is_empty())
}

fn begin_rename(s: &mut State, path: &Path) {
    if path == trash_path() || path.starts_with(trash_path()) {
        s.status = "Restore an item before renaming it".into();
        return;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    s.rename_edit.text = name.into_owned();
    let end = s.rename_edit.text.len();
    s.rename_edit.anchor = 0;
    s.rename_edit.focus = end;
    s.rename_edit.focused = true;
    s.renaming = Some(path.to_path_buf());
}

fn cancel_rename(s: &mut State) {
    s.renaming = None;
    s.rename_edit.focused = false;
}

fn commit_rename(s: &mut State) {
    let Some(source) = s.renaming.take() else {
        return;
    };
    s.rename_edit.focused = false;
    let name = s.rename_edit.text.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        s.status = "Choose a valid file name".into();
        s.renaming = Some(source);
        return;
    }
    let destination = source.with_file_name(name);
    if destination == source {
        s.status = "Name unchanged".into();
        read_directory(s);
        return;
    }
    if destination.symlink_metadata().is_ok() {
        s.status = "An item with that name already exists".into();
        s.renaming = Some(source);
        return;
    }
    let mut renamed_path = None;
    match fs::rename(&source, &destination) {
        Ok(()) => {
            for favorite in &mut s.favorites {
                if let Location::Local(favorite) = favorite
                    && let Ok(relative) = favorite.strip_prefix(&source)
                {
                    *favorite = destination.join(relative);
                }
            }
            save_favorites(&s.favorites);
            renamed_path = Some(destination);
            s.status = "Renamed".into();
        }
        Err(error) => {
            s.status = format!("Rename failed: {error}");
            s.renaming = Some(source);
        }
    }
    read_directory(s);
    if let Some(path) = renamed_path {
        select_path(s, &Location::Local(path));
    }
}

fn create_folder(s: &mut State) {
    let Location::Local(cwd) = s.cwd.clone() else {
        ensure_local_cwd(s);
        return;
    };
    let Some(name) = prompt_name("New Folder", "") else {
        return;
    };
    if name.contains('/') || name == "." || name == ".." {
        s.status = "Names can't contain “/”".into();
        return;
    }
    let path = cwd.join(name);
    s.status = match fs::create_dir(&path) {
        Ok(()) => "Folder created".into(),
        Err(error) => format!("Couldn't create folder: {error}"),
    };
    read_directory(s);
}

fn confirm(title: &str, message: &str, ok: &str) -> bool {
    Command::new("zenity")
        .args([
            "--question",
            &format!("--title={title}"),
            &format!("--text={message}"),
            &format!("--ok-label={ok}"),
            "--cancel-label=Cancel",
        ])
        .status()
        .is_ok_and(|status| status.success())
}

fn empty_trash(s: &mut State) {
    if !confirm(
        "Empty Trash",
        "Permanently delete all items in the Trash? This can't be undone.",
        "Empty Trash",
    ) {
        return;
    }
    let emptied = Command::new("gio")
        .args(["trash", "--empty"])
        .status()
        .is_ok_and(|status| status.success());
    s.status = if emptied {
        s.undo_trash.clear();
        "Trash emptied".into()
    } else {
        "Couldn't empty Trash".into()
    };
    clear_selection(s);
    read_directory(s);
}

fn clear_selection(s: &mut State) {
    s.selected = None;
    s.selected_paths.clear();
    s.selection_anchor = None;
    s.selected_image = None;
    clear_exif(s);
    s.preview = "Choose a file to preview it here.".into();
}

fn clear_exif(s: &mut State) {
    s.exif_generation = s.exif_generation.wrapping_add(1);
    s.exif_details = None;
}

/// Shows `path` in the active tab without touching history (tab switches, back/forward).
fn show_directory(s: &mut State, path: Location) {
    s.cwd = path;
    s.search = rune::TextEdit::new("");
    s.editing_location = false;
    if let Some(tab) = s.tabs.get_mut(s.active_tab) {
        *tab = s.cwd.clone();
    }
    clear_selection(s);
    read_directory(s);
}

fn go_back(s: &mut State) {
    if s.history_index > 0 {
        s.history_index -= 1;
        show_directory(s, s.history[s.history_index].clone());
    }
}

fn go_forward(s: &mut State) {
    if s.history_index + 1 < s.history.len() {
        s.history_index += 1;
        show_directory(s, s.history[s.history_index].clone());
    }
}

fn open_in_new_tab(s: &mut State, path: &Location) {
    let Some(path) = folder_target(s, path) else {
        return;
    };
    s.tabs.push(path.clone());
    s.active_tab = s.tabs.len() - 1;
    navigate(s, path);
}

fn close_active_tab(s: &mut State) {
    close_tab(s, s.active_tab);
}

fn close_tab(s: &mut State, index: usize) {
    if s.tabs.len() <= 1 || index >= s.tabs.len() {
        return;
    }
    s.tabs.remove(index);
    if index < s.active_tab {
        s.active_tab -= 1;
    } else if index == s.active_tab {
        s.active_tab = index.min(s.tabs.len() - 1);
    }
    show_directory(s, s.tabs[s.active_tab].clone());
}

fn switch_tab(s: &mut State, index: usize) {
    if index >= s.tabs.len() || index == s.active_tab {
        return;
    }
    s.active_tab = index;
    show_directory(s, s.tabs[index].clone());
}

fn toggle_hidden(s: &mut State) {
    s.show_hidden = !s.show_hidden;
    s.status = if s.show_hidden {
        "Showing hidden files".into()
    } else {
        "Hiding hidden files".into()
    };
    refresh_view(s);
    save_settings(s);
}

fn set_sort(s: &mut State, sort: SortBy) {
    if s.sort == sort {
        s.ascending = !s.ascending;
    } else {
        s.sort = sort;
        s.ascending = true;
    }
    refresh_view(s);
    save_settings(s);
}

fn navigate(s: &mut State, path: Location) {
    let path = match &path {
        Location::Local(local) => {
            if !local.is_dir() && *local != trash_path() {
                return;
            }
            path
        }
        Location::Remote(_) => match folder_target(s, &path) {
            Some(target) => target,
            None => return,
        },
    };
    s.cwd = path.clone();
    if let Some(tab) = s.tabs.get_mut(s.active_tab) {
        *tab = path.clone();
    } else {
        s.tabs.push(path.clone());
        s.active_tab = s.tabs.len() - 1;
    }
    clear_selection(s);
    s.search = rune::TextEdit::new("");
    s.editing_location = false;
    if s.history.get(s.history_index) != Some(&path) {
        s.history.truncate(s.history_index + 1);
        s.history.push(path);
        s.history_index = s.history.len() - 1;
    }
    read_directory(s);
    refresh_mounts(s);
}

fn submit_location(s: &mut State) {
    let input = s.location_edit.text.trim().to_owned();
    let path = match Location::parse_input(&input, &home()) {
        Some(Location::Local(path)) => path,
        Some(remote) => {
            // Whether a remote URI exists is only known once gio lists it; errors show in
            // the status bar and the file list.
            navigate(s, remote);
            s.editing_location = false;
            return;
        }
        None => {
            s.status = "Only local file:// URIs are supported (file://host/… isn't)".into();
            return;
        }
    };
    if path.is_dir() {
        navigate(s, Location::Local(path));
        s.editing_location = false;
    } else if path.is_file() {
        open(&path);
        s.editing_location = false;
    } else {
        s.status = "Location does not exist".into();
    }
}

fn select_path(s: &mut State, location: &Location) {
    s.selected = Some(location.clone());
    s.selected_paths = vec![location.clone()];
    s.selection_anchor = Some(location.clone());
    let Location::Local(path) = location else {
        select_remote(s, location);
        return;
    };
    if path.is_dir() {
        clear_exif(s);
        s.preview = "Double-click or press Enter to open this folder.".into();
        s.selected_image = None;
        s.status = "Folder selected".into();
    } else {
        refresh_preview(s, location);
        s.status = format!(
            "{} selected",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }
}

/// Rebuilds a local file's preview from its current type.
fn refresh_preview(s: &mut State, location: &Location) {
    let Location::Local(path) = location else {
        return;
    };
    if path.is_dir() {
        return;
    }
    let kind = local_file_type(s, path);
    clear_exif(s);
    s.preview = preview_file(path, kind);
    s.selected_image = if kind == FileType::Image {
        s.image_cache
            .as_ref()
            .map(|cache| cache.file(path, 240.0 * s.zoom, 1.0))
    } else {
        None
    };
    if matches!(kind, FileType::Image | FileType::OtherImage) {
        let mtime = s
            .entries
            .iter()
            .find(|entry| entry.path.as_local() == Some(path))
            .map_or(0, |entry| entry.modified_secs);
        request_exif(s, path, mtime);
    }
}

/// Remote files show metadata only: previews would read file contents over the network.
fn select_remote(s: &mut State, location: &Location) {
    s.selected_image = None;
    clear_exif(s);
    if location_is_dir(s, location) {
        s.preview = "Double-click or press Enter to open this folder.".into();
        s.status = "Folder selected".into();
        return;
    }
    let entry = entry_for(s, location);
    s.preview = format!(
        "Type: {}\nSize: {}\nModified: {}\nLocation: {}\n\nPreviews aren't loaded from remote locations. Use Open to view the file in its default application.",
        entry
            .and_then(|entry| entry.content_type.clone())
            .unwrap_or_else(|| "Unknown".into()),
        entry.map_or_else(|| "—".into(), |entry| human_size(entry.size)),
        entry.map_or_else(|| "—".into(), |entry| entry.modified.clone()),
        location.display_uri(),
    );
    s.status = format!("{} selected", location.name());
}

fn select_with_modifiers(s: &mut State, path: &Location, mods: rune::Mods) {
    let visible = s
        .entries
        .iter()
        .filter(|entry| {
            entry
                .name
                .to_lowercase()
                .contains(&s.search.text.to_lowercase())
        })
        .collect::<Vec<_>>();
    if mods.shift {
        let anchor = s
            .selection_anchor
            .as_ref()
            .and_then(|anchor| visible.iter().position(|entry| &entry.path == anchor));
        let target = visible.iter().position(|entry| &entry.path == path);
        if let (Some(anchor), Some(target)) = (anchor, target) {
            let range = visible[anchor.min(target)..=anchor.max(target)]
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>();
            if mods.ctrl {
                for item in range {
                    if !s.selected_paths.contains(&item) {
                        s.selected_paths.push(item);
                    }
                }
            } else {
                s.selected_paths = range;
            }
        } else {
            s.selected_paths = vec![path.clone()];
            s.selection_anchor = Some(path.clone());
        }
    } else if mods.ctrl {
        if let Some(index) = s.selected_paths.iter().position(|item| item == path) {
            s.selected_paths.remove(index);
        } else {
            s.selected_paths.push(path.clone());
        }
        s.selection_anchor = Some(path.clone());
    } else {
        s.selected_paths = vec![path.clone()];
        s.selection_anchor = Some(path.clone());
    }
    s.selected = s.selected_paths.last().cloned();
    if s.selected_paths.len() == 1 {
        if let Some(selected) = s.selected.clone() {
            select_path(s, &selected);
        }
    } else {
        s.selected_image = None;
        clear_exif(s);
        s.preview = format!("{} items selected.", s.selected_paths.len());
        s.status = format!("{} items selected", s.selected_paths.len());
    }
}

fn activate_path(s: &mut State, path: &Location) {
    if let Some(folder) = folder_target(s, path) {
        navigate(s, folder);
    } else {
        open_location(s, path);
    }
}

fn trash_path() -> PathBuf {
    home().join(".local/share/Trash/files")
}

fn restore_from_trash(path: &Path) -> std::io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let info = home()
        .join(".local/share/Trash/info")
        .join(format!("{name}.trashinfo"));
    let original = original_path_from_trash_info(&info)
        .ok_or_else(|| std::io::Error::other("Invalid original path in Trash info"))?;
    if original.symlink_metadata().is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} already exists", format_path(&original)),
        ));
    }
    if let Some(parent) = original.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(path, &original).or_else(|_| {
        copy_path(path, &original)?;
        remove_path(path)
    })?;
    let _ = fs::remove_file(info);
    Ok(())
}

fn original_path_from_trash_info(info: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(info).ok()?;
    let encoded = contents
        .lines()
        .find_map(|line| line.strip_prefix("Path="))?;
    let uri = if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    };
    local_file_uri(&uri)
}

/// The newest Trash entry whose original location is `original`.
fn find_trashed(original: &Path) -> Option<PathBuf> {
    let info_dir = home().join(".local/share/Trash/info");
    fs::read_dir(info_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| original_path_from_trash_info(&entry.path()).as_deref() == Some(original))
        .max_by_key(|entry| {
            fs::read_to_string(entry.path())
                .ok()
                .and_then(|contents| {
                    contents
                        .lines()
                        .find_map(|line| line.strip_prefix("DeletionDate="))
                        .map(str::to_owned)
                })
                .unwrap_or_default()
        })
        .and_then(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            Some(trash_path().join(name.strip_suffix(".trashinfo")?))
        })
}

fn undo_last_trash(s: &mut State) {
    let Some(batch) = s.undo_trash.pop() else {
        s.status = "Nothing to undo".into();
        return;
    };
    let mut restored = 0;
    let mut failure = None;
    for original in &batch {
        match find_trashed(original).map(|path| restore_from_trash(&path)) {
            Some(Ok(())) => restored += 1,
            Some(Err(error)) => failure = Some(error.to_string()),
            None => failure = Some("item is no longer in the Trash".into()),
        }
    }
    s.status = match failure {
        None => format!("Undo: restored {restored} item(s) from Trash"),
        Some(error) => format!("Undo restored {restored}/{} item(s): {error}", batch.len()),
    };
    clear_selection(s);
    read_directory(s);
}

fn context_items(s: &State) -> Vec<(&'static str, &'static str, bool)> {
    let single = s.selected_paths.len() <= 1;
    let dir = s
        .selected
        .as_ref()
        .is_some_and(|path| folder_target(s, path).is_some());
    let favorite = s
        .selected
        .as_ref()
        .is_some_and(|path| s.favorites.contains(path));
    let in_trash = is_trash(&s.cwd);
    // Remote folders are browse-only: everything below that mutates goes through std::fs.
    let local = !s.cwd.is_remote();
    vec![
        ("open", "Open", single),
        ("open-tab", "Open in New Tab", single && dir),
        ("", "", false),
        (
            "favorite-add",
            "Add to Favorites",
            single && dir && !favorite,
        ),
        (
            "favorite-remove",
            "Remove from Favorites",
            single && favorite,
        ),
        ("", "", false),
        ("copy", "Copy", local),
        ("cut", "Cut", local && !in_trash),
        ("paste", "Paste", local && !in_trash),
        ("duplicate", "Duplicate", local && !in_trash),
        ("", "", false),
        ("copy-path", "Copy Path", single),
        ("rename", "Rename", local && single && !in_trash),
        ("trash", "Move to Trash", local && !in_trash),
        ("restore", "Restore from Trash", in_trash),
        ("delete", "Delete Permanently", local),
        ("properties", "Properties", single),
    ]
}

fn context_action(s: &mut State, action: Option<&str>) {
    let Some(location) = s.selected.clone() else {
        return;
    };
    let local_only = matches!(
        action,
        Some("copy" | "cut" | "paste" | "duplicate" | "rename" | "trash" | "restore" | "delete")
    );
    if local_only && !ensure_local_cwd(s) {
        return;
    }
    // Local-only actions below use `path`; remote locations never reach them.
    let path = location
        .as_local()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    match action {
        Some("open") => activate_path(s, &location),
        Some("open-tab") => open_in_new_tab(s, &location),
        Some("favorite-add") if folder_target(s, &location).is_some() => {
            let path = folder_target(s, &location).unwrap_or(location);
            if !s.favorites.contains(&path) {
                s.favorites.push(path.clone());
                save_favorites(&s.favorites);
            }
            s.status = "Added to Favorites".into();
        }
        Some("favorite-remove") => {
            s.favorites.retain(|favorite| favorite != &location);
            save_favorites(&s.favorites);
            s.status = "Removed from Favorites".into();
        }
        Some("copy") => {
            s.clipboard = local_paths(&s.selected_paths);
            s.clipboard_cut = false;
            write_file_clipboard(&s.clipboard, false);
            s.status = format!("Copied {} item(s)", s.clipboard.len());
        }
        Some("cut") => {
            s.clipboard = local_paths(&s.selected_paths);
            s.clipboard_cut = true;
            write_file_clipboard(&s.clipboard, true);
            s.status = format!("Cut {} item(s)", s.clipboard.len());
        }
        Some("paste") => transfer_clipboard(s),
        Some("duplicate") => duplicate_selection(s),
        Some("trash") => {
            let mut batch = Vec::new();
            for item in local_paths(&s.selected_paths) {
                if Command::new("gio")
                    .args(["trash", "--"])
                    .arg(&item)
                    .status()
                    .is_ok_and(|status| status.success())
                {
                    batch.push(item);
                }
            }
            s.status = format!("Moved {} item(s) to Trash · Ctrl+Z to undo", batch.len());
            if !batch.is_empty() {
                s.undo_trash.push(batch);
            }
            clear_selection(s);
            read_directory(s);
        }
        Some("restore") => {
            let items = local_paths(&s.selected_paths);
            let mut restored = 0;
            let mut failure = None;
            for item in &items {
                match restore_from_trash(item) {
                    Ok(()) => restored += 1,
                    Err(error) => failure = Some(error.to_string()),
                }
            }
            s.status = match failure {
                None => format!("Restored {restored} item(s) from Trash"),
                Some(error) => format!("Restored {restored}/{} item(s): {error}", items.len()),
            };
            clear_selection(s);
            read_directory(s);
        }
        Some("delete") => {
            let items = local_paths(&s.selected_paths);
            let message = if items.len() == 1 {
                format!(
                    "Permanently delete “{}”? This can't be undone.",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )
            } else {
                format!(
                    "Permanently delete {} items? This can't be undone.",
                    items.len()
                )
            };
            if confirm("Delete Permanently", &message, "Delete") {
                let in_trash = is_trash(&s.cwd);
                let mut deleted = 0;
                s.status = String::new();
                for item in &items {
                    if let Err(error) = remove_path(item) {
                        s.status = format!("Delete failed: {error}");
                        break;
                    }
                    deleted += 1;
                    if in_trash {
                        let name = item.file_name().unwrap_or_default().to_string_lossy();
                        let _ = fs::remove_file(
                            home()
                                .join(".local/share/Trash/info")
                                .join(format!("{name}.trashinfo")),
                        );
                    }
                }
                if s.status.is_empty() {
                    s.status = format!("Deleted {deleted} item(s) permanently");
                }
                clear_selection(s);
                read_directory(s);
            }
        }
        Some("copy-path") => {
            // Remote items copy their URI, which other GIO applications accept.
            let value = location.key();
            let copied = Command::new("wl-copy")
                .arg(&value)
                .status()
                .is_ok_and(|status| status.success());
            s.status = if copied {
                "Path copied to clipboard".into()
            } else {
                format!("Clipboard unavailable · {}", format_location(&location))
            };
        }
        Some("properties") if location.is_remote() => {
            let entry = entry_for(s, &location);
            let details = format!(
                "Name: {}\nLocation: {}\nType: {}\nSize: {}\nModified: {}",
                entry.map_or_else(|| location.name(), |entry| entry.name.clone()),
                location
                    .parent()
                    .map_or_else(|| location.display_uri(), |parent| parent.display_uri()),
                if location_is_dir(s, &location) {
                    "Folder".to_owned()
                } else {
                    entry
                        .and_then(|entry| entry.content_type.clone())
                        .unwrap_or_else(|| "Unknown".into())
                },
                entry
                    .filter(|entry| !entry.dir)
                    .map_or_else(|| "—".into(), |entry| human_size(entry.size)),
                entry.map_or_else(|| "—".into(), |entry| entry.modified.clone()),
            );
            show_properties(s, details);
        }
        Some("rename") => {
            begin_rename(s, &path);
        }
        Some("properties") => {
            let metadata = fs::metadata(&path).ok();
            let details = format!(
                "Name: {}\nLocation: {}\nType: {}\nSize: {}\nModified: {}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                path.parent().unwrap_or_else(|| Path::new("/")).display(),
                if path.is_dir() {
                    "Folder".to_owned()
                } else {
                    let kind = local_file_type(s, &path);
                    s.entries
                        .iter()
                        .find(|entry| entry.path.as_local() == Some(path.as_path()))
                        .and_then(|entry| entry.content_type.clone())
                        .filter(|mime| !mime::is_unknown(mime))
                        .map_or_else(
                            || kind.label().to_owned(),
                            |mime| format!("{} ({mime})", kind.label()),
                        )
                },
                metadata
                    .as_ref()
                    .map(|m| human_size(m.len()))
                    .unwrap_or_else(|| "—".into()),
                metadata
                    .and_then(|m| m.modified().ok())
                    .map(|time| format!("{time:?}"))
                    .unwrap_or_else(|| "—".into())
            );
            show_properties(s, details);
        }
        _ => {}
    }
}

/// Shows `details` in a zenity dialog without blocking the window (a FileManager1 caller
/// is waiting for the window to come up), or in the status bar without zenity.
fn show_properties(s: &mut State, details: String) {
    let dialog = Command::new("zenity")
        .args(["--info", "--title=Properties", "--text"])
        .arg(&details)
        .stdin(Stdio::null())
        .spawn();
    s.status = match dialog {
        Ok(mut dialog) => {
            std::thread::spawn(move || dialog.wait());
            "Properties".into()
        }
        Err(_) => details.replace('\n', " · "),
    };
}

fn view(s: &State, cx: &mut Cx<State>) -> El {
    let t = tok::theme(cx.system_dark());
    let accent = tok::accent(0, cx.system_dark());
    let z = |points: f32| points * s.zoom;
    let app_width = cx.layout_size("app-root").map_or(1120.0, |size| size[0]);
    let content_width = (app_width / s.zoom - 217.0).max(0.0);
    let list_min_width = 260.0f32.min((content_width * 0.55).max(120.0));
    let preview_min_width = 220.0f32.min((content_width - list_min_width).max(120.0));
    let preview_max_width = 520.0f32.min((content_width - list_min_width).max(preview_min_width));
    let visible_preview_width = s.preview_width.clamp(preview_min_width, preview_max_width);
    let preview_width = z(visible_preview_width);
    let list_width = cx
        .layout_size("file-list")
        .map(|size| size[0])
        .unwrap_or((app_width - z(217.0) - preview_width).max(0.0));
    let show_modified = list_width >= z(420.0);
    let show_size = list_width >= z(300.0);
    let crumbs = match &s.cwd {
        Location::Local(cwd) => cwd
            .components()
            .enumerate()
            .map(|(index, c)| {
                (
                    c.as_os_str().to_string_lossy().into_owned(),
                    Location::Local(cwd.components().take(index + 1).collect::<PathBuf>()),
                )
            })
            .collect::<Vec<_>>(),
        Location::Remote(_) => s.cwd.remote_crumbs(),
    };
    let breadcrumb = row()
        .h(z(38.0))
        .gap(z(4.0))
        .center_y()
        .children(crumbs.into_iter().map(|(name, path)| {
            row()
                .h(z(28.0))
                .min_w(0.0)
                .px(z(8.0))
                .center_y()
                .radius(radius::SM)
                .hover(t.hover)
                .child(
                    text(name.clone())
                        .font(scaled_font(type_::BODY, s.zoom))
                        .color(t.text_secondary)
                        .min_w(0.0)
                        .truncate(),
                )
                .on_press(move |s: &mut State| navigate(s, path.clone()))
        }));
    let nav_button = |label: &'static str, enabled: bool, action: fn(&mut State)| {
        row()
            .size(z(32.0), z(32.0))
            .center()
            .radius(radius::SM)
            .hover(t.hover)
            .opacity(if enabled { 1.0 } else { 0.4 })
            .child(
                text(label)
                    .font(scaled_font(type_::BODY, s.zoom))
                    .color(t.text),
            )
            .on_press(move |s: &mut State| action(s))
    };
    let header = row()
        .w_full()
        .h(z(56.0))
        .px(z(18.0))
        .center_y()
        .gap(z(8.0))
        .fill(t.bg)
        .child(nav_button("‹", s.history_index > 0, go_back))
        .child(nav_button(
            "›",
            s.history_index + 1 < s.history.len(),
            go_forward,
        ))
        .child(breadcrumb.grow(1.0).min_w(0.0))
        .child(
            row()
                .h(z(34.0))
                .w(z(220.0))
                .px(z(10.0))
                .center_y()
                .radius(radius::SM)
                .fill(t.card)
                .child(
                    rune::text_input(&s.search, scaled_font(type_::BODY, s.zoom))
                        .key("search")
                        .w(z(220.0))
                        .h(z(34.0))
                        .padding(z(8.0))
                        .radius(radius::SM)
                        .fill(t.card)
                        .color(t.text)
                        .placeholder("⌕  Search this folder", t.text_secondary)
                        .label("Search this folder")
                        .on_edit(|s: &mut State| &mut s.search),
                ),
        )
        .child(nav_button("+", true, create_folder))
        .child(nav_button(
            if s.grid_view { "☰" } else { "▦" },
            true,
            |s| {
                s.grid_view = !s.grid_view;
                save_settings(s);
            },
        ))
        .child(nav_button("◌", true, toggle_hidden))
        .children(is_trash(&s.cwd).then(|| nav_button("Empty", true, empty_trash)))
        .children((!s.undo_trash.is_empty()).then(|| nav_button("Undo", true, undo_last_trash)))
        .children(
            s.operation
                .as_ref()
                .map(|_| nav_button("Cancel", true, cancel_operation)),
        );

    let tabs = row()
        .w_full()
        .h(z(34.0))
        .px(z(14.0))
        .gap(z(4.0))
        .center_y()
        .fill(t.card)
        .children(s.tabs.iter().enumerate().map(|(index, path)| {
            let active = index == s.active_tab;
            let title = path.name();
            row()
                .h(z(28.0))
                .min_w(z(100.0))
                .px(z(10.0))
                .center_y()
                .radius(radius::SM)
                .fill(if active {
                    t.control_track
                } else {
                    Color::TRANSPARENT
                })
                .hover(t.hover)
                .child(
                    text(title)
                        .font(scaled_font(type_::BODY, s.zoom))
                        .color(t.text)
                        .grow(1.0),
                )
                .child(
                    text("×")
                        .font(scaled_font(type_::BODY, s.zoom))
                        .color(t.text_secondary)
                        .on_press(move |s: &mut State| close_tab(s, index)),
                )
                .on_press(move |s: &mut State| switch_tab(s, index))
        }))
        .child(nav_button("+", true, |s| {
            let cwd = s.cwd.clone();
            open_in_new_tab(s, &cwd);
        }));

    let location_bar = if s.editing_location {
        rune::text_input(&s.location_edit, scaled_font(type_::BODY, s.zoom))
            .key("location")
            .w_full()
            .h(z(38.0))
            .px(z(20.0))
            .padding(z(6.0))
            .fill(t.bg)
            .color(t.text)
            .placeholder(
                "Enter a path or a URI such as sftp://host/",
                t.text_secondary,
            )
            .label("Location")
            .on_edit(|s: &mut State| &mut s.location_edit)
            .on_key(|s: &mut State, event: &KeyEvent, _| {
                if event.key == Key::Enter {
                    submit_location(s);
                    true
                } else {
                    false
                }
            })
    } else {
        row()
            .w_full()
            .h(z(38.0))
            .px(z(20.0))
            .center_y()
            .fill(t.bg)
            .child(
                text(format_location(&s.cwd))
                    .font(scaled_font(type_::LABEL, s.zoom))
                    .color(t.text_secondary),
            )
            .on_press(|s: &mut State| {
                s.location_edit.text = s.cwd.key();
                let end = s.location_edit.text.len();
                s.location_edit.anchor = end;
                s.location_edit.focus = end;
                s.location_edit.focused = true;
                s.editing_location = true;
            })
    };

    let side_item = |label: String, path: Location, active: bool| {
        let is_favorite = s.favorites.contains(&path);
        let star_path = path.clone();
        let can_toggle = label != "Home" && label != "Trash" && label != "Network";
        let leading = if path.is_remote() {
            text("◎")
                .font(scaled_font(type_::BODY, s.zoom))
                .color(if active { accent } else { t.text_secondary })
        } else if label == "Home" {
            text("⌂")
                .font(scaled_font(type_::BODY, s.zoom))
                .color(if active { accent } else { t.text_secondary })
        } else if label == "Trash" {
            text("▱")
                .font(scaled_font(type_::BODY, s.zoom))
                .color(if active { accent } else { t.text_secondary })
        } else {
            file_icon(s, IconKind::Folder, z(16.0), accent)
        };
        row()
            .key(path.key())
            .w_full()
            .h(z(34.0))
            .px(z(10.0))
            .gap(z(10.0))
            .center_y()
            .radius(radius::SM)
            .fill(if active {
                t.control_track
            } else {
                Color::TRANSPARENT
            })
            .hover(t.hover)
            .child(row().w(z(16.0)).center().child(leading))
            .child(
                text(label)
                    .font(scaled_font(type_::BODY, s.zoom))
                    .color(t.text)
                    .grow(1.0)
                    .min_w(0.0)
                    .truncate(),
            )
            .child(if can_toggle {
                row()
                    .size(z(24.0), z(24.0))
                    .center()
                    .radius(radius::SM)
                    .hover(t.control_track)
                    .child(
                        text(if is_favorite { "★" } else { "☆" })
                            .font(scaled_font(type_::BODY, s.zoom))
                            .color(if is_favorite {
                                accent
                            } else {
                                t.text_secondary
                            }),
                    )
                    .on_press(move |s: &mut State| toggle_favorite(s, &star_path))
            } else {
                el().size(z(24.0), z(24.0))
            })
            .on_press(move |s: &mut State| navigate(s, path.clone()))
    };
    // Removable drives, mounted or not. Click opens (mounting first); the inline button
    // unmounts; right-click offers Mount, Unmount, Eject and Power Off.
    let device_row = |volume: &udisks::Volume| {
        let busy = volume_busy(s, volume);
        let mount_point = volume.mount_points.first().cloned();
        let active = mount_point
            .as_ref()
            .is_some_and(|point| s.cwd == Location::Local(point.clone()));
        let detail = match (busy, &mount_point) {
            (Some(verb), _) => format!("{verb}…"),
            (None, Some(_)) => volume.drive_name.clone(),
            (None, None) => format!("{} · not mounted", volume.drive_name),
        };
        let open_object = volume.object.clone();
        let menu_object = volume.object.clone();
        let unmount_object = volume.object.clone();
        row()
            .key(format!("device:{}", volume.object))
            .w_full()
            .h(z(42.0))
            .px(z(10.0))
            .gap(z(10.0))
            .center_y()
            .radius(radius::SM)
            .fill(if active {
                t.control_track
            } else {
                Color::TRANSPARENT
            })
            .hover(t.hover)
            .opacity(if busy.is_some() { 0.6 } else { 1.0 })
            .child(
                row().w(z(16.0)).center().child(
                    text(if volume.is_mounted() { "▣" } else { "▢" })
                        .font(scaled_font(type_::BODY, s.zoom))
                        .color(if active { accent } else { t.text_secondary }),
                ),
            )
            .child(
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .child(
                        text(volume_name(volume))
                            .font(scaled_font(type_::BODY, s.zoom))
                            .color(t.text)
                            .min_w(0.0)
                            .truncate(),
                    )
                    .child(
                        text(detail)
                            .font(scaled_font(type_::LABEL, s.zoom))
                            .color(t.text_secondary)
                            .min_w(0.0)
                            .truncate(),
                    ),
            )
            .child(if volume.is_mounted() && busy.is_none() {
                row()
                    .size(z(24.0), z(24.0))
                    .center()
                    .radius(radius::SM)
                    .hover(t.control_track)
                    .child(
                        text("⏏")
                            .font(scaled_font(type_::BODY, s.zoom))
                            .color(t.text_secondary),
                    )
                    .label("Unmount")
                    .on_press(move |s: &mut State| {
                        device_action(s, &unmount_object, DeviceOp::Unmount)
                    })
            } else {
                el().size(z(24.0), z(24.0))
            })
            .on_press(move |s: &mut State| device_action(s, &open_object, DeviceOp::Open))
            .on_secondary_press(move |s: &mut State, press: PointerPress, _tx: &mut Tx| {
                s.context_menu = None;
                s.device_menu = Some((press.x, press.y, menu_object.clone()));
            })
    };
    let device_error = s
        .volumes_error
        .as_ref()
        .filter(|error| !error.contains("isn't installed"));
    let device_section = (!s.volumes.is_empty() || device_error.is_some()).then(|| {
        col()
            .w_full()
            .gap(z(3.0))
            .child(
                text("DEVICES")
                    .font(scaled_font(type_::LABEL, s.zoom))
                    .color(t.text_secondary)
                    .padding(z(8.0)),
            )
            .children(s.volumes.iter().map(device_row))
            .children(device_error.map(|error| {
                text(format!("Couldn't list drives: {error}"))
                    .font(scaled_font(type_::LABEL, s.zoom))
                    .color(t.text_secondary)
                    .w_full()
                    .min_w(0.0)
                    .px(z(8.0))
                    .wrap()
            }))
            .child(el().h(z(6.0)))
    });
    let sidebar = col()
        .key("sidebar")
        .w(z(210.0))
        .h_full()
        .padding(z(12.0))
        .gap(z(3.0))
        .fill(t.card)
        .child(
            text("PLACES")
                .font(scaled_font(type_::LABEL, s.zoom))
                .color(t.text_secondary)
                .padding(z(8.0)),
        )
        .child(side_item(
            "Home".into(),
            Location::Local(home()),
            s.cwd == Location::Local(home()),
        ))
        .children(
            s.favorites
                .iter()
                .filter(|location| match location {
                    // Remote favorites can't be checked without the network; always list them.
                    Location::Local(p) => p.exists() && *p != home(),
                    Location::Remote(_) => true,
                })
                .map(|path| {
                    let label = match path {
                        Location::Local(local) => local
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        Location::Remote(_) => path.name(),
                    };
                    side_item(label, path.clone(), s.cwd == *path)
                }),
        )
        .child(el().w_full().h(z(1.0)).fill(t.separator))
        .child(el().h(z(8.0)))
        .children(
            s.mounts
                .iter()
                .filter_map(|mount| Some((mount.name.clone(), Location::from_uri(&mount.uri)?)))
                .filter(|(_, location)| !s.favorites.contains(location))
                .map(|(name, location)| {
                    let active = s.cwd == location;
                    side_item(name, location, active)
                }),
        )
        .children(device_section)
        .child({
            let network = Location::Remote("network:///".into());
            let active = s.cwd == network;
            side_item("Network".into(), network, active)
        })
        .child(side_item(
            "Trash".into(),
            Location::Local(trash_path()),
            is_trash(&s.cwd),
        ))
        .child(el().grow(1.0))
        .on_drop_files(|s: &mut State, paths, _| {
            let mut added = 0;
            for path in paths.iter().filter(|path| path.is_dir()) {
                let location = Location::Local(path.clone());
                if !s.favorites.contains(&location) {
                    s.favorites.push(location);
                    added += 1;
                }
            }
            save_favorites(&s.favorites);
            s.status = format!("Added {added} folder(s) to Favorites");
        });

    let filtered = s.entries.iter().filter(|e| {
        e.name
            .to_lowercase()
            .contains(&s.search.text.to_lowercase())
    });
    let entries = filtered
        .map(|entry| {
            let path = entry.path.clone();
            let selected = s.selected_paths.contains(&entry.path);
            let kind = entry_icon_kind(entry);
            let size = if entry.dir {
                "—".into()
            } else {
                human_size(entry.size)
            };
            let rename_control = (s.renaming.is_some()
                && s.renaming.as_deref() == entry.path.as_local())
            .then(|| {
                rune::text_input(&s.rename_edit, scaled_font(type_::BODY, s.zoom))
                    .key("rename")
                    .w_full()
                    .h(z(32.0))
                    .padding(z(4.0))
                    .fill(t.card)
                    .color(t.text)
                    .label("Rename item")
                    .on_edit(|s: &mut State| &mut s.rename_edit)
                    .on_key(|s: &mut State, event: &KeyEvent, _| match &event.key {
                        Key::Enter => {
                            commit_rename(s);
                            true
                        }
                        Key::Escape => {
                            cancel_rename(s);
                            true
                        }
                        _ => false,
                    })
            });
            let path_click = path.clone();
            let item = if s.grid_view {
                col()
                    .key(path.key())
                    .grow(1.0)
                    .min_w(0.0)
                    .h(z(128.0))
                    .padding(z(10.0))
                    .gap(z(8.0))
                    .center()
                    .radius(radius::SM)
                    .fill(if selected { t.control_track } else { t.card })
                    .hover(t.hover)
                    .child(if entry_thumbnail(s, entry).is_some() {
                        entry_icon(s, entry, kind, z(GRID_THUMBNAIL), accent)
                    } else {
                        file_icon(s, kind, z(36.0), accent)
                    })
                    .child(rename_control.unwrap_or_else(|| {
                        text(entry.name.clone())
                            .font(scaled_font(type_::BODY, s.zoom))
                            .color(t.text)
                            .w_full()
                            .min_w(0.0)
                            .wrap()
                    }))
            } else {
                row()
                    .key(path.key())
                    .w_full()
                    .h(z(42.0))
                    .px(z(12.0))
                    .gap(z(10.0))
                    .center_y()
                    .radius(radius::SM)
                    .fill(if selected {
                        t.control_track
                    } else {
                        Color::TRANSPARENT
                    })
                    .hover(t.hover)
                    .child(rename_control.unwrap_or_else(|| {
                        row()
                            .grow(1.0)
                            .min_w(0.0)
                            .h_full()
                            .gap(z(8.0))
                            .center_y()
                            .child(entry_icon(s, entry, kind, z(20.0), accent))
                            .child(
                                text(entry.name.clone())
                                    .font(scaled_font(type_::BODY, s.zoom))
                                    .color(if entry.dir { t.text } else { t.text_secondary })
                                    .grow(1.0)
                                    .min_w(0.0)
                                    .truncate(),
                            )
                    }))
                    .children(show_modified.then(|| {
                        text(entry.modified.clone())
                            .font(scaled_font(type_::LABEL, s.zoom))
                            .color(t.text_secondary)
                            .w(z(90.0))
                            .truncate()
                    }))
                    .children(show_size.then(|| {
                        text(size)
                            .font(scaled_font(type_::LABEL, s.zoom))
                            .color(t.text_secondary)
                            .w(z(68.0))
                            .truncate()
                    }))
            };
            item.on_press_tx(move |s: &mut State, tx: &mut Tx| {
                select_with_modifiers(s, &path_click, tx.mods())
            })
            .on_double_press({
                let path = path.clone();
                move |s: &mut State| activate_path(s, &path)
            })
            .on_secondary_press(
                move |s: &mut State, press: PointerPress, _tx: &mut Tx| {
                    if !s.selected_paths.contains(&path) {
                        select_path(s, &path);
                    }
                    s.context_menu = Some((press.x, press.y));
                },
            )
        })
        .collect::<Vec<_>>();
    let entry_body = if s.grid_view {
        let columns = if s.zoom >= 1.3 { 2 } else { 3 };
        let mut rows = col().w_full().gap(z(8.0));
        let mut items = entries.into_iter().peekable();
        while items.peek().is_some() {
            let cells = items
                .by_ref()
                .take(columns)
                .map(|item| item.grow(1.0).min_w(0.0))
                .collect::<Vec<_>>();
            rows = rows.child(row().w_full().gap(z(8.0)).children(cells));
        }
        rows
    } else {
        col().w_full().children(entries)
    };
    let entry_body = match (&s.list_message, s.entries.is_empty()) {
        (Some(message), true) => col().w_full().padding(z(16.0)).child(
            text(message.clone())
                .font(scaled_font(type_::BODY, s.zoom))
                .color(t.text_secondary)
                .w_full()
                .min_w(0.0)
                .wrap(),
        ),
        _ => entry_body,
    };
    let sort_label = |label: &str, sort: SortBy| {
        if s.sort == sort {
            format!("{label} {}", if s.ascending { "▲" } else { "▼" })
        } else {
            label.to_owned()
        }
    };
    let table_header = row()
        .w_full()
        .h(z(32.0))
        .px(z(12.0))
        .gap(z(10.0))
        .center_y()
        .child(
            text(sort_label("NAME", SortBy::Name))
                .font(scaled_font(type_::LABEL, s.zoom))
                .color(t.text_secondary)
                .grow(1.0)
                .on_press(|s: &mut State| set_sort(s, SortBy::Name)),
        )
        .children(show_modified.then(|| {
            text(sort_label("MODIFIED", SortBy::Modified))
                .font(scaled_font(type_::LABEL, s.zoom))
                .color(t.text_secondary)
                .w(z(90.0))
                .on_press(|s: &mut State| set_sort(s, SortBy::Modified))
        }))
        .children(show_size.then(|| {
            text(sort_label("SIZE", SortBy::Size))
                .font(scaled_font(type_::LABEL, s.zoom))
                .color(t.text_secondary)
                .w(z(68.0))
                .on_press(|s: &mut State| set_sort(s, SortBy::Size))
        }));
    let sort_chip = |title: &'static str, sort: SortBy| {
        row()
            .h(z(26.0))
            .px(z(8.0))
            .center_y()
            .radius(radius::SM)
            .fill(if s.sort == sort {
                t.control_track
            } else {
                Color::TRANSPARENT
            })
            .hover(t.hover)
            .child(
                text(sort_label(title, sort))
                    .font(scaled_font(type_::LABEL, s.zoom))
                    .color(if s.sort == sort {
                        t.text
                    } else {
                        t.text_secondary
                    }),
            )
            .on_press(move |s: &mut State| set_sort(s, sort))
    };
    let grid_header = row()
        .w_full()
        .h(z(32.0))
        .px(z(12.0))
        .gap(z(4.0))
        .center_y()
        .child(
            text("SORT")
                .font(scaled_font(type_::LABEL, s.zoom))
                .color(t.text_secondary),
        )
        .child(sort_chip("NAME", SortBy::Name))
        .child(sort_chip("MODIFIED", SortBy::Modified))
        .child(sort_chip("SIZE", SortBy::Size));
    let list_header = Some(if s.grid_view {
        grid_header
    } else {
        table_header
    });
    let list = col()
        .key("file-list")
        .w_full()
        .h_full()
        .padding(z(8.0))
        .children(list_header)
        .child(el().w_full().h(z(1.0)).fill(t.separator))
        .child(entry_body.grow(1.0).min_h(0.0).scroll(rune::ScrollSpec {
            stick_bottom: false,
        }));

    let selected_name = s.selected.as_ref().and_then(|location| match location {
        Location::Local(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
        Location::Remote(_) => {
            Some(entry_for(s, location).map_or_else(|| location.name(), |entry| entry.name.clone()))
        }
    });
    let preview = col()
        .key("preview")
        .w(preview_width)
        .shrink()
        .min_w(z(preview_min_width))
        .h_full()
        .min_h(0.0)
        .padding(z(20.0))
        .gap(z(12.0))
        .fill(t.card)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .child(
            selected_name
                .map(|name| {
                    let kind = s.selected.as_ref().map_or("FILE", |p| {
                        if location_is_dir(s, p) {
                            "FOLDER"
                        } else {
                            match p {
                                Location::Local(path) => local_file_type(s, path).label(),
                                Location::Remote(_) => FileType::of(
                                    entry_for(s, p).and_then(|entry| entry.content_type.as_deref()),
                                    Path::new(&name),
                                )
                                .label(),
                            }
                        }
                    });
                    let preview_icon = s
                        .selected
                        .as_ref()
                        .map_or(IconKind::Generic, |path| location_icon_kind(s, path));
                    let detail = s
                        .selected
                        .as_ref()
                        .map(|p| match p {
                            _ if location_is_dir(s, p) => "Folder".into(),
                            Location::Local(p) => fs::metadata(p)
                                .map(|m| human_size(m.len()))
                                .unwrap_or_else(|_| "—".into()),
                            Location::Remote(_) => entry_for(s, p)
                                .map_or_else(|| "—".into(), |entry| human_size(entry.size)),
                        })
                        .unwrap_or_default();
                    let exif_details = match s.selected.as_ref() {
                        Some(Location::Local(path)) => s
                            .exif_details
                            .as_ref()
                            .filter(|(seen_path, seen_mtime, _)| {
                                seen_path == path
                                    && s.entries.iter().any(|entry| {
                                        entry.path.as_local() == Some(path.as_path())
                                            && entry.modified_secs == *seen_mtime
                                    })
                            })
                            .map(|(_, _, details)| details.clone()),
                        _ => None,
                    };
                    let open_path = s.selected.clone();
                    let favorite_path = s.selected.clone();
                    let is_favorite = favorite_path
                        .as_ref()
                        .is_some_and(|path| s.favorites.contains(path));
                    let image = (!s.image_expanded)
                        .then(|| s.selected_image.as_ref().map(|image| image.pixels()))
                        .flatten();
                    let name_line = if kind == "FOLDER" {
                        row()
                            .w_full()
                            .center_y()
                            .justify_between()
                            .gap(z(8.0))
                            .pr(z(12.0))
                            .child(
                                text(name)
                                    .font(scaled_font(type_::TITLE, s.zoom))
                                    .color(t.text)
                                    .grow(1.0)
                                    .min_w(0.0)
                                    .wrap(),
                            )
                            .children(favorite_path.map(|path| {
                                row()
                                    .size(z(32.0), z(32.0))
                                    .min_w(z(32.0))
                                    .center()
                                    .radius(radius::SM)
                                    .hover(t.hover)
                                    .child(
                                        text(if is_favorite { "★" } else { "☆" })
                                            .font(scaled_font(type_::TITLE, s.zoom))
                                            .color(accent),
                                    )
                                    .on_press(move |s: &mut State| toggle_favorite(s, &path))
                            }))
                    } else {
                        row().w_full().child(
                            text(name)
                                .font(scaled_font(type_::TITLE, s.zoom))
                                .color(t.text)
                                .w_full()
                                .min_w(0.0)
                                .wrap(),
                        )
                    };
                    col()
                        .gap(z(12.0))
                        .child(if let Some(image) = image {
                            el().w_full()
                                .key("file-preview-image")
                                .matched("file-preview-image")
                                .h(z(180.0))
                                .radius(radius::MD)
                                .fill(t.card)
                                .center()
                                .pixels_fit(image, rune::SurfaceFit::Fit)
                                .on_press(|s: &mut State| s.image_expanded = true)
                        } else {
                            el().w_full()
                                .h(z(112.0))
                                .radius(radius::MD)
                                .fill(t.bg)
                                .center()
                                .child(
                                    match s
                                        .selected
                                        .as_ref()
                                        .and_then(|selected| entry_for(s, selected))
                                        .filter(|entry| entry_thumbnail(s, entry).is_some())
                                    {
                                        Some(entry) => {
                                            entry_icon(s, entry, preview_icon, z(96.0), accent)
                                        }
                                        None => file_icon(s, preview_icon, z(48.0), accent),
                                    },
                                )
                        })
                        .child(
                            text(kind)
                                .font(scaled_font(type_::LABEL, s.zoom))
                                .color(accent)
                                .w_full()
                                .min_w(0.0),
                        )
                        .child(name_line)
                        .child(
                            text(detail)
                                .font(scaled_font(type_::BODY, s.zoom))
                                .color(t.text_secondary),
                        )
                        .child(el().w_full().h(z(1.0)).fill(t.separator))
                        .child(
                            text(s.preview.clone())
                                .font(scaled_font(type_::BODY, s.zoom))
                                .color(t.text_secondary)
                                .w_full()
                                .min_w(0.0)
                                .wrap(),
                        )
                        .children(exif_details.map(|details| {
                            col()
                                .w_full()
                                .gap(z(6.0))
                                .child(
                                    text("EXIF")
                                        .font(scaled_font(type_::LABEL, s.zoom))
                                        .color(t.text_secondary),
                                )
                                .child(
                                    text(details)
                                        .font(scaled_font(type_::BODY, s.zoom))
                                        .color(t.text_secondary)
                                        .w_full()
                                        .min_w(0.0)
                                        .wrap(),
                                )
                        }))
                        .child(
                            row()
                                .w_full()
                                .h(z(36.0))
                                .center()
                                .radius(radius::SM)
                                .fill(accent)
                                .child(
                                    text(if kind == "FOLDER" {
                                        "Open Folder"
                                    } else {
                                        "Open"
                                    })
                                    .font(scaled_font(type_::BODY, s.zoom))
                                    .color(Color::WHITE),
                                )
                                .on_press(move |s: &mut State| {
                                    if let Some(path) = &open_path {
                                        activate_path(s, path);
                                    }
                                }),
                        )
                })
                .unwrap_or_else(|| {
                    col()
                        .gap(z(8.0))
                        .child(
                            text("PREVIEW")
                                .font(scaled_font(type_::LABEL, s.zoom))
                                .color(t.text_secondary),
                        )
                        .child(
                            text("Select a file")
                                .font(scaled_font(type_::TITLE, s.zoom))
                                .color(t.text),
                        )
                        .child(
                            text("A quick look at your file, without leaving the folder.")
                                .font(scaled_font(type_::BODY, s.zoom))
                                .color(t.text_secondary)
                                .w_full()
                                .wrap(),
                        )
                }),
        );

    let content = row()
        .w_full()
        .h_full()
        .child(sidebar)
        .child(el().w(z(1.0)).h_full().fill(t.separator))
        .child(
            col()
                .grow(1.0)
                .h_full()
                .min_w(0.0)
                .child(header)
                .child(tabs)
                .child(location_bar)
                .child(
                    row()
                        .w_full()
                        .grow(1.0)
                        .min_h(0.0)
                        .min_w(0.0)
                        .child(
                            col()
                                .grow(1.0)
                                .h_full()
                                .min_w(z(list_min_width))
                                .min_h(0.0)
                                .child(list),
                        )
                        .child(
                            row()
                                .key("preview-resize")
                                .w(z(7.0))
                                .h_full()
                                .center()
                                .hover(t.hover)
                                .cursor(Cursor::ResizeColumn)
                                .on_drag(move |s: &mut State, d: &mut Drag, tx: &mut Tx| {
                                    match d.phase {
                                        Phase::Down => {
                                            d.data = s
                                                .preview_width
                                                .clamp(preview_min_width, preview_max_width)
                                        }
                                        Phase::Move => {
                                            s.preview_width = (d.data - d.dx / s.zoom)
                                                .clamp(preview_min_width, preview_max_width);
                                            tx.instant();
                                        }
                                        Phase::Up if d.cancelled() => {
                                            s.preview_width = d.data;
                                            save_settings(s);
                                        }
                                        Phase::Up => {
                                            s.preview_width = s
                                                .preview_width
                                                .clamp(preview_min_width, preview_max_width);
                                            save_settings(s);
                                        }
                                    }
                                })
                                .child(el().w(z(1.0)).h_full().fill(t.separator)),
                        )
                        .child(preview),
                )
                .child(
                    row()
                        .w_full()
                        .h(z(28.0))
                        .px(z(18.0))
                        .center_y()
                        .fill(t.bg)
                        .child(
                        text(format!(
                            "{} items    ·    {}    ·    {}%    Ctrl + / − to zoom · Ctrl+0 reset",
                            s.entries.len(),
                            s.status,
                            (s.zoom * 100.0).round() as u32
                        ))
                        .font(scaled_font(type_::LABEL, s.zoom))
                        .color(t.text_secondary),
                    ),
                ),
        );
    let lightbox = if s.image_expanded {
        s.selected_image.as_ref().map(|image| {
            let pixels = image.pixels();
            el().key("image-lightbox")
                .absolute(0.0, 0.0)
                .w_full()
                .h_full()
                .fill(Color::BLACK.alpha(0.78))
                .transition(Transition::opacity())
                .on_press(|s: &mut State| s.image_expanded = false)
                .child(
                    el().key("file-preview-image")
                        .matched("file-preview-image")
                        .w_full()
                        .h_full()
                        .padding(z(36.0))
                        .center()
                        .pixels_fit(pixels, rune::SurfaceFit::Fit)
                        .on_press(|s: &mut State| s.image_expanded = false),
                )
        })
    } else {
        None
    };
    let context_overlay = s
        .context_menu
        .map(|anchor| menu_overlay(s, cx, anchor, context_items(s), context_menu_select));
    let device_overlay = s.device_menu.as_ref().map(|(x, y, object)| {
        menu_overlay(s, cx, (*x, *y), device_items(s, object), device_menu_select)
    });
    col()
        .key("app-root")
        .w_full()
        .h_full()
        .fill(t.bg)
        .child(content)
        .children(lightbox)
        .children(context_overlay)
        .children(device_overlay)
}

/// A popup menu at `anchor`, kept inside the window. Pressing outside closes it; pressing an
/// enabled item closes it and calls `select` with the item's id.
fn menu_overlay(
    s: &State,
    cx: &mut Cx<State>,
    (anchor_x, anchor_y): (f32, f32),
    items: Vec<(&'static str, &'static str, bool)>,
    select: fn(&mut State, &'static str),
) -> El {
    let t = tok::theme(cx.system_dark());
    let z = |points: f32| points * s.zoom;
    let window = cx.layout_size("app-root").unwrap_or([1120.0, 740.0]);
    let menu_width = z(250.0);
    let menu_height = z(16.0
        + items
            .iter()
            .map(|(id, _, _)| if id.is_empty() { 10.0 } else { 32.0 })
            .sum::<f32>());
    let x = anchor_x.clamp(0.0, (window[0] - menu_width).max(0.0));
    let y = anchor_y.clamp(0.0, (window[1] - menu_height).max(0.0));
    el().key("context-menu-overlay")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .on_press(|s: &mut State| {
            s.context_menu = None;
            s.device_menu = None;
        })
        .child(
            col()
                .key("context-menu")
                .absolute(x, y)
                .w(menu_width)
                .padding(z(6.0))
                .gap(z(2.0))
                .radius(radius::MD)
                .fill(t.card)
                .border(1.0, t.card_border)
                .shadow(10.0, z(18.0), Color::BLACK.alpha(0.35))
                .children(items.into_iter().map(|(id, label, enabled)| {
                    if id.is_empty() {
                        el().w_full().h(z(1.0)).fill(t.separator)
                    } else {
                        row()
                            .w_full()
                            .h(z(32.0))
                            .px(z(10.0))
                            .center_y()
                            .radius(radius::SM)
                            .hover(t.hover)
                            .opacity(if enabled { 1.0 } else { 0.45 })
                            .child(
                                text(label)
                                    .font(scaled_font(type_::BODY, s.zoom))
                                    .color(t.text),
                            )
                            .on_press(move |s: &mut State| {
                                if enabled {
                                    select(s, id);
                                } else {
                                    s.context_menu = None;
                                    s.device_menu = None;
                                }
                            })
                    }
                })),
        )
}

fn on_key(s: &mut State, event: &KeyEvent, _: &mut Tx) -> bool {
    if event.key == Key::Escape && (s.context_menu.is_some() || s.device_menu.is_some()) {
        s.context_menu = None;
        s.device_menu = None;
        return true;
    }
    if event.key == Key::Escape && s.image_expanded {
        s.image_expanded = false;
        return true;
    }
    if event.key == Key::Escape && s.operation.is_some() {
        cancel_operation(s);
        return true;
    }
    if event.key == Key::Enter && s.renaming.is_some() {
        commit_rename(s);
        return true;
    }
    if event.key == Key::Escape && s.renaming.is_some() {
        cancel_rename(s);
        return true;
    }
    if event.key == Key::Enter && s.editing_location {
        submit_location(s);
        return true;
    }
    if event.key == Key::Escape && s.editing_location {
        s.editing_location = false;
        return true;
    }
    if event.key == Key::Enter && !s.search.focused && !s.location_edit.focused {
        if let Some(path) = s.selected.clone() {
            activate_path(s, &path);
            return true;
        }
        return false;
    }
    if !event.mods.ctrl {
        return false;
    }
    let typing = s.search.focused || s.location_edit.focused || s.editing_location;
    match &event.key {
        Key::Command('a') if !typing => {
            s.selected_paths = s
                .entries
                .iter()
                .filter(|entry| {
                    entry
                        .name
                        .to_lowercase()
                        .contains(&s.search.text.to_lowercase())
                })
                .map(|entry| entry.path.clone())
                .collect();
            s.selected = s.selected_paths.last().cloned();
            s.preview = format!("{} items selected.", s.selected_paths.len());
            s.status = format!("{} items selected", s.selected_paths.len());
            s.selected_image = None;
            true
        }
        Key::Command('c' | 'x' | 'v') if !typing && !ensure_local_cwd(s) => true,
        Key::Command('c') if !typing => {
            s.clipboard = local_paths(&s.selected_paths);
            s.clipboard_cut = false;
            write_file_clipboard(&s.clipboard, false);
            s.status = format!("Copied {} item(s)", s.clipboard.len());
            true
        }
        Key::Command('x') if !typing => {
            s.clipboard = local_paths(&s.selected_paths);
            s.clipboard_cut = true;
            write_file_clipboard(&s.clipboard, true);
            s.status = format!("Cut {} item(s)", s.clipboard.len());
            true
        }
        Key::Command('v') if !typing => {
            transfer_clipboard(s);
            true
        }
        Key::Command('n') if event.mods.shift => {
            create_folder(s);
            true
        }
        Key::Command('n') => {
            let cwd = s.cwd.clone();
            open_in_new_tab(s, &cwd);
            true
        }
        Key::Command('t') => {
            s.tabs.push(s.cwd.clone());
            s.active_tab = s.tabs.len() - 1;
            true
        }
        Key::Command('w') => {
            close_active_tab(s);
            true
        }
        Key::Command('h') => {
            toggle_hidden(s);
            true
        }
        Key::Command('z') if !typing && !s.undo_trash.is_empty() => {
            undo_last_trash(s);
            true
        }
        Key::Command('+') | Key::Command('=') => {
            s.zoom = (s.zoom + 0.1).min(1.6);
            save_settings(s);
            true
        }
        Key::Text(value) if value == "+" || value == "=" => {
            s.zoom = (s.zoom + 0.1).min(1.6);
            save_settings(s);
            true
        }
        Key::Command('-') => {
            s.zoom = (s.zoom - 0.1).max(0.75);
            save_settings(s);
            true
        }
        Key::Text(value) if value == "-" => {
            s.zoom = (s.zoom - 0.1).max(0.75);
            save_settings(s);
            true
        }
        Key::Command('0') => {
            s.zoom = 1.0;
            save_settings(s);
            true
        }
        Key::Text(value) if value == "0" => {
            s.zoom = 1.0;
            save_settings(s);
            true
        }
        _ => false,
    }
}

/// Tracks focus for operation notifications; coming back to the window withdraws the last
/// one, since the status bar now shows the same result.
fn on_window_focus(s: &mut State, focused: bool, _: &mut Tx) {
    s.window_focused = focused;
    if focused {
        s.notifier.withdraw();
    }
}

fn main() {
    let _ = START_DIRECTORY.set(startup_directory());
    rune::set_app_id("ferune");
    let app = rune::App::new(AppSpec::<State> {
        setup: Some(|s, waker| {
            let _ = WAKER.set(waker.clone());
            watch_volumes(s);
            let cache = rune::ImageCache::new(waker);
            for kind in [
                IconKind::Folder,
                IconKind::Pdf,
                IconKind::Image,
                IconKind::Markdown,
                IconKind::Code,
                IconKind::Text,
                IconKind::Archive,
                IconKind::Audio,
                IconKind::Video,
                IconKind::Generic,
            ] {
                let fallback = cache.svg(kind.fallback_svg().into_bytes(), 64.0, 1.0);
                s.icons.insert(kind, fallback);
            }
            for (kind, path) in &s.icon_paths {
                s.icons.insert(*kind, cache.file(path, 64.0, 1.0));
            }
            s.image_cache = Some(cache);
            s.thumbs = Some(thumbnail::Thumbs::new(wake));
            s.mime = Some(mime::Detector::new(wake));
            request_mime_types(s);
            request_thumbnails(s);
            file_manager1::spawn(None, s.file_manager_tx.clone(), wake);
        }),
        poll: Some(poll),
        on_key: Some(on_key),
        ..AppSpec::new("Ferune File Manager", (1120.0, 740.0), init, view)
    })
    .on_focus(on_window_focus);
    let _ = APP.set(app.handle());
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_previewable_file_types_case_insensitively() {
        let label = |name: &str| FileType::of(None, Path::new(name)).label();
        assert_eq!(label("report.PDF"), "PDF DOCUMENT");
        assert_eq!(label("notes.md"), "MARKDOWN");
        assert_eq!(label("photo.png"), "IMAGE");
        assert_eq!(label("readme.txt"), "TEXT FILE");
        assert_eq!(label("data.bin"), "FILE");
    }

    #[test]
    fn maps_common_extensions_to_icon_families() {
        let icon = |name: &str| FileType::of(None, Path::new(name)).icon();
        assert_eq!(icon("report.pdf"), IconKind::Pdf);
        assert_eq!(icon("photo.avif"), IconKind::Image);
        assert_eq!(icon("drawing.svgz"), IconKind::Image);
        assert_eq!(icon("notes.md"), IconKind::Markdown);
        assert_eq!(icon("archive.tar"), IconKind::Archive);
        assert_eq!(icon("main.rs"), IconKind::Code);
        assert_eq!(icon("settings.ini"), IconKind::Code);
        assert_eq!(icon("server.log"), IconKind::Text);
        assert_eq!(icon("song.flac"), IconKind::Audio);
        assert_eq!(icon("clip.mkv"), IconKind::Video);
        assert_eq!(icon("unknown.xyz"), IconKind::Generic);
    }

    #[test]
    fn detected_mime_types_beat_the_file_name() {
        let kind = |mime: &str, name: &str| FileType::of(Some(mime), Path::new(name));
        // Extensionless and mismatched names.
        assert_eq!(kind("image/png", "picture"), FileType::Image);
        assert_eq!(kind("image/png", "photo.backup"), FileType::Image);
        assert_eq!(kind("application/pdf", "scan.png"), FileType::Pdf);
        assert_eq!(kind("text/markdown", "README"), FileType::Markdown);
        assert_eq!(kind("text/x-shellscript", "run-me"), FileType::Code);
        assert_eq!(kind("text/x-readme", "README"), FileType::Text);
        assert_eq!(kind("application/json", "data"), FileType::Code);
        assert_eq!(kind("video/mp2t", "clip.ts"), FileType::Video);
        assert_eq!(kind("application/zip", "bundle.dat"), FileType::Archive);
        assert_eq!(kind("image/heif", "IMG_1.heic"), FileType::OtherImage);
        assert_eq!(kind("application/x-executable", "tool"), FileType::Other);
        // Aliases resolve when the database is loaded; uppercase is normalized either way.
        assert_eq!(kind("APPLICATION/PDF", "x"), FileType::Pdf);
        // "Don't know" answers fall back to the extension.
        assert_eq!(
            kind("application/octet-stream", "notes.md"),
            FileType::Markdown
        );
        assert_eq!(kind("application/octet-stream", "blob"), FileType::Unknown);
        let entry = |mime: Option<&str>, name: &str| Entry {
            path: Location::Local(PathBuf::from("/x").join(name)),
            name: name.into(),
            dir: false,
            hidden: false,
            size: 1,
            modified_secs: 0,
            modified: String::new(),
            content_type: mime.map(Into::into),
            target: None,
        };
        assert_eq!(
            entry_icon_kind(&entry(Some("image/png"), "picture")),
            IconKind::Image
        );
        assert_eq!(entry_icon_kind(&entry(None, "picture")), IconKind::Generic);
        assert_eq!(
            entry_icon_kind(&entry(Some("text/plain"), "x.pdf")),
            IconKind::Text
        );
    }

    #[test]
    fn previews_follow_the_type_not_the_name() {
        let dir = std::env::temp_dir().join(format!("ferune-preview-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x02\x80\0\0\x01\xe0\x08\x06\0\0\0";
        let picture = dir.join("picture");
        fs::write(&picture, png).unwrap();
        assert!(preview_file(&picture, FileType::Image).contains("640 × 480 pixels"));
        let markdown = dir.join("NOTES");
        fs::write(&markdown, "# Title\n- item").unwrap();
        assert!(preview_file(&markdown, FileType::Markdown).starts_with("◆  TITLE"));
        // Text that's named like an archive still previews as text when typed as text...
        let text = dir.join("words.zip");
        fs::write(&text, "just words").unwrap();
        assert_eq!(preview_file(&text, FileType::Text), "just words");
        // ...and a file typed as an archive isn't read as text whatever it contains.
        assert_eq!(
            preview_file(&text, FileType::Archive),
            "This file can't be previewed as text."
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// The whole local path through the real `gio`: an extensionless PNG starts with a generic
    /// icon, then gets the image icon and an image preview once the detector answers.
    #[test]
    fn local_entries_pick_up_detected_types() {
        let has_gio = Command::new("gio")
            .arg("version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !has_gio {
            eprintln!("skipping: gio isn't installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("ferune-detect-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let picture = dir.join("picture");
        fs::write(
            &picture,
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89",
        )
        .unwrap();
        let mut s = init_at(Location::Local(dir.clone()));
        s.mime = Some(mime::Detector::new(|| {}));
        request_mime_types(&mut s);
        let location = Location::Local(picture.clone());
        select_path(&mut s, &location);
        assert!(!s.preview.starts_with("IMAGE PREVIEW"));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while s.entries[0].content_type.is_none() && std::time::Instant::now() < deadline {
            poll_mime_types(&mut s);
            std::thread::sleep(Duration::from_millis(10));
        }
        let content_type = s.entries[0].content_type.clone();
        if content_type.as_deref() == Some("application/octet-stream") {
            eprintln!("skipping: shared-mime-info's database isn't installed");
            return;
        }
        assert_eq!(content_type.as_deref(), Some("image/png"));
        assert_eq!(entry_icon_kind(&s.entries[0]), IconKind::Image);
        assert_eq!(location_icon_kind(&s, &location), IconKind::Image);
        assert!(s.preview.starts_with("IMAGE PREVIEW"), "{}", s.preview);
        // Re-reading the folder keeps the type without asking again.
        read_directory(&mut s);
        assert_eq!(s.entries[0].content_type.as_deref(), Some("image/png"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn markdown_preview_formats_headings_and_lists() {
        assert_eq!(
            markdown_preview("# Title\n\n- first\n- second"),
            "◆  TITLE\n\n  •  first\n  •  second"
        );
    }

    #[test]
    fn file_uris_round_trip_through_percent_encoding() {
        let path = Path::new("/tmp/My Files/naïve #1%.txt");
        let uri = file_uri(path);
        assert!(!uri.contains(' '));
        assert_eq!(local_file_uri(&uri).as_deref(), Some(path));
        assert_eq!(local_file_uri("file://host/etc"), None);
    }

    #[test]
    fn reads_png_dimensions_from_the_header() {
        let mut png_header = vec![0; 24];
        png_header[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png_header[16..20].copy_from_slice(&640_u32.to_be_bytes());
        png_header[20..24].copy_from_slice(&480_u32.to_be_bytes());
        assert_eq!(image_dimensions(&png_header), Some((640, 480)));
    }

    fn remote(uri: &str) -> Location {
        Location::Remote(uri.into())
    }

    fn remote_item(uri: &str, file_type: &str, target_uri: Option<&str>) -> gio::RemoteEntry {
        gio::RemoteEntry {
            uri: uri.into(),
            name: Location::Remote(uri.into()).name(),
            file_type: file_type.into(),
            size: 2048,
            modified: Some(0),
            content_type: Some("text/plain".into()),
            target_uri: target_uri.map(Into::into),
            hidden: false,
        }
    }

    /// A state showing `sftp://host/srv` with a folder, a file and a shortcut, as if `gio list`
    /// had just answered.
    fn remote_state() -> State {
        let mut s = init_at(Location::Local(std::env::temp_dir()));
        s.cwd = remote("sftp://host/srv");
        (s.sort, s.ascending, s.show_hidden) = (SortBy::Name, true, false);
        s.listing_generation = 7;
        s.gio_tx
            .send(GioEvent::Listed {
                generation: 7,
                result: Ok(vec![
                    remote_item("sftp://host/srv/docs", "directory", None),
                    remote_item("sftp://host/srv/notes.txt", "regular", None),
                    remote_item("sftp://host/srv/root.link", "shortcut", Some("file:///")),
                ]),
            })
            .unwrap();
        drain_gio_events(&mut s);
        s
    }

    #[test]
    fn remote_listings_become_sorted_entries() {
        let s = remote_state();
        let names = s
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["docs", "root.link", "notes.txt"]);
        assert_eq!(s.list_message, None);
        assert!(s.status.starts_with("Connected"));
    }

    #[test]
    fn superseded_and_failed_remote_listings_report_correctly() {
        let mut s = remote_state();
        s.gio_tx
            .send(GioEvent::Listed {
                generation: 3,
                result: Ok(Vec::new()),
            })
            .unwrap();
        drain_gio_events(&mut s);
        assert_eq!(
            s.entries.len(),
            3,
            "a stale listing must not replace the folder"
        );

        s.gio_tx
            .send(GioEvent::Listed {
                generation: 7,
                result: Err("Hostname not known".into()),
            })
            .unwrap();
        drain_gio_events(&mut s);
        assert!(s.entries.is_empty());
        assert!(s.status.contains("Hostname not known"), "{}", s.status);
        assert!(
            s.list_message
                .as_deref()
                .unwrap()
                .contains("Hostname not known")
        );
    }

    #[test]
    fn remote_entries_browse_folders_follow_shortcuts_and_open_files() {
        let s = remote_state();
        assert_eq!(
            folder_target(&s, &remote("sftp://host/srv/docs")),
            Some(remote("sftp://host/srv/docs"))
        );
        assert_eq!(
            folder_target(&s, &remote("sftp://host/srv/root.link")),
            Some(Location::Local("/".into()))
        );
        assert_eq!(
            folder_target(&s, &remote("sftp://host/srv/notes.txt")),
            None
        );
        assert_eq!(
            location_icon_kind(&s, &remote("sftp://host/srv/notes.txt")),
            IconKind::Text
        );
    }

    #[test]
    fn remote_folders_only_offer_browse_actions() {
        let mut s = remote_state();
        select_path(&mut s, &remote("sftp://host/srv/notes.txt"));
        assert!(s.preview.contains("text/plain"), "{}", s.preview);
        assert!(s.selected_image.is_none());
        let enabled = context_items(&s)
            .into_iter()
            .filter(|(id, _, enabled)| *enabled && !id.is_empty())
            .map(|(id, _, _)| id)
            .collect::<Vec<_>>();
        assert_eq!(enabled, ["open", "copy-path", "properties"]);

        // Even when invoked directly, mutating actions refuse remote folders.
        context_action(&mut s, Some("delete"));
        assert!(s.status.contains("aren't available"), "{}", s.status);
        s.clipboard = vec![std::env::temp_dir()];
        transfer_clipboard(&mut s);
        assert!(s.operation_rx.is_none());
        assert!(s.status.contains("aren't available"), "{}", s.status);
    }

    #[test]
    fn sorting_a_remote_folder_reuses_its_listing() {
        let mut s = remote_state();
        // What set_sort does, minus saving the user's settings file.
        s.ascending = false;
        refresh_view(&mut s);
        let names = s
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["root.link", "docs", "notes.txt"]);
        assert_eq!(
            s.listing_generation, 7,
            "sorting must not list the folder again"
        );
    }

    fn volume(device: &str, drive: &str, mount_points: &[&Path]) -> udisks::Volume {
        udisks::Volume {
            object: format!("/org/freedesktop/UDisks2/block_devices/{device}"),
            device: format!("/dev/{device}"),
            label: None,
            fs_type: "vfat".into(),
            size: 16 * 1024 * 1024 * 1024,
            read_only: false,
            mount_points: mount_points.iter().map(|p| p.to_path_buf()).collect(),
            drive: format!("/org/freedesktop/UDisks2/drives/{drive}"),
            drive_name: "Kingston DataTraveler".into(),
            can_power_off: true,
            ejectable: false,
        }
    }

    /// A state whose startup device listing has been consumed, so the real machine's drives
    /// can't race the test's own events.
    fn device_state(volumes: Vec<udisks::Volume>) -> State {
        let mut s = init_at(Location::Local(std::env::temp_dir()));
        let _ = s.device_rx.recv_timeout(Duration::from_secs(20));
        s.volumes_refreshing = false;
        s.device_tx.send(DeviceEvent::Volumes(Ok(volumes))).unwrap();
        drain_device_events(&mut s);
        s
    }

    #[test]
    fn device_listings_fill_the_sidebar_and_close_stale_menus() {
        let mut s = device_state(vec![volume("sdb1", "K", &[]), volume("sdb2", "K", &[])]);
        assert_eq!(s.volumes.len(), 2);
        assert_eq!(volume_name(&s.volumes[0]), "16.0 GB Volume");
        s.device_menu = Some((0.0, 0.0, s.volumes[1].object.clone()));
        s.device_tx
            .send(DeviceEvent::Volumes(Ok(vec![volume("sdb1", "K", &[])])))
            .unwrap();
        drain_device_events(&mut s);
        assert!(
            s.device_menu.is_none(),
            "unplugged device's menu must close"
        );
        s.device_tx
            .send(DeviceEvent::Volumes(Err("udisksd isn't running".into())))
            .unwrap();
        drain_device_events(&mut s);
        assert!(s.volumes.is_empty());
        assert_eq!(s.volumes_error.as_deref(), Some("udisksd isn't running"));
    }

    #[test]
    fn device_menu_offers_actions_that_fit_the_volume_state() {
        let dir = std::env::temp_dir();
        let s = device_state(vec![volume("sdb1", "K", &[]), volume("sdc1", "S", &[&dir])]);
        let enabled = |object: &str| {
            device_items(&s, object)
                .into_iter()
                .filter(|(_, _, enabled)| *enabled)
                .map(|(id, _, _)| id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            enabled(&s.volumes[0].object),
            ["open", "mount", "power-off"]
        );
        assert_eq!(
            enabled(&s.volumes[1].object),
            ["open", "unmount", "power-off"]
        );
        assert!(device_items(&s, "/gone").is_empty());
    }

    #[test]
    fn opening_a_mounted_device_navigates_without_running_udisksctl() {
        let dir = std::env::temp_dir().join(format!("ferune-mounted-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut s = device_state(vec![volume("sdb1", "K", &[&dir])]);
        let object = s.volumes[0].object.clone();
        device_action(&mut s, &object, DeviceOp::Open);
        assert_eq!(s.cwd, Location::Local(dir.clone()));
        assert!(s.device_busy.is_empty());
        // Mount on an already-mounted volume is refused up front, too.
        device_action(&mut s, &object, DeviceOp::Mount);
        assert!(s.status.contains("already mounted"), "{}", s.status);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn busy_devices_refuse_more_actions() {
        let mut s = device_state(vec![volume("sdb1", "K", &[]), volume("sdb2", "K", &[])]);
        let drive = s.volumes[0].drive.clone();
        s.device_busy.insert(drive, "Powering off");
        let object = s.volumes[1].object.clone();
        device_action(&mut s, &object, DeviceOp::Open);
        assert!(s.status.contains("busy: Powering off"), "{}", s.status);
        assert_eq!(s.device_busy.len(), 1, "no second job was started");
        assert!(
            device_items(&s, &object)
                .iter()
                .all(|(_, _, enabled)| !enabled)
        );
    }

    #[test]
    fn finished_device_jobs_report_and_leave_released_mount_points() {
        let dir = std::env::temp_dir().join(format!("ferune-release-{}", std::process::id()));
        fs::create_dir_all(dir.join("photos")).unwrap();
        let mut s = device_state(vec![volume("sdb1", "K", &[&dir])]);
        navigate(&mut s, Location::Local(dir.join("photos")));
        let drive = s.volumes[0].drive.clone();
        s.device_busy.insert(drive.clone(), "Powering off");
        s.device_tx
            .send(DeviceEvent::Done {
                key: drive,
                name: "Kingston DataTraveler".into(),
                op: DeviceOp::Remove(udisks::Removal::PowerOff),
                released: vec![dir.clone()],
                result: Ok(None),
            })
            .unwrap();
        drain_device_events(&mut s);
        assert!(s.device_busy.is_empty());
        assert_eq!(s.cwd, Location::Local(home()));
        assert!(s.status.contains("safe to unplug"), "{}", s.status);

        s.device_tx
            .send(DeviceEvent::Done {
                key: "x".into(),
                name: "STICK".into(),
                op: DeviceOp::Unmount,
                released: Vec::new(),
                result: Err("Error unmounting /dev/sdb1: target is busy".into()),
            })
            .unwrap();
        drain_device_events(&mut s);
        assert_eq!(
            s.status,
            "Couldn't unmount STICK: Error unmounting /dev/sdb1: target is busy"
        );

        s.device_tx
            .send(DeviceEvent::Done {
                key: "y".into(),
                name: "STICK".into(),
                op: DeviceOp::Open,
                released: Vec::new(),
                result: Ok(Some(dir.clone())),
            })
            .unwrap();
        drain_device_events(&mut s);
        assert_eq!(s.cwd, Location::Local(dir.clone()), "a fresh mount opens");
        assert!(s.status.starts_with("Mounted STICK at"), "{}", s.status);
        let _ = fs::remove_dir_all(dir);
    }

    fn fm_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferune-reveal-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Hands a FileManager1 call to the UI the way the D-Bus thread does: translated, sent on
    /// the channel, then drained by `poll`.
    fn deliver(s: &mut State, method: file_manager1::Method, uris: &[String], startup_id: &str) {
        let request = file_manager1::plan(method, uris, startup_id);
        s.file_manager_tx
            .send(file_manager1::Event::Request(request))
            .unwrap();
        assert!(drain_file_manager_events(s));
    }

    fn local_at(path: &Path) -> Location {
        Location::Local(path.to_path_buf())
    }

    #[test]
    fn show_items_selects_in_tabs() {
        use file_manager1::Method::ShowItems;
        let dir = fm_dir("items");
        let start = dir.join("start");
        let sub = dir.join("sub");
        fs::create_dir_all(&start).unwrap();
        fs::create_dir_all(&sub).unwrap();
        for file in [dir.join("a.txt"), dir.join("b.txt"), sub.join("c.txt")] {
            fs::write(file, "x").unwrap();
        }
        let mut s = init_at(local_at(&start));
        s.show_hidden = false;

        // An untouched window shows the first folder in its only tab.
        deliver(
            &mut s,
            ShowItems,
            &[file_uri(&dir.join("a.txt"))],
            "token-1",
        );
        assert_eq!(s.tabs, vec![local_at(&dir)]);
        assert_eq!(s.cwd, local_at(&dir));
        assert_eq!(s.selected, Some(local_at(&dir.join("a.txt"))));
        assert_eq!(s.selected_paths, vec![local_at(&dir.join("a.txt"))]);
        assert_eq!(
            s.reveal_scroll,
            Some(dir.join("a.txt").display().to_string())
        );
        assert_eq!(s.activation.as_deref(), Some("token-1"));
        assert!(!s.reveal_properties);
        assert_eq!(s.history.len(), 2, "Back returns to where it started");

        // Once used, another folder gets its own tab, in front.
        deliver(&mut s, ShowItems, &[file_uri(&sub.join("c.txt"))], "");
        assert_eq!(s.tabs, vec![local_at(&dir), local_at(&sub)]);
        assert_eq!(s.active_tab, 1);
        assert_eq!(s.selected, Some(local_at(&sub.join("c.txt"))));

        // A folder already in a tab is switched to rather than opened again.
        deliver(
            &mut s,
            ShowItems,
            &[file_uri(&dir.join("a.txt")), file_uri(&dir.join("b.txt"))],
            "",
        );
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.active_tab, 0);
        assert_eq!(
            s.selected_paths,
            vec![local_at(&dir.join("a.txt")), local_at(&dir.join("b.txt"))]
        );
        assert_eq!(s.status, "2 items selected");

        // A file made after the folder was read (a finished download) is found: the folder
        // in front is read again.
        fs::write(dir.join("download.zip"), "x").unwrap();
        deliver(
            &mut s,
            ShowItems,
            &[file_uri(&dir.join("download.zip"))],
            "",
        );
        assert_eq!(s.selected, Some(local_at(&dir.join("download.zip"))));
        assert_eq!(s.tabs.len(), 2);

        // Hidden items turn hidden files on, without saving the preference.
        fs::write(dir.join(".secret"), "x").unwrap();
        read_directory(&mut s);
        assert!(entry_for(&s, &local_at(&dir.join(".secret"))).is_none());
        deliver(&mut s, ShowItems, &[file_uri(&dir.join(".secret"))], "");
        assert!(s.show_hidden);
        assert_eq!(s.selected, Some(local_at(&dir.join(".secret"))));
        assert!(s.status.ends_with("showing hidden files"), "{}", s.status);

        // Something that vanished: its folder opens, nothing is selected, the status says why.
        deliver(&mut s, ShowItems, &[file_uri(&sub.join("gone"))], "");
        assert_eq!(s.cwd, local_at(&sub));
        assert_eq!(s.selected, None);
        assert!(s.status.contains("doesn't exist"), "{}", s.status);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn show_folders_and_properties() {
        use file_manager1::Method::{ShowFolders, ShowItemProperties};
        let dir = fm_dir("folders");
        let (one, two) = (dir.join("one"), dir.join("two"));
        fs::create_dir_all(&one).unwrap();
        fs::create_dir_all(&two).unwrap();
        fs::write(two.join("report.pdf"), "x").unwrap();
        let mut s = init_at(local_at(&dir));
        select_path(&mut s, &local_at(&one));

        deliver(&mut s, ShowFolders, &[file_uri(&one), file_uri(&two)], "");
        assert_eq!(s.tabs, vec![local_at(&dir), local_at(&one), local_at(&two)]);
        assert_eq!(s.active_tab, 2);
        assert_eq!(s.cwd, local_at(&two));
        assert_eq!(s.selected, None);
        assert_eq!(s.reveal_scroll, None);

        deliver(
            &mut s,
            ShowItemProperties,
            &[file_uri(&two.join("report.pdf"))],
            "",
        );
        assert_eq!(s.tabs.len(), 3);
        assert_eq!(s.selected, Some(local_at(&two.join("report.pdf"))));
        // `poll` shows the existing Properties view once the window is up.
        assert!(s.reveal_properties);
        assert!(context_items(&s).contains(&("properties", "Properties", true)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn remote_reveals_wait_for_the_listing() {
        let mut s = init_at(Location::Local(std::env::temp_dir()));
        s.show_hidden = false;
        select_path(&mut s, &Location::Local(std::env::temp_dir()));
        deliver(
            &mut s,
            file_manager1::Method::ShowItems,
            &["sftp://host/srv/notes.txt".into()],
            "",
        );
        assert_eq!(s.cwd, remote("sftp://host/srv"));
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.selected, None, "nothing to select until gio answers");
        assert!(s.pending_reveal.is_some());
        // Stand in for gio: the real listing (which fails, no such host) is now stale.
        s.listing_generation += 1;
        s.gio_tx
            .send(GioEvent::Listed {
                generation: s.listing_generation,
                result: Ok(vec![
                    remote_item("sftp://host/srv/docs", "directory", None),
                    remote_item("sftp://host/srv/notes.txt", "regular", None),
                ]),
            })
            .unwrap();
        drain_gio_events(&mut s);
        assert_eq!(s.selected, Some(remote("sftp://host/srv/notes.txt")));
        assert_eq!(
            s.reveal_scroll.as_deref(),
            Some("sftp://host/srv/notes.txt")
        );
        assert!(s.pending_reveal.is_none());

        // A listing that fails drops the reveal.
        deliver(
            &mut s,
            file_manager1::Method::ShowItems,
            &["sftp://other/x/y".into()],
            "",
        );
        s.listing_generation += 1;
        s.gio_tx
            .send(GioEvent::Listed {
                generation: s.listing_generation,
                result: Err("no route".into()),
            })
            .unwrap();
        drain_gio_events(&mut s);
        assert!(s.pending_reveal.is_none());
        assert_eq!(s.selected, None);
    }

    #[test]
    fn service_failures_leave_the_window_alone() {
        let dir = fm_dir("unavailable");
        let mut s = init_at(local_at(&dir));
        let (tabs, status) = (s.tabs.clone(), s.status.clone());
        s.file_manager_tx
            .send(file_manager1::Event::Unavailable("name taken".into()))
            .unwrap();
        s.file_manager_tx
            .send(file_manager1::Event::Registered)
            .unwrap();
        assert!(!drain_file_manager_events(&mut s));
        assert_eq!((s.tabs.clone(), s.status.clone()), (tabs, status));
        assert_eq!(s.activation, None);
        let _ = fs::remove_dir_all(dir);
    }

    /// The whole path: another application calls ShowItems on a private bus, the D-Bus thread
    /// queues it, and draining it in `poll` moves the window.
    #[test]
    fn bus_calls_reach_the_window() {
        let Some(bus) = file_manager1::tests::PrivateBus::start("window") else {
            return;
        };
        let dir = fm_dir("bus");
        fs::write(dir.join("photo.png"), "x").unwrap();
        let mut s = init_at(Location::Local(std::env::temp_dir()));
        let woke = Arc::new(AtomicBool::new(false));
        let flag = woke.clone();
        file_manager1::spawn(
            Some(bus.address.clone()),
            s.file_manager_tx.clone(),
            move || flag.store(true, Ordering::SeqCst),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let call = file_uri(&dir.join("photo.png"));
        while bus
            .call("ShowItems", std::slice::from_ref(&call), "tok")
            .is_err()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "service never registered"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        while s.cwd != local_at(&dir) {
            assert!(
                std::time::Instant::now() < deadline,
                "request never arrived"
            );
            drain_file_manager_events(&mut s);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(woke.load(Ordering::SeqCst));
        assert_eq!(s.selected, Some(local_at(&dir.join("photo.png"))));
        assert_eq!(s.activation.as_deref(), Some("tok"));
        let _ = fs::remove_dir_all(dir);
    }

    fn recorded(s: &State) -> &[notify::Notice] {
        s.notifier.recorded.as_deref().unwrap()
    }

    /// Starts a stand-in copy of `total` items that began `ago`, whose events the test sends.
    fn fake_operation(
        s: &mut State,
        kind: notify::Kind,
        total: usize,
        ago: Duration,
    ) -> mpsc::Sender<OperationEvent> {
        let items = vec![PathBuf::from("/nowhere/item"); total];
        let (_, sender) = begin_operation(s, kind, &items, Some("~/Backup".into()), "Starting…");
        s.operation_info.as_mut().unwrap().started = Instant::now() - ago;
        sender
    }

    #[test]
    fn long_operations_notify_only_while_the_window_is_away() {
        let long = Duration::from_secs(5);
        let mut s = init_at(Location::Local(std::env::temp_dir()));
        let mut tx = Tx::default();

        // Watched: the status bar is enough.
        let sender = fake_operation(&mut s, notify::Kind::Copy, 2, long);
        sender.send(OperationEvent::Finished(2)).unwrap();
        assert!(poll_operation(&mut s, &mut tx));
        assert_eq!(s.status, "Pasted 2 item(s)");
        assert!(recorded(&s).is_empty());

        // Away, but quick.
        on_window_focus(&mut s, false, &mut tx);
        let sender = fake_operation(&mut s, notify::Kind::Copy, 2, Duration::from_millis(300));
        sender.send(OperationEvent::Finished(2)).unwrap();
        poll_operation(&mut s, &mut tx);
        assert!(recorded(&s).is_empty());

        // Long only because the "Replace file?" prompt sat unanswered.
        let sender = fake_operation(&mut s, notify::Kind::Copy, 1, long);
        sender
            .send(OperationEvent::Waited(Duration::from_secs(4)))
            .unwrap();
        sender.send(OperationEvent::Finished(1)).unwrap();
        poll_operation(&mut s, &mut tx);
        assert!(recorded(&s).is_empty());

        // Away and long: exactly one notice, even though the worker's channel then closes.
        let sender = fake_operation(&mut s, notify::Kind::Move, 3, long);
        sender
            .send(OperationEvent::Progress(1, 3, "a".into(), "Moving"))
            .unwrap();
        sender.send(OperationEvent::Cancelled(1)).unwrap();
        drop(sender);
        poll_operation(&mut s, &mut tx);
        assert!(!poll_operation(&mut s, &mut tx), "nothing left to poll");
        assert_eq!(s.status, "Operation cancelled after 1 item(s)");
        assert_eq!(recorded(&s).len(), 1);
        assert_eq!(recorded(&s)[0].summary, "Move cancelled");
        assert_eq!(recorded(&s)[0].body, "Stopped after 1 of 3 items");

        // A worker that dies without a word is a failure, too.
        let sender = fake_operation(&mut s, notify::Kind::Copy, 1, long);
        drop(sender);
        poll_operation(&mut s, &mut tx);
        assert!(s.status.contains("stopped unexpectedly"), "{}", s.status);
        assert_eq!(recorded(&s).len(), 2);
        assert_eq!(recorded(&s)[1].urgency, notify::Urgency::Critical);

        // Coming back withdraws what's on screen.
        assert!(s.notifier.showing());
        on_window_focus(&mut s, true, &mut tx);
        assert!(!s.notifier.showing());
    }

    #[test]
    fn duplicate_runs_in_the_background_and_leaves_a_cut_alone() {
        let dir = std::env::temp_dir().join(format!("ferune-duplicate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("folder")).unwrap();
        fs::write(dir.join("folder/inside.txt"), "x").unwrap();
        fs::write(dir.join("notes.txt"), "notes").unwrap();
        let mut s = init_at(Location::Local(dir.clone()));
        let mut tx = Tx::default();
        s.selected = Some(Location::Local(dir.join("folder")));
        s.selected_paths = vec![
            Location::Local(dir.join("folder")),
            Location::Local(dir.join("notes.txt")),
        ];
        let cut = vec![PathBuf::from("/elsewhere/cut-me")];
        s.clipboard = cut.clone();
        s.clipboard_cut = true;

        context_action(&mut s, Some("duplicate"));
        assert!(s.operation_rx.is_some(), "{}", s.status);
        let deadline = Instant::now() + Duration::from_secs(10);
        while s.operation_rx.is_some() {
            assert!(Instant::now() < deadline, "duplicate never finished");
            poll_operation(&mut s, &mut tx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(s.status, "Duplicated 2 item(s)");
        assert_eq!(
            fs::read_to_string(dir.join("folder copy 1/inside.txt")).unwrap(),
            "x"
        );
        assert_eq!(
            fs::read_to_string(dir.join("notes copy 1.txt")).unwrap(),
            "notes"
        );
        assert_eq!(s.clipboard, cut);
        assert!(s.clipboard_cut);
        // Focused the whole time (and quick): no notice.
        assert!(recorded(&s).is_empty());
        let _ = fs::remove_dir_all(dir);
    }
}
