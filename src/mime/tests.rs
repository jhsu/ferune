use super::*;
use std::time::{Duration, Instant};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferune-mime-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A 1×1 PNG's signature and header: enough for shared-mime-info's magic rules.
const PNG: &[u8] =
    b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89";

fn gio_available() -> bool {
    std::process::Command::new("gio")
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Polls until `done` holds or a few seconds pass.
fn wait_for(detector: &mut Detector, mut done: impl FnMut(&Detector) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        detector.poll();
        if done(detector) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn extension_guesses_cover_the_old_tables() {
    let guess = |name: &str| from_extension(Path::new(name));
    assert_eq!(guess("B.JPG"), Some("image/jpeg"));
    assert_eq!(guess("notes.md"), Some("text/markdown"));
    assert_eq!(guess("archive.tar"), Some("application/x-tar"));
    assert_eq!(guess("README"), None);
    assert_eq!(guess(".png"), None);
    // Uninformative detections fall back to the name.
    assert_eq!(
        effective(Some("application/octet-stream"), Path::new("a.pdf")),
        Some("application/pdf")
    );
    assert_eq!(
        effective(Some("application/x-zerosize"), Path::new("a.md")),
        Some("text/markdown")
    );
    assert_eq!(
        effective(Some("image/png"), Path::new("photo.backup")),
        Some("image/png")
    );
    assert_eq!(effective(None, Path::new("noext")), None);
}

#[test]
fn database_follows_aliases_subclasses_and_implicit_parents() {
    let mut database = Database::default();
    // The user's directory comes first and wins for aliases and icons.
    database.add(
        "application/x-pdf application/pdf\n",
        "# comment\napplication/x-shellscript application/x-executable\n",
        "application/zip:my-zip-icon\n",
    );
    database.add(
        "application/x-pdf application/other\ntext/x-markdown text/markdown\n",
        "application/x-shellscript text/plain\napplication/yaml text/plain\n\
         text/markdown text/plain\nimage/svg+xml application/xml\n",
        "application/zip:package-x-generic\napplication/x-tar:package-x-generic\n",
    );
    assert_eq!(database.canonical("application/x-pdf"), "application/pdf");
    assert_eq!(database.canonical("text/x-markdown"), "text/markdown");
    assert!(database.is_a("text/x-markdown", "text/markdown"));
    assert!(database.is_a("text/x-markdown", "text/plain"));
    // Parents accumulate across directories.
    assert!(database.is_a("application/x-shellscript", "application/x-executable"));
    assert!(database.is_a("application/x-shellscript", "text/plain"));
    assert!(database.is_a("application/yaml", "text/plain"));
    // Implicit rules: text/* is text/plain, *+xml is application/xml, which is text.
    assert!(database.is_a("text/x-python3", "text/plain"));
    assert!(database.is_a("application/rss+xml", "application/xml"));
    assert!(!database.is_a("image/png", "text/plain"));
    assert!(!database.is_a("application/pdf", "text/plain"));
    assert_eq!(
        database.generic_icon("application/zip"),
        Some("my-zip-icon")
    );
    assert_eq!(
        database.generic_icon("application/x-tar"),
        Some("package-x-generic")
    );
    assert_eq!(database.generic_icon("image/png"), None);
}

#[test]
fn database_survives_cycles_and_junk() {
    let mut database = Database::default();
    database.add("", "a/x a/y\na/y a/x\nnot-a-line\n a/z  \n", ":\nfoo:\n");
    assert!(database.is_a("a/x", "a/y"));
    assert!(!database.is_a("a/x", "text/plain"));
    assert_eq!(database.generic_icon("foo"), None);
}

#[test]
fn the_installed_database_loads_when_present() {
    let database = Database::load();
    if !Path::new("/usr/share/mime/subclasses").exists() {
        eprintln!("skipping: shared-mime-info isn't installed");
        return;
    }
    assert!(database.is_a("text/markdown", "text/plain"));
    assert!(database.is_a("application/x-yaml", "text/plain"));
    assert!(!database.is_a("image/png", "text/plain"));
}

/// End to end through the real `gio` and shared-mime-info: names that don't say what a file
/// is are typed by its contents.
#[test]
fn detects_extensionless_and_mismatched_files_through_gio() {
    if !gio_available() {
        eprintln!("skipping: gio isn't installed");
        return;
    }
    let dir = temp_dir("detect");
    let write = |name: &str, bytes: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    };
    let noext = write("picture", PNG);
    let mismatched = write("photo.backup", PNG);
    let markdown = write("notes.md", b"# Title\n");
    let script = write("run-me", b"#!/bin/sh\necho hi\n");
    let odd = write("odd\nname", b"plain words\n");
    let missing = dir.join("missing.png");
    let typed = detect(&[
        noext.clone(),
        missing.clone(),
        mismatched.clone(),
        markdown.clone(),
        script.clone(),
        odd.clone(),
    ])
    .unwrap();
    let find = |path: &Path| {
        typed
            .iter()
            .find(|typed| typed.path == path)
            .map(|typed| typed.content_type.as_str())
    };
    if find(&noext) == Some("application/octet-stream") {
        eprintln!("skipping: shared-mime-info's database isn't installed");
        return;
    }
    assert_eq!(find(&noext), Some("image/png"));
    assert_eq!(find(&mismatched), Some("image/png"));
    assert_eq!(find(&markdown), Some("text/markdown"));
    assert_eq!(find(&script), Some("text/x-shellscript"));
    assert_eq!(find(&odd), Some("text/plain"));
    // A file that disappeared doesn't fail the batch.
    assert_eq!(find(&missing), None);
    assert!(typed.iter().all(|typed| typed.mtime.is_some()));

    // The same through the background detector.
    let mut detector = Detector::new(|| {});
    let files = [noext.clone(), mismatched.clone(), markdown.clone()].map(|path| {
        let mtime = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        (path, mtime)
    });
    detector.show(&dir, &files);
    assert!(wait_for(&mut detector, |detector| files
        .iter()
        .all(|(path, mtime)| detector.get(path, *mtime).is_some())));
    assert_eq!(detector.get(&noext, files[0].1), Some("image/png"));
    assert_eq!(detector.get(&mismatched, files[1].1), Some("image/png"));
    assert_eq!(detector.get(&markdown, files[2].1), Some("text/markdown"));
    let _ = std::fs::remove_dir_all(dir);
}

/// Answers every file as `text/plain`, modified at second 5.
fn plain_at_five(paths: &[PathBuf]) -> Result<Vec<Typed>, Failure> {
    std::thread::sleep(Duration::from_millis(5));
    Ok(paths
        .iter()
        .map(|path| Typed {
            path: path.clone(),
            mtime: Some(5),
            content_type: "text/plain".into(),
        })
        .collect())
}

#[test]
fn answers_for_a_folder_the_user_left_are_ignored() {
    let mut detector = Detector::start(|| {}, plain_at_five);
    let old = (PathBuf::from("/old/a"), 5);
    let new = (PathBuf::from("/new/b"), 5);
    detector.show(Path::new("/old"), std::slice::from_ref(&old));
    // Give the worker time to answer for /old before the user moves on.
    std::thread::sleep(Duration::from_millis(50));
    detector.show(Path::new("/new"), std::slice::from_ref(&new));
    assert!(wait_for(&mut detector, |detector| detector
        .get(&new.0, 5)
        .is_some()));
    assert_eq!(detector.get(&old.0, 5), None);
    assert!(!detector.known.contains_key(&old.0));
}

#[test]
fn answers_about_an_older_version_of_a_file_are_ignored() {
    let mut detector = Detector::start(|| {}, plain_at_five);
    let folder = Path::new("/f");
    let current = (PathBuf::from("/f/edited"), 9);
    detector.show(folder, std::slice::from_ref(&current));
    assert!(wait_for(&mut detector, |detector| detector
        .asked
        .get(&current.0)
        .is_some_and(|(_, state)| *state == Asked::NoAnswer)
        || detector.known.contains_key(&current.0)));
    assert_eq!(detector.get(&current.0, 9), None);
    // The answer matches the listing once the file is listed at the time GIO saw.
    assert_eq!(detector.get(&current.0, 5), Some("text/plain"));
}

#[test]
fn re_listing_keeps_known_types_and_only_asks_about_new_files() {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    fn counting(paths: &[PathBuf]) -> Result<Vec<Typed>, Failure> {
        CALLS.fetch_add(paths.len() as u64, Ordering::SeqCst);
        plain_at_five(paths)
    }
    let mut detector = Detector::start(|| {}, counting);
    let folder = Path::new("/g");
    let a = (PathBuf::from("/g/a"), 5);
    let b = (PathBuf::from("/g/b"), 5);
    detector.show(folder, std::slice::from_ref(&a));
    assert!(wait_for(&mut detector, |detector| detector
        .get(&a.0, 5)
        .is_some()));
    detector.show(folder, &[a.clone(), b.clone()]);
    assert!(wait_for(&mut detector, |detector| detector
        .get(&b.0, 5)
        .is_some()));
    detector.show(folder, &[a.clone(), b.clone()]);
    std::thread::sleep(Duration::from_millis(50));
    detector.poll();
    assert_eq!(CALLS.load(Ordering::SeqCst), 2);
}

#[test]
fn a_missing_gio_switches_detection_off() {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    fn missing(_: &[PathBuf]) -> Result<Vec<Typed>, Failure> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        Err(Failure::Unavailable)
    }
    let mut detector = Detector::start(|| {}, missing);
    let folder = Path::new("/h");
    detector.show(folder, &[(PathBuf::from("/h/a.png"), 1)]);
    assert!(wait_for(&mut detector, |detector| detector.unavailable));
    detector.show(folder, &[(PathBuf::from("/h/b.png"), 1)]);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(detector.get(Path::new("/h/a.png"), 1), None);
    // Callers then fall back to the extension.
    assert_eq!(effective(None, Path::new("/h/a.png")), Some("image/png"));
}

#[test]
fn large_folders_are_typed_in_batches() {
    static LARGEST: AtomicU64 = AtomicU64::new(0);
    fn record(paths: &[PathBuf]) -> Result<Vec<Typed>, Failure> {
        LARGEST.fetch_max(paths.len() as u64, Ordering::SeqCst);
        plain_at_five(paths)
    }
    let mut detector = Detector::start(|| {}, record);
    let files = (0..300)
        .map(|index| (PathBuf::from(format!("/big/{index}")), 5))
        .collect::<Vec<_>>();
    detector.show(Path::new("/big"), &files);
    assert!(wait_for(&mut detector, |detector| files
        .iter()
        .all(|(path, mtime)| detector.get(path, *mtime).is_some())));
    assert_eq!(LARGEST.load(Ordering::SeqCst), BATCH as u64);
}
