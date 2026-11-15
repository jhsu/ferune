//! Freedesktop desktop notifications (`org.freedesktop.Notifications`) for long file
//! operations that end while Ferune's window isn't focused.
//!
//! [`notice`] decides, from plain values, whether a finished operation deserves a notification
//! and what it says; it's pure, so the tests check every decision without a desktop. A
//! [`Notifier`] delivers notices from its own thread, started on first use, with a
//! single-threaded tokio runtime and a direct zbus call (zbus and tokio already come with
//! Rune). The UI thread only ever sends on a channel, so a slow or missing notification
//! daemon can't stall it; when there's no session bus or no daemon, the notice is dropped with
//! a note on stderr and Ferune carries on.
//!
//! Only one Ferune notification is kept at a time: each new one replaces the last
//! (`replaces_id`), and the window regaining focus withdraws it, because the status bar shows
//! the same result by then.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

pub const BUS_NAME: &str = "org.freedesktop.Notifications";
pub const OBJECT_PATH: &str = "/org/freedesktop/Notifications";
pub const INTERFACE: &str = "org.freedesktop.Notifications";

/// Operations that end sooner than this never notify: the user is most likely still looking.
/// Time spent answering Ferune's own prompts (the "Replace file?" dialog) doesn't count.
pub const MIN_DURATION: Duration = Duration::from_secs(2);

/// How long connecting to the bus, or one call to the daemon, may take before giving up on
/// that notice.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors longer than this are cut short in the notification body (the status bar keeps the
/// whole message).
const MAX_ERROR_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Copy,
    Move,
    Duplicate,
}

impl Kind {
    fn noun(self) -> &'static str {
        match self {
            Kind::Copy => "Copy",
            Kind::Move => "Move",
            Kind::Duplicate => "Duplicate",
        }
    }

    fn past(self) -> &'static str {
        match self {
            Kind::Copy => "Copied",
            Kind::Move => "Moved",
            Kind::Duplicate => "Duplicated",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Finished,
    Cancelled,
    Failed(String),
}

/// How a file operation ended, and what the user could see when it did.
#[derive(Clone, Debug)]
pub struct Report {
    pub kind: Kind,
    pub outcome: Outcome,
    /// Items processed (skipped ones included) and items asked for.
    pub done: usize,
    pub total: usize,
    /// The item's name when exactly one was asked for.
    pub item: Option<String>,
    /// Where copies and moves went (a folder's name); `None` for duplicates.
    pub destination: Option<String>,
    /// Wall-clock time minus time spent waiting on the user's answers.
    pub active: Duration,
    /// Whether Ferune's window had focus when the operation ended.
    pub focused: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Urgency {
    Normal,
    Critical,
}

impl Urgency {
    fn byte(self) -> u8 {
        match self {
            Urgency::Normal => 1,
            Urgency::Critical => 2,
        }
    }
}

/// One notification's content, as plain text; [`escape_markup`] is applied on delivery when
/// the daemon parses body markup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    /// A standard Freedesktop category: `transfer.complete`, `transfer.error` or `transfer`.
    pub category: &'static str,
}

/// The notification for `report`, or `None` when it shouldn't notify: the window had focus,
/// the operation was quick, or it had nothing to do.
pub fn notice(report: &Report) -> Option<Notice> {
    if report.focused || report.active < MIN_DURATION || report.total == 0 {
        return None;
    }
    let kind = report.kind;
    let what = match (&report.item, report.total) {
        (Some(item), 1) => format!("“{item}”"),
        _ => items(report.total),
    };
    let to = report
        .destination
        .as_ref()
        .map(|folder| format!(" to “{folder}”"))
        .unwrap_or_default();
    let progress = format!(
        "{} of {}",
        report.done.min(report.total),
        items(report.total)
    );
    Some(match &report.outcome {
        Outcome::Finished => Notice {
            summary: format!("{} finished", kind.noun()),
            body: format!("{} {what}{to}", kind.past()),
            urgency: Urgency::Normal,
            category: "transfer.complete",
        },
        Outcome::Cancelled => Notice {
            summary: format!("{} cancelled", kind.noun()),
            body: format!("Stopped after {progress}"),
            urgency: Urgency::Normal,
            category: "transfer",
        },
        Outcome::Failed(error) => Notice {
            summary: format!("{} failed", kind.noun()),
            body: format!("{}\n{progress} done", shorten(error.trim())),
            urgency: Urgency::Critical,
            category: "transfer.error",
        },
    })
}

fn items(count: usize) -> String {
    if count == 1 {
        "1 item".into()
    } else {
        format!("{count} items")
    }
}

fn shorten(text: &str) -> String {
    if text.chars().count() <= MAX_ERROR_CHARS {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(MAX_ERROR_CHARS - 1).collect();
    short.push('…');
    short
}

/// Daemons that advertise `body-markup` read the body as a small subset of HTML, so a file
/// named `a<b>&c` must arrive escaped to show as itself.
pub fn escape_markup(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            c => escaped.push(c),
        }
    }
    escaped
}

enum Command {
    Show(Notice),
    /// Close the notification shown last, if it's still up.
    Withdraw,
}

/// Sends notices to the desktop from a background thread that starts with the first one.
pub struct Notifier {
    /// The bus to use instead of the session bus (tests).
    address: Option<String>,
    commands: Option<mpsc::Sender<Command>>,
    /// A notice went out since the last withdrawal, so focusing the window has one to close.
    showing: bool,
    /// Tests keep notices here instead of delivering them.
    #[cfg(test)]
    pub recorded: Option<Vec<Notice>>,
}

impl Notifier {
    pub fn new(address: Option<String>) -> Notifier {
        Notifier {
            address,
            commands: None,
            showing: false,
            #[cfg(test)]
            recorded: None,
        }
    }

    /// Records notices in `recorded` and never starts a thread or touches a bus.
    #[cfg(test)]
    pub fn recording() -> Notifier {
        Notifier {
            recorded: Some(Vec::new()),
            ..Notifier::new(None)
        }
    }

    /// Whether a notice is (as far as Ferune knows) still on screen.
    #[cfg(test)]
    pub fn showing(&self) -> bool {
        self.showing
    }

    /// Queues `notice` for delivery; never blocks.
    pub fn show(&mut self, notice: Notice) {
        self.showing = true;
        #[cfg(test)]
        if let Some(recorded) = &mut self.recorded {
            recorded.push(notice);
            return;
        }
        self.send(Command::Show(notice));
    }

    /// Closes the last notice, which went stale once the user came back to the window.
    pub fn withdraw(&mut self) {
        if std::mem::take(&mut self.showing) {
            self.send(Command::Withdraw);
        }
    }

    fn send(&mut self, command: Command) {
        #[cfg(test)]
        if self.recorded.is_some() {
            return;
        }
        if self.commands.is_none() {
            let (sender, receiver) = mpsc::channel();
            let address = self.address.clone();
            let started = std::thread::Builder::new()
                .name("ferune-notify".into())
                .spawn(move || deliver(address, receiver));
            match started {
                Ok(_) => self.commands = Some(sender),
                Err(error) => {
                    eprintln!("ferune: desktop notifications unavailable: {error}");
                    return;
                }
            }
        }
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }
}

/// A connection to the daemon and whether it parses body markup.
struct Daemon {
    connection: zbus::Connection,
    markup: bool,
}

/// The notifier thread: one connection, made on demand and dropped after any error so the
/// next notice tries again (a daemon may start later in the session).
fn deliver(address: Option<String>, commands: mpsc::Receiver<Command>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ferune: desktop notifications unavailable: {error}");
            return;
        }
    };
    let mut daemon: Option<Daemon> = None;
    // The daemon's id for the notice on screen; 0 for none.
    let mut shown = 0u32;
    while let Ok(command) = commands.recv() {
        runtime.block_on(async {
            match command {
                Command::Show(notice) => {
                    if daemon.is_none() {
                        match tokio::time::timeout(CALL_TIMEOUT, connect(address.as_deref())).await
                        {
                            Ok(Ok(connected)) => daemon = Some(connected),
                            Ok(Err(error)) => return unavailable(&error.to_string()),
                            Err(_) => return unavailable("the session bus timed out"),
                        }
                    }
                    let Some(connected) = &daemon else { return };
                    let call = notify(connected, &notice, shown);
                    match tokio::time::timeout(CALL_TIMEOUT, call).await {
                        Ok(Ok(id)) => shown = id,
                        Ok(Err(error)) => {
                            daemon = None;
                            shown = 0;
                            unavailable(&error.to_string());
                        }
                        Err(_) => {
                            daemon = None;
                            shown = 0;
                            unavailable("the notification daemon didn't answer");
                        }
                    }
                }
                Command::Withdraw => {
                    if let (Some(connected), id @ 1..) = (&daemon, std::mem::take(&mut shown)) {
                        let body = (id,);
                        let call = connected.connection.call_method(
                            Some(BUS_NAME),
                            OBJECT_PATH,
                            Some(INTERFACE),
                            "CloseNotification",
                            &body,
                        );
                        // Already dismissed by the user is fine too.
                        let _ = tokio::time::timeout(CALL_TIMEOUT, call).await;
                    }
                }
            }
        });
    }
}

fn unavailable(why: &str) {
    eprintln!("ferune: desktop notification not shown: {why}");
}

async fn connect(address: Option<&str>) -> zbus::Result<Daemon> {
    let builder = match address {
        Some(address) => zbus::connection::Builder::address(address),
        None => zbus::connection::Builder::session(),
    };
    let connection = builder.build().await?;
    // A daemon that can't list its capabilities still gets plain-text bodies.
    let markup = match connection
        .call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some(INTERFACE),
            "GetCapabilities",
            &(),
        )
        .await
    {
        Ok(reply) => reply
            .body()
            .deserialize::<Vec<String>>()
            .is_ok_and(|capabilities| capabilities.iter().any(|c| c == "body-markup")),
        Err(zbus::Error::MethodError(..)) => false,
        Err(error) => return Err(error),
    };
    Ok(Daemon { connection, markup })
}

async fn notify(daemon: &Daemon, notice: &Notice, replaces: u32) -> zbus::Result<u32> {
    let body = if daemon.markup {
        escape_markup(&notice.body)
    } else {
        notice.body.clone()
    };
    let mut hints: HashMap<&str, zbus::Variant> = HashMap::new();
    hints.insert("urgency", zbus::Variant::from(notice.urgency.byte()));
    hints.insert("category", zbus::Variant::from(notice.category));
    // Lets the daemon show Ferune's name and icon from `ferune.desktop`.
    hints.insert("desktop-entry", zbus::Variant::from("ferune"));
    let actions: Vec<&str> = Vec::new();
    let reply = daemon
        .connection
        .call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some(INTERFACE),
            "Notify",
            &(
                "Ferune",
                replaces,
                "system-file-manager",
                notice.summary.as_str(),
                body.as_str(),
                actions,
                hints,
                -1i32,
            ),
        )
        .await?;
    reply.body().deserialize::<u32>()
}

#[cfg(test)]
mod tests;
