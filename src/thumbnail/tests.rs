use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ferune-thumbnail-test-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn thumbnailer(exec: &[&str]) -> Thumbnailer {
    Thumbnailer {
        exec: exec.iter().map(|argument| argument.to_string()).collect(),
        mime_types: vec!["image/png".into()],
        try_exec: None,
    }
}

fn set_mtime(path: &Path, seconds: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
        .unwrap();
}

#[test]
fn names_cache_files_like_the_specification() {
    // The specification's own example.
    assert_eq!(
        cache_name("file:///home/jens/photos/me.png"),
        "c6ee772d9e49320e97ec29a7eb5b1697.png"
    );
}

#[test]
fn builds_uris_exactly_as_glib_does() {
    assert_eq!(
        thumbnail_uri(Path::new("/home/me/My Photos/a+b,(1)!.png")).as_deref(),
        Some("file:///home/me/My%20Photos/a+b,(1)!.png")
    );
    assert_eq!(
        thumbnail_uri(Path::new("/tmp/café #1 100%?[x]{y}.jpg")).as_deref(),
        Some("file:///tmp/caf%C3%A9%20%231%20100%25%3F%5Bx%5D%7By%7D.jpg")
    );
    assert_eq!(
        thumbnail_uri(Path::new("/srv/$&'*:=@_~-.x")).as_deref(),
        Some("file:///srv/$&'*:=@_~-.x")
    );
    // Non-UTF-8 bytes are encoded rather than lost.
    use std::os::unix::ffi::OsStrExt;
    let raw = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff.png"));
    assert_eq!(thumbnail_uri(raw).as_deref(), Some("file:///tmp/%FF.png"));
    assert_eq!(thumbnail_uri(Path::new("relative.png")), None);
}

#[test]
fn trusts_thumbnails_only_while_uri_mtime_and_size_match() {
    let uri = "file:///a.png";
    let fresh = pairs(&[("Thumb::URI", uri), ("Thumb::MTime", "1700000000")]);
    assert!(is_fresh(&fresh, uri, 1_700_000_000, 10));
    assert!(!is_fresh(&fresh, uri, 1_700_000_001, 10), "file changed");
    assert!(!is_fresh(&fresh, "file:///b.png", 1_700_000_000, 10));
    let with_size = pairs(&[
        ("Thumb::URI", uri),
        ("Thumb::MTime", "1700000000"),
        ("Thumb::Size", "10"),
    ]);
    assert!(is_fresh(&with_size, uri, 1_700_000_000, 10));
    assert!(!is_fresh(&with_size, uri, 1_700_000_000, 11));
    // Fractional seconds from other writers compare by whole seconds.
    let fractional = pairs(&[("Thumb::URI", uri), ("Thumb::MTime", "1700000000.25")]);
    assert!(is_fresh(&fractional, uri, 1_700_000_000, 10));
    assert!(!is_fresh(&pairs(&[("Thumb::URI", uri)]), uri, 0, 0));
    assert!(!is_fresh(
        &pairs(&[("Thumb::URI", uri), ("Thumb::MTime", "soon")]),
        uri,
        0,
        0
    ));
}

#[test]
fn picks_the_covering_size_and_looks_in_larger_ones_too() {
    assert_eq!(Flavor::for_pixels(40), Flavor::Normal);
    assert_eq!(Flavor::for_pixels(128), Flavor::Normal);
    assert_eq!(Flavor::for_pixels(129), Flavor::Large);
    assert_eq!(Flavor::for_pixels(5000), Flavor::XxLarge);
    assert_eq!(
        Flavor::Large.lookup_order().collect::<Vec<_>>(),
        [Flavor::Large, Flavor::XLarge, Flavor::XxLarge]
    );
}

#[test]
fn guesses_thumbnailable_types_and_skips_the_cache_and_gvfs() {
    let guess = |path: &str| mime_for(None, Path::new(path));
    assert_eq!(guess("/a/B.JPG").as_deref(), Some("image/jpeg"));
    assert_eq!(guess("/a/b.pdf").as_deref(), Some("application/pdf"));
    assert_eq!(guess("/a/b.mkv").as_deref(), Some("video/x-matroska"));
    assert_eq!(guess("/a/png"), None);
    // A detected type wins over the name; "don't know" answers fall back to it.
    assert_eq!(
        mime_for(Some("image/png"), Path::new("/a/photo.backup")).as_deref(),
        Some("image/png")
    );
    assert_eq!(
        mime_for(Some("application/pdf"), Path::new("/a/scan.png")).as_deref(),
        Some("application/pdf")
    );
    assert_eq!(
        mime_for(Some("application/octet-stream"), Path::new("/a/b.jpg")).as_deref(),
        Some("image/jpeg")
    );
    assert!(worth_trying("image/png") && worth_trying("application/pdf"));
    assert!(worth_trying("image/svg+xml") && worth_trying("video/mp2t"));
    assert!(!worth_trying("text/plain") && !worth_trying("application/json"));
    assert!(!worth_trying("text/x-python3"));
    let root = Path::new("/home/me/.cache/thumbnails");
    assert!(excluded(&root.join("normal/x.png"), root));
    assert!(excluded(
        Path::new("/run/user/1000/gvfs/sftp:host=h/x.png"),
        root
    ));
    assert!(!excluded(Path::new("/home/me/x.png"), root));
}

/// A source PNG, a cache in a temp folder, and a "thumbnailer" that copies its input: the
/// whole generate → store → look up → invalidate cycle without touching the real cache.
#[test]
fn generates_stores_finds_and_invalidates_thumbnails() {
    let dir = temp_dir("cycle");
    let source = dir.join("photo one.png");
    fs::write(&source, png::marker(&[("Source", "yes")])).unwrap();
    set_mtime(&source, 1_700_000_000);
    let cache = Cache::new(
        dir.join("thumbnails"),
        dir.join("scratch"),
        vec![thumbnailer(&["cp", "%i", "%o"])],
    );
    let uri = thumbnail_uri(&source).unwrap();
    let stored = dir.join("thumbnails/normal").join(cache_name(&uri));

    assert!(matches!(
        cache.lookup(&source, None, Flavor::Normal),
        Lookup::Generate
    ));
    assert!(matches!(
        cache.generate(&source, None, Flavor::Normal),
        Outcome::Ready(_, (1, 1))
    ));
    let text = png::text(&fs::read(&stored).unwrap()).unwrap();
    assert!(is_fresh(
        &text,
        &uri,
        1_700_000_000,
        fs::metadata(&source).unwrap().len()
    ));
    assert!(text.contains(&("Source".into(), "yes".into())));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&stored).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(matches!(
        cache.lookup(&source, None, Flavor::Normal),
        Lookup::Found(Outcome::Ready(..))
    ));

    // A thumbnail in a larger size serves a smaller request.
    let large = Cache::new(dir.join("thumbnails"), dir.join("scratch"), Vec::new());
    fs::create_dir_all(dir.join("thumbnails/large")).unwrap();
    fs::rename(&stored, dir.join("thumbnails/large").join(cache_name(&uri))).unwrap();
    assert!(matches!(
        large.lookup(&source, None, Flavor::Normal),
        Lookup::Found(Outcome::Ready(..))
    ));

    // Editing the file makes the cached thumbnail stale.
    set_mtime(&source, 1_700_000_100);
    assert!(matches!(
        cache.lookup(&source, None, Flavor::Normal),
        Lookup::Generate
    ));
    // Without a thumbnailer, a stale thumbnail falls back to the icon.
    assert!(matches!(
        large.lookup(&source, None, Flavor::Normal),
        Lookup::Found(Outcome::Unavailable)
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn records_failures_until_the_file_changes() {
    let dir = temp_dir("failure");
    let source = dir.join("broken.png");
    fs::write(&source, b"not really a png").unwrap();
    set_mtime(&source, 1_700_000_000);
    let failing = Cache::new(
        dir.join("thumbnails"),
        dir.join("scratch"),
        vec![thumbnailer(&["false", "%i", "%o"])],
    );
    assert!(matches!(
        failing.generate(&source, None, Flavor::Normal),
        Outcome::Unavailable
    ));
    let marker = failing
        .fail_dir
        .join(cache_name(&thumbnail_uri(&source).unwrap()));
    assert!(marker.is_file());
    assert!(matches!(
        failing.lookup(&source, None, Flavor::Normal),
        Lookup::Found(Outcome::Unavailable)
    ));
    // A thumbnailer that "succeeds" without writing a PNG is a failure too.
    let copying = Cache::new(
        dir.join("thumbnails"),
        dir.join("scratch"),
        vec![thumbnailer(&["cp", "%i", "%o"])],
    );
    set_mtime(&source, 1_700_000_100);
    assert!(matches!(
        copying.lookup(&source, None, Flavor::Normal),
        Lookup::Generate
    ));
    assert!(matches!(
        copying.generate(&source, None, Flavor::Normal),
        Outcome::Unavailable
    ));
    assert!(
        !dir.join("scratch").exists(),
        "scratch output is cleaned up"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn unsupported_and_missing_files_need_no_thumbnailer() {
    let dir = temp_dir("unsupported");
    let notes = dir.join("notes.txt");
    fs::write(&notes, "hello").unwrap();
    let cache = Cache::new(dir.join("thumbnails"), dir.join("scratch"), Vec::new());
    for path in [notes, dir.join("gone.png"), dir.clone()] {
        assert!(matches!(
            cache.lookup(&path, None, Flavor::Normal),
            Lookup::Found(Outcome::Unavailable)
        ));
    }
    let _ = fs::remove_dir_all(dir);
}

/// Reads (never writes) this machine's real thumbnail cache: every thumbnail another program
/// stored for a local file that still exists must sit under the name Ferune computes.
#[test]
#[ignore = "reads the user's real thumbnail cache"]
fn agrees_with_thumbnails_other_programs_wrote() {
    let Some(root) = cache_root() else { return };
    let mut checked = 0;
    for flavor in Flavor::ALL {
        let Ok(items) = fs::read_dir(root.join(flavor.dir_name())) else {
            continue;
        };
        for item in items.filter_map(Result::ok).take(200) {
            let Some(text) = read_limited(&item.path()).as_deref().and_then(png::text) else {
                continue;
            };
            let Some((_, uri)) = text.iter().find(|(key, _)| key == "Thumb::URI") else {
                continue;
            };
            let Some(path) = crate::local_file_uri(uri).filter(|path| path.exists()) else {
                continue;
            };
            assert_eq!(thumbnail_uri(&path).as_deref(), Some(uri.as_str()));
            assert_eq!(item.file_name().to_string_lossy(), cache_name(uri));
            checked += 1;
        }
    }
    eprintln!("checked {checked} existing thumbnails");
}

/// Runs the system's real thumbnailers against a temp cache, when one handles PNG.
#[test]
#[ignore = "runs installed thumbnailers"]
fn generates_with_an_installed_thumbnailer() {
    let installed = thumbnailer::discover();
    if thumbnailer::select(&installed, "image/png").is_none() {
        eprintln!("no PNG thumbnailer installed; skipping");
        return;
    }
    let dir = temp_dir("system");
    let source = dir.join("pixel.png");
    // A real 1×1 PNG with compressed data, which the thumbnailer must decode.
    fs::write(&source, png::marker(&[])).unwrap();
    let cache = Cache::new(dir.join("thumbnails"), dir.join("scratch"), installed);
    assert!(matches!(
        cache.generate(&source, None, Flavor::Normal),
        Outcome::Ready(..)
    ));
    assert!(matches!(
        cache.lookup(&source, None, Flavor::Normal),
        Lookup::Found(Outcome::Ready(..))
    ));
    let _ = fs::remove_dir_all(dir);
}
