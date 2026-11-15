use super::*;
use crate::file_manager1::tests::PrivateBus;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn report(kind: Kind, outcome: Outcome, done: usize, total: usize) -> Report {
    Report {
        kind,
        outcome,
        done,
        total,
        item: None,
        destination: Some("~/Backup".into()),
        active: Duration::from_secs(5),
        focused: false,
    }
}

#[test]
fn quick_focused_or_empty_operations_stay_quiet() {
    let long = report(Kind::Copy, Outcome::Finished, 3, 3);
    assert!(notice(&long).is_some());

    let quick = Report {
        active: MIN_DURATION - Duration::from_millis(1),
        ..long.clone()
    };
    assert_eq!(notice(&quick), None, "under the threshold");
    let at_threshold = Report {
        active: MIN_DURATION,
        ..long.clone()
    };
    assert!(notice(&at_threshold).is_some());

    let watched = Report {
        focused: true,
        ..long.clone()
    };
    assert_eq!(notice(&watched), None, "the status bar already says it");

    // Even a failure stays in the status bar when the user is looking at it.
    let failed_watched = Report {
        outcome: Outcome::Failed("Permission denied".into()),
        focused: true,
        ..long.clone()
    };
    assert_eq!(notice(&failed_watched), None);

    let nothing = report(Kind::Copy, Outcome::Finished, 0, 0);
    assert_eq!(notice(&nothing), None);
}

#[test]
fn completion_names_the_items_and_destination() {
    assert_eq!(
        notice(&report(Kind::Copy, Outcome::Finished, 3, 3)),
        Some(Notice {
            summary: "Copy finished".into(),
            body: "Copied 3 items to “~/Backup”".into(),
            urgency: Urgency::Normal,
            category: "transfer.complete",
        })
    );
    let one = Report {
        item: Some("photos".into()),
        ..report(Kind::Move, Outcome::Finished, 1, 1)
    };
    let one = notice(&one).unwrap();
    assert_eq!(one.summary, "Move finished");
    assert_eq!(one.body, "Moved “photos” to “~/Backup”");

    let duplicate = Report {
        destination: None,
        ..report(Kind::Duplicate, Outcome::Finished, 1, 1)
    };
    assert_eq!(notice(&duplicate).unwrap().body, "Duplicated 1 item");
}

#[test]
fn cancellation_and_failure_say_how_far_it_got() {
    let cancelled = notice(&report(Kind::Move, Outcome::Cancelled, 2, 5)).unwrap();
    assert_eq!(cancelled.summary, "Move cancelled");
    assert_eq!(cancelled.body, "Stopped after 2 of 5 items");
    assert_eq!(cancelled.urgency, Urgency::Normal);
    assert_eq!(cancelled.category, "transfer");

    let failed = notice(&report(
        Kind::Copy,
        Outcome::Failed(" No space left on device (os error 28)\n".into()),
        1,
        4,
    ))
    .unwrap();
    assert_eq!(failed.summary, "Copy failed");
    assert_eq!(
        failed.body,
        "No space left on device (os error 28)\n1 of 4 items done"
    );
    assert_eq!(failed.urgency, Urgency::Critical);
    assert_eq!(failed.category, "transfer.error");
}

#[test]
fn long_errors_are_shortened_on_a_character_boundary() {
    let error = "é".repeat(MAX_ERROR_CHARS + 50);
    let failed = notice(&report(Kind::Copy, Outcome::Failed(error), 0, 1)).unwrap();
    let first_line = failed.body.lines().next().unwrap();
    assert_eq!(first_line.chars().count(), MAX_ERROR_CHARS);
    assert!(first_line.ends_with('…'));
}

#[test]
fn markup_is_escaped_for_daemons_that_parse_it() {
    assert_eq!(
        escape_markup("Copied “<b>a&b</b>” 'x' \"y\""),
        "Copied “&lt;b&gt;a&amp;b&lt;/b&gt;” &apos;x&apos; &quot;y&quot;"
    );
    assert_eq!(escape_markup("plain name.txt"), "plain name.txt");
}

#[test]
fn recording_notifier_never_delivers() {
    let mut notifier = Notifier::recording();
    let notice = notice(&report(Kind::Copy, Outcome::Finished, 1, 1)).unwrap();
    notifier.show(notice.clone());
    assert!(notifier.showing());
    assert!(notifier.commands.is_none(), "no thread, no bus");
    notifier.withdraw();
    assert!(!notifier.showing());
    assert_eq!(notifier.recorded, Some(vec![notice]));
}

/// A call the stand-in daemon received.
#[derive(Debug, PartialEq)]
enum Call {
    Notify {
        replaces: u32,
        summary: String,
        body: String,
        urgency: Option<u8>,
        category: Option<String>,
    },
    Close(u32),
}

/// Stands in for the desktop's notification daemon on a private bus.
struct FakeDaemon {
    calls: Arc<Mutex<Vec<Call>>>,
    next_id: u32,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl FakeDaemon {
    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into(), "body-markup".into()]
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        _app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        mut hints: HashMap<String, zbus::OwnedVariant>,
        _expire_timeout: i32,
    ) -> u32 {
        let urgency = hints.remove("urgency").and_then(|v| u8::try_from(v).ok());
        let category = hints
            .remove("category")
            .and_then(|v| String::try_from(v).ok());
        self.calls.lock().unwrap().push(Call::Notify {
            replaces: replaces_id,
            summary,
            body,
            urgency,
            category,
        });
        if replaces_id != 0 {
            return replaces_id;
        }
        self.next_id += 1;
        self.next_id
    }

    fn close_notification(&self, id: u32) {
        self.calls.lock().unwrap().push(Call::Close(id));
    }
}

/// Serves a [`FakeDaemon`] on `address` from a thread; returns once its name is owned.
fn serve_fake_daemon(address: &str, calls: Arc<Mutex<Vec<Call>>>) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let address = address.to_owned();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let connection = zbus::connection::Builder::address(address.as_str())
                .name(BUS_NAME)
                .serve_at(OBJECT_PATH, FakeDaemon { calls, next_id: 0 })
                .build()
                .await
                .unwrap();
            ready_tx.send(()).unwrap();
            let _connection = connection;
            std::future::pending::<()>().await;
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
}

fn wait_for_calls(calls: &Mutex<Vec<Call>>, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while calls.lock().unwrap().len() < count {
        assert!(Instant::now() < deadline, "{:?}", calls.lock().unwrap());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn delivers_replaces_and_withdraws_over_dbus() {
    let Some(bus) = PrivateBus::start("notify") else {
        return;
    };
    let calls = Arc::new(Mutex::new(Vec::new()));
    serve_fake_daemon(&bus.address, calls.clone());
    let mut notifier = Notifier::new(Some(bus.address.clone()));

    let first = Report {
        item: Some("<draft> & notes".into()),
        ..report(Kind::Copy, Outcome::Finished, 1, 1)
    };
    notifier.show(notice(&first).unwrap());
    notifier.show(
        notice(&report(
            Kind::Move,
            Outcome::Failed("Disk full".into()),
            0,
            2,
        ))
        .unwrap(),
    );
    notifier.withdraw();
    // Nothing on screen any more: a second withdrawal sends nothing.
    notifier.withdraw();
    wait_for_calls(&calls, 3);
    std::thread::sleep(Duration::from_millis(100));

    let calls = calls.lock().unwrap();
    assert_eq!(
        *calls,
        [
            Call::Notify {
                replaces: 0,
                summary: "Copy finished".into(),
                // The daemon parses markup, so the file name arrives escaped.
                body: "Copied “&lt;draft&gt; &amp; notes” to “~/Backup”".into(),
                urgency: Some(1),
                category: Some("transfer.complete".into()),
            },
            // The second replaces the first rather than piling up.
            Call::Notify {
                replaces: 1,
                summary: "Move failed".into(),
                body: "Disk full\n0 of 2 items done".into(),
                urgency: Some(2),
                category: Some("transfer.error".into()),
            },
            Call::Close(1),
        ]
    );
}

#[test]
fn a_missing_daemon_is_not_fatal() {
    let Some(bus) = PrivateBus::start("notify-none") else {
        return;
    };
    // Nobody owns org.freedesktop.Notifications on this bus.
    let mut notifier = Notifier::new(Some(bus.address.clone()));
    let notice = notice(&report(Kind::Copy, Outcome::Finished, 1, 1)).unwrap();
    notifier.show(notice.clone());
    notifier.withdraw();
    notifier.show(notice);
    // The thread keeps running and accepting work after the failures.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        notifier
            .commands
            .as_ref()
            .unwrap()
            .send(Command::Withdraw)
            .is_ok()
    );
}
