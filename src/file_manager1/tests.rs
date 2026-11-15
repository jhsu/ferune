use super::*;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferune-fm1-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn local(path: &str) -> Option<Location> {
    Some(Location::Local(PathBuf::from(path)))
}

fn uris(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn translates_file_uris_and_paths() {
    // Percent-escapes decode into the bytes of the local name.
    assert_eq!(
        target("file:///tmp/My%20Files/na%C3%AFve%20%231.txt"),
        local("/tmp/My Files/naïve #1.txt")
    );
    // Round trip through the URI Ferune itself writes.
    let odd = Path::new("/tmp/a b/100%/ü?.md");
    assert_eq!(
        target(&crate::file_uri(odd)),
        Some(Location::Local(odd.into()))
    );
    // Variants callers send instead of a canonical URI.
    assert_eq!(target("file://localhost/etc/hosts"), local("/etc/hosts"));
    assert_eq!(target("FILE:///etc/hosts"), local("/etc/hosts"));
    assert_eq!(target("file:/etc/hosts"), local("/etc/hosts"));
    assert_eq!(target("/etc/hosts"), local("/etc/hosts"));
    assert_eq!(target("  /etc/hosts\n"), local("/etc/hosts"));
    // Trailing slashes and `.` segments go, so the folder matches its listed path.
    assert_eq!(target("file:///usr/share/"), local("/usr/share"));
    assert_eq!(target("/usr/./share//"), local("/usr/share"));
    assert_eq!(target("file:///"), local("/"));
}

#[test]
fn rejects_what_cant_be_shown() {
    assert_eq!(target("file://otherhost/etc/hosts"), None);
    assert_eq!(target("relative/path"), None);
    assert_eq!(target("file:relative"), None);
    assert_eq!(target(""), None);
    assert_eq!(target("not a uri"), None);
}

#[test]
fn keeps_other_schemes_as_gio_uris() {
    assert_eq!(
        target("SFTP://me@host/srv/My Dir/"),
        Some(Location::Remote("sftp://me@host/srv/My%20Dir".into()))
    );
    assert_eq!(
        target("smb://nas/share/a%20b.txt"),
        Some(Location::Remote("smb://nas/share/a%20b.txt".into()))
    );
}

#[test]
fn show_items_opens_each_containing_folder_once() {
    let dir = temp_dir("items");
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    for file in [dir.join("a.txt"), dir.join("b.txt"), sub.join("c.txt")] {
        std::fs::write(file, "x").unwrap();
    }
    let at = |path: &Path| Location::Local(path.to_path_buf());
    let request = plan(
        Method::ShowItems,
        &[
            crate::file_uri(&dir.join("a.txt")),
            dir.join("b.txt").display().to_string(),
            crate::file_uri(&sub.join("c.txt")),
            crate::file_uri(&dir.join("a.txt")),
            // A folder passed to ShowItems is selected in its parent, too.
            crate::file_uri(&sub),
        ],
        "token-1",
    );
    assert_eq!(request.startup_id, "token-1");
    assert!(request.problems.is_empty(), "{:?}", request.problems);
    assert_eq!(
        request.reveals,
        vec![
            Reveal {
                folder: at(&dir),
                select: vec![at(&dir.join("a.txt")), at(&dir.join("b.txt")), at(&sub)],
                properties: false,
            },
            Reveal {
                folder: at(&sub),
                select: vec![at(&sub.join("c.txt"))],
                properties: false,
            },
        ]
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn missing_items_fall_back_to_their_folder() {
    let dir = temp_dir("missing");
    let request = plan(
        Method::ShowItems,
        &[
            crate::file_uri(&dir.join("gone.txt")),
            crate::file_uri(&dir.join("no/such/file")),
            "file://elsewhere/x".into(),
        ],
        "",
    );
    assert_eq!(
        request.reveals,
        vec![Reveal {
            folder: Location::Local(dir.clone()),
            select: Vec::new(),
            properties: false,
        }]
    );
    assert_eq!(request.problems.len(), 3, "{:?}", request.problems);
    // Nothing at all to show.
    let request = plan(
        Method::ShowItems,
        &uris(&["relative", "/no/such/dir/x"]),
        "",
    );
    assert!(request.reveals.is_empty());
    assert_eq!(request.problems.len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn show_folders_opens_folders_and_reveals_files() {
    let dir = temp_dir("folders");
    let (one, two) = (dir.join("one"), dir.join("two"));
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    std::fs::write(dir.join("file.txt"), "x").unwrap();
    let request = plan(
        Method::ShowFolders,
        &[
            crate::file_uri(&one),
            crate::file_uri(&two),
            crate::file_uri(&dir.join("file.txt")),
            crate::file_uri(&dir.join("absent")),
            "sftp://host/srv".into(),
        ],
        "",
    );
    let folders: Vec<_> = request.reveals.iter().map(|r| r.folder.clone()).collect();
    assert_eq!(
        folders,
        vec![
            Location::Local(one),
            Location::Local(two),
            Location::Local(dir.clone()),
            Location::Remote("sftp://host/srv".into()),
        ]
    );
    // A file given as a folder is selected in its folder; folders select nothing.
    assert_eq!(
        request.reveals[2].select,
        vec![Location::Local(dir.join("file.txt"))]
    );
    assert!(request.reveals[0].select.is_empty());
    assert_eq!(request.problems.len(), 1, "{:?}", request.problems);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn properties_reveal_one_item() {
    let dir = temp_dir("properties");
    std::fs::write(dir.join("a"), "x").unwrap();
    std::fs::write(dir.join("b"), "x").unwrap();
    let request = plan(
        Method::ShowItemProperties,
        &[
            crate::file_uri(&dir.join("a")),
            crate::file_uri(&dir.join("b")),
        ],
        "",
    );
    assert_eq!(
        request.reveals,
        vec![Reveal {
            folder: Location::Local(dir.clone()),
            select: vec![Location::Local(dir.join("a"))],
            properties: true,
        }]
    );
    assert_eq!(request.problems.len(), 1);
    // A root has no parent: open it with itself selected.
    let root = plan(Method::ShowItemProperties, &uris(&["file:///"]), "");
    assert_eq!(
        root.reveals,
        vec![Reveal {
            folder: Location::Local("/".into()),
            select: vec![Location::Local("/".into())],
            properties: true,
        }]
    );
    // Remote items aren't checked here: gio answers once the folder is listed.
    let remote = plan(
        Method::ShowItemProperties,
        &uris(&["sftp://h/srv/x.txt"]),
        "",
    );
    assert_eq!(
        remote.reveals,
        vec![Reveal {
            folder: Location::Remote("sftp://h/srv".into()),
            select: vec![Location::Remote("sftp://h/srv/x.txt".into())],
            properties: true,
        }]
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A throwaway `dbus-daemon` bus, so the tests never touch (or steal a name on) the user's
/// session bus.
pub(crate) struct PrivateBus {
    daemon: Child,
    dir: PathBuf,
    pub(crate) address: String,
}

impl PrivateBus {
    /// `None`, with a note, when `dbus-daemon` isn't installed.
    pub(crate) fn start(name: &str) -> Option<PrivateBus> {
        let dir = temp_dir(&format!("bus-{name}"));
        let config = dir.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.display()
            ),
        )
        .unwrap();
        let daemon = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut daemon) = daemon else {
            eprintln!("skipping: dbus-daemon isn't installed");
            let _ = std::fs::remove_dir_all(dir);
            return None;
        };
        let mut address = String::new();
        let stdout = daemon.stdout.take().unwrap();
        BufReader::new(stdout).read_line(&mut address).unwrap();
        let address = address.trim().to_owned();
        assert!(!address.is_empty(), "dbus-daemon printed no address");
        Some(PrivateBus {
            daemon,
            dir,
            address,
        })
    }

    /// Calls a FileManager1 method as another application would.
    pub(crate) fn call(&self, method: &str, uris: &[String], startup_id: &str) -> zbus::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let connection = zbus::connection::Builder::address(self.address.as_str())
                .build()
                .await?;
            connection
                .call_method(
                    Some(BUS_NAME),
                    OBJECT_PATH,
                    Some(INTERFACE),
                    method,
                    &(uris, startup_id),
                )
                .await
                .map(drop)
        })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn next_event(events: &mpsc::Receiver<Event>) -> Event {
    events
        .recv_timeout(Duration::from_secs(10))
        .expect("no FileManager1 event within 10 s")
}

#[test]
fn serves_the_interface_on_the_bus() {
    let Some(bus) = PrivateBus::start("serve") else {
        return;
    };
    let dir = temp_dir("serve");
    std::fs::write(dir.join("song.ogg"), "x").unwrap();
    let (sender, events) = mpsc::channel();
    let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = woken.clone();
    spawn(Some(bus.address.clone()), sender, move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    assert!(matches!(next_event(&events), Event::Registered));

    let item = crate::file_uri(&dir.join("song.ogg"));
    bus.call("ShowItems", std::slice::from_ref(&item), "startup-42")
        .unwrap();
    let Event::Request(request) = next_event(&events) else {
        panic!("expected a request");
    };
    assert_eq!(request.method, Method::ShowItems);
    assert_eq!(request.startup_id, "startup-42");
    assert_eq!(request.reveals[0].folder, Location::Local(dir.clone()));
    assert_eq!(
        request.reveals[0].select,
        vec![Location::Local(dir.join("song.ogg"))]
    );
    // Each event woke the UI: Registered, then the request.
    assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), 2);

    for method in ["ShowFolders", "ShowItemProperties"] {
        bus.call(method, std::slice::from_ref(&item), "").unwrap();
        let Event::Request(request) = next_event(&events) else {
            panic!("expected a request");
        };
        assert_eq!(request.reveals[0].folder, Location::Local(dir.clone()));
    }

    // Nothing showable is an InvalidArgs error the caller can fall back on, and no event.
    let error = bus
        .call("ShowItems", &uris(&["relative/path"]), "")
        .unwrap_err();
    assert!(
        matches!(&error, zbus::Error::FDO(e) if matches!(**e, zbus::fdo::Error::InvalidArgs(_))),
        "{error:?}"
    );
    let error = bus.call("ShowFolders", &[], "").unwrap_err();
    assert!(matches!(error, zbus::Error::FDO(_)), "{error:?}");
    assert!(events.recv_timeout(Duration::from_millis(200)).is_err());

    // A second instance finds the name taken and gives up without taking it over.
    let (sender, second) = mpsc::channel();
    spawn(Some(bus.address.clone()), sender, || {});
    match next_event(&second) {
        Event::Unavailable(why) => assert!(why.contains("already owned"), "{why}"),
        _ => panic!("the second instance should not get the name"),
    }
    bus.call("ShowFolders", &[crate::file_uri(&dir)], "")
        .unwrap();
    assert!(matches!(next_event(&events), Event::Request(_)));
    assert!(second.recv_timeout(Duration::from_millis(200)).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_missing_bus_is_reported_not_fatal() {
    let dir = temp_dir("nobus");
    let (sender, events) = mpsc::channel();
    spawn(
        Some(format!(
            "unix:path={}",
            dir.join("no-such-socket").display()
        )),
        sender,
        || {},
    );
    assert!(matches!(next_event(&events), Event::Unavailable(_)));
    let _ = std::fs::remove_dir_all(dir);
}
