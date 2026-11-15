//! The Freedesktop `org.freedesktop.FileManager1` D-Bus service: how browsers, editors and
//! other desktop apps ask the file manager to "show in folder" or "show properties".
//!
//! The service runs on its own thread with a single-threaded tokio runtime (zbus and tokio
//! already come with Rune's `dbus` feature). A method call never touches the UI's `State`: it
//! turns the caller's URIs into a [`Request`], a plan of plain values, and sends it over a
//! channel, then wakes the event loop, which applies it in `poll`. A missing session bus or a
//! name that another file manager already owns is reported as [`Event::Unavailable`] and
//! changes nothing else.
//!
//! Interface (`/org/freedesktop/FileManager1`):
//!
//! - `ShowFolders(as uris, s startup_id)`: open each folder.
//! - `ShowItems(as uris, s startup_id)`: open each item's folder with the item selected.
//! - `ShowItemProperties(as uris, s startup_id)`: reveal the item and show its properties.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crate::location::Location;

pub const BUS_NAME: &str = "org.freedesktop.FileManager1";
pub const OBJECT_PATH: &str = "/org/freedesktop/FileManager1";
#[cfg(test)]
pub const INTERFACE: &str = "org.freedesktop.FileManager1";

/// How long connecting to the bus and requesting the name may take before giving up.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Named after the D-Bus methods they come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum Method {
    ShowFolders,
    ShowItems,
    ShowItemProperties,
}

/// One folder to show and the items in it to select.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reveal {
    pub folder: Location,
    /// Selected once the folder is listed; empty to just open the folder.
    pub select: Vec<Location>,
    /// Show the (first) selected item's properties once it's revealed.
    pub properties: bool,
}

/// A method call translated for the UI thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// In the caller's order; the last one ends up in the active tab.
    pub reveals: Vec<Reveal>,
    /// What couldn't be shown, for the status bar.
    pub problems: Vec<String>,
    /// The caller's activation token (`XDG_ACTIVATION_TOKEN` or `DESKTOP_STARTUP_ID`), if any.
    pub startup_id: String,
}

pub enum Event {
    /// The name is ours; requests can arrive.
    Registered,
    /// No session bus, or the name belongs to someone else. Ferune runs as before without it.
    Unavailable(String),
    Request(Request),
}

/// A location from one entry of a method's `uris` argument. The spec asks for URIs, but some
/// callers send plain absolute paths, `file:/path` or `file://localhost/path`, so those work
/// too. `file://` URIs are percent-decoded into local paths; other schemes stay GIO URIs.
/// Relative paths and `file://otherhost/…` can't be shown and give `None`.
pub fn target(uri: &str) -> Option<Location> {
    let uri = uri.trim();
    if uri.starts_with('/') {
        return Some(Location::Local(clean_path(Path::new(uri))));
    }
    let Some(rest) = strip_prefix_ignore_case(uri, "file:") else {
        return Location::from_uri(uri).filter(Location::is_remote);
    };
    let encoded = if let Some(path) = strip_prefix_ignore_case(rest, "//localhost/") {
        format!("/{path}")
    } else if let Some(path) = rest.strip_prefix("//") {
        // `file:///path`; anything else names a host.
        path.starts_with('/').then(|| path.to_owned())?
    } else {
        rest.starts_with('/').then(|| rest.to_owned())?
    };
    let path = crate::local_file_uri(&format!("file://{encoded}"))?;
    Some(Location::Local(clean_path(&path)))
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Drops `.` segments, repeated and trailing slashes, so a path compares equal to the paths
/// Ferune lists. `..` stays: resolving it without the file system would be wrong across
/// symlinks.
fn clean_path(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| *component != Component::CurDir)
        .collect()
}

/// Translates a method call into what the UI should do. Local paths are checked here (on
/// the D-Bus thread) so the UI never waits on the file system for a caller's typo; remote
/// URIs are only known once `gio` lists them, so they're passed through.
pub fn plan(method: Method, uris: &[String], startup_id: &str) -> Request {
    let mut request = Request {
        method,
        reveals: Vec::new(),
        problems: Vec::new(),
        startup_id: startup_id.to_owned(),
    };
    let mut targets: Vec<Location> = Vec::new();
    for uri in uris {
        match target(uri) {
            Some(location) if !targets.contains(&location) => targets.push(location),
            Some(_) => {}
            None => request
                .problems
                .push(format!("unsupported location “{uri}”")),
        }
    }
    if method == Method::ShowItemProperties && targets.len() > 1 {
        // The properties view shows one item at a time.
        request.problems.push(format!(
            "properties are shown for the first of {} items",
            targets.len()
        ));
        targets.truncate(1);
    }
    for location in targets {
        let reveal = match method {
            Method::ShowFolders => folder_reveal(location, &mut request.problems),
            Method::ShowItems => item_reveal(location, false, &mut request.problems),
            Method::ShowItemProperties => item_reveal(location, true, &mut request.problems),
        };
        let Some(reveal) = reveal else { continue };
        // Items in the same folder share one tab and one selection.
        match request
            .reveals
            .iter_mut()
            .find(|existing| existing.folder == reveal.folder)
        {
            Some(existing) => {
                for item in reveal.select {
                    if !existing.select.contains(&item) {
                        existing.select.push(item);
                    }
                }
                existing.properties |= reveal.properties;
            }
            None => request.reveals.push(reveal),
        }
    }
    request
}

fn folder_reveal(location: Location, problems: &mut Vec<String>) -> Option<Reveal> {
    if let Location::Local(path) = &location {
        if path.is_file() || (path.exists() && !path.is_dir()) {
            // A file passed as a folder: show it where it is instead.
            return item_reveal(location, false, problems);
        }
        if !path.is_dir() {
            problems.push(format!("{} doesn't exist", path.display()));
            return None;
        }
    }
    Some(Reveal {
        folder: location,
        select: Vec::new(),
        properties: false,
    })
}

fn item_reveal(location: Location, properties: bool, problems: &mut Vec<String>) -> Option<Reveal> {
    // `None` at a root (`/`, `sftp://host/`).
    let Some(folder) = location.parent() else {
        // A root has no folder to be shown in: open it, selecting it for its properties.
        return Some(Reveal {
            select: if properties {
                vec![location.clone()]
            } else {
                Vec::new()
            },
            folder: location,
            properties,
        });
    };
    if let (Location::Local(path), Location::Local(folder_path)) = (&location, &folder) {
        // `symlink_metadata`: a dangling symlink is still an item to show.
        if path.symlink_metadata().is_err() {
            problems.push(format!("{} doesn't exist", path.display()));
            // Still open the folder it was in, when that's there.
            return folder_path.is_dir().then_some(Reveal {
                folder,
                select: Vec::new(),
                properties: false,
            });
        }
    }
    Some(Reveal {
        folder,
        select: vec![location],
        properties,
    })
}

/// The object served at [`OBJECT_PATH`].
struct Service {
    events: mpsc::Sender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Service {
    fn submit(
        &self,
        method: Method,
        uris: Vec<String>,
        startup_id: String,
    ) -> zbus::fdo::Result<()> {
        let request = plan(method, &uris, &startup_id);
        if request.reveals.is_empty() {
            // Callers such as browsers fall back to opening the folder themselves on an error.
            let why = if request.problems.is_empty() {
                "no URIs given".to_owned()
            } else {
                request.problems.join("; ")
            };
            return Err(zbus::fdo::Error::InvalidArgs(why));
        }
        self.events
            .send(Event::Request(request))
            .map_err(|_| zbus::fdo::Error::Failed("Ferune is shutting down".into()))?;
        (self.wake)();
        Ok(())
    }
}

#[zbus::interface(name = "org.freedesktop.FileManager1")]
impl Service {
    fn show_folders(&self, uris: Vec<String>, startup_id: String) -> zbus::fdo::Result<()> {
        self.submit(Method::ShowFolders, uris, startup_id)
    }

    fn show_items(&self, uris: Vec<String>, startup_id: String) -> zbus::fdo::Result<()> {
        self.submit(Method::ShowItems, uris, startup_id)
    }

    fn show_item_properties(&self, uris: Vec<String>, startup_id: String) -> zbus::fdo::Result<()> {
        self.submit(Method::ShowItemProperties, uris, startup_id)
    }
}

/// Serves the interface on the session bus (or on `address`, for tests) from a new thread.
/// Every outcome arrives on `events`: [`Event::Registered`] or [`Event::Unavailable`] once,
/// then a [`Event::Request`] per method call. `wake` runs after each event is sent.
pub fn spawn(
    address: Option<String>,
    events: mpsc::Sender<Event>,
    wake: impl Fn() + Send + Sync + 'static,
) {
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
    let report = {
        let events = events.clone();
        let wake = wake.clone();
        move |event| {
            let _ = events.send(event);
            wake();
        }
    };
    let started = std::thread::Builder::new()
        .name("ferune-filemanager1".into())
        .spawn({
            let report = report.clone();
            move || serve(address, events, wake, report)
        });
    if let Err(error) = started {
        report(Event::Unavailable(format!(
            "couldn't start its thread: {error}"
        )));
    }
}

fn serve(
    address: Option<String>,
    events: mpsc::Sender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
    report: impl Fn(Event),
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return report(Event::Unavailable(error.to_string())),
    };
    runtime.block_on(async move {
        let builder = match &address {
            Some(address) => zbus::connection::Builder::address(address.as_str()),
            None => zbus::connection::Builder::session(),
        };
        // Don't take the name from a file manager that's already running; whoever was first
        // keeps answering.
        let setup = builder
            .name(BUS_NAME)
            .replace_existing_names(false)
            .allow_name_replacements(false)
            .serve_at(OBJECT_PATH, Service { events, wake })
            .build();
        let connection = match tokio::time::timeout(SETUP_TIMEOUT, setup).await {
            Ok(Ok(connection)) => connection,
            Ok(Err(zbus::Error::NameTaken)) => {
                return report(Event::Unavailable(format!(
                    "{BUS_NAME} is already owned by another program"
                )));
            }
            Ok(Err(error)) => return report(Event::Unavailable(error.to_string())),
            Err(_) => return report(Event::Unavailable("the session bus timed out".into())),
        };
        report(Event::Registered);
        // The connection answers calls from tasks on this runtime for as long as it's held.
        let _connection = connection;
        std::future::pending::<()>().await;
    });
}

#[cfg(test)]
pub(crate) mod tests;
