//! Where Ferune is looking: a local path that `std::fs` handles, or a GIO URI (sftp://, smb://,
//! mtp://, …) that only the `gio` tool touches. Keeping the two apart is what stops remote
//! locations from ever reaching `std::fs`, where a URI would read as a relative local path.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Location {
    Local(PathBuf),
    /// A normalized URI (see [`normalize_uri`]) for anything other than a local `file://`.
    Remote(String),
}

impl Location {
    /// A location from a URI: `file:///…` becomes a local path; other `scheme://` URIs stay
    /// remote. `file://host/…` (a file on another machine) isn't supported.
    pub fn from_uri(uri: &str) -> Option<Location> {
        if uri_scheme(uri)? == "file" {
            return crate::local_file_uri(uri).map(Location::Local);
        }
        normalize_uri(uri).map(Location::Remote)
    }

    /// What the user typed in the location bar or passed on the command line.
    pub fn parse_input(input: &str, home: &Path) -> Option<Location> {
        let input = input.trim();
        if uri_scheme(input).is_some() {
            return Location::from_uri(input);
        }
        Some(Location::Local(if input == "~" {
            home.to_path_buf()
        } else if let Some(tail) = input.strip_prefix("~/") {
            home.join(tail)
        } else {
            PathBuf::from(input)
        }))
    }

    /// One line of the favorites file. Absolute paths (every line written before remote
    /// support) stay local; URIs are parsed as URIs.
    pub fn from_config_line(line: &str) -> Option<Location> {
        if line.starts_with('/') {
            Some(Location::Local(PathBuf::from(line)))
        } else if uri_scheme(line).is_some() {
            Location::from_uri(line)
        } else {
            Some(Location::Local(PathBuf::from(line)))
        }
    }

    pub fn to_config_line(&self) -> String {
        self.key()
    }

    pub fn as_local(&self) -> Option<&Path> {
        match self {
            Location::Local(path) => Some(path),
            Location::Remote(_) => None,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Location::Remote(_))
    }

    /// A stable string identity: the path for local locations, the URI for remote ones.
    pub fn key(&self) -> String {
        match self {
            Location::Local(path) => path.to_string_lossy().into_owned(),
            Location::Remote(uri) => uri.clone(),
        }
    }

    /// The URI to hand to `gio`.
    pub fn uri(&self) -> String {
        match self {
            Location::Local(path) => crate::file_uri(path),
            Location::Remote(uri) => uri.clone(),
        }
    }

    /// A short title: the last path segment, or the host/scheme at a remote root.
    pub fn name(&self) -> String {
        match self {
            Location::Local(path) => path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned(),
            Location::Remote(uri) => {
                let (root, segments) = split_remote(uri);
                match segments.last() {
                    Some(segment) => decode_lossy(segment),
                    None => root_label(root),
                }
            }
        }
    }

    /// The URI with percent-escapes decoded, for reading rather than editing.
    pub fn display_uri(&self) -> String {
        decode_lossy(&self.uri())
    }

    pub fn parent(&self) -> Option<Location> {
        match self {
            Location::Local(path) => path.parent().map(|parent| Location::Local(parent.into())),
            Location::Remote(uri) => {
                let (root, segments) = split_remote(uri);
                let (_, ancestors) = segments.split_last()?;
                Some(Location::Remote(format!("{root}{}", ancestors.join("/"))))
            }
        }
    }

    /// Breadcrumbs for a remote location: its root (host or scheme), then each folder.
    pub fn remote_crumbs(&self) -> Vec<(String, Location)> {
        let Location::Remote(uri) = self else {
            return Vec::new();
        };
        let (root, segments) = split_remote(uri);
        let mut crumbs = vec![(root_label(root), Location::Remote(root.to_owned()))];
        for index in 0..segments.len() {
            crumbs.push((
                decode_lossy(segments[index]),
                Location::Remote(format!("{root}{}", segments[..=index].join("/"))),
            ));
        }
        crumbs
    }
}

/// The lowercased scheme of a `scheme://…` URI.
pub fn uri_scheme(input: &str) -> Option<String> {
    let (scheme, _) = input.split_once("://")?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c));
    valid.then(|| scheme.to_ascii_lowercase())
}

/// Lowercases the scheme, percent-encodes bytes that can't appear raw in a URI (spaces,
/// non-ASCII, stray `%`) while keeping existing escapes, ends a bare root with `/`, and drops
/// trailing slashes elsewhere, so the same folder always compares equal.
pub fn normalize_uri(input: &str) -> Option<String> {
    let scheme = uri_scheme(input)?;
    let rest = &input[input.find("://")? + 3..];
    let mut uri = format!("{scheme}://");
    let bytes = rest.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        let escape = byte == b'%'
            && bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
            && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit);
        if escape || byte.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    let root_len = root_len(&uri);
    if uri.len() < root_len {
        uri.push('/');
    }
    while uri.len() > root_len && uri.ends_with('/') {
        uri.pop();
    }
    Some(uri)
}

/// Length of `scheme://authority/`, counting the slash even when the URI lacks it.
fn root_len(uri: &str) -> usize {
    let start = uri.find("://").map_or(0, |index| index + 3);
    uri[start..]
        .find('/')
        .map_or(uri.len() + 1, |slash| start + slash + 1)
}

/// `("sftp://host/", ["home", "me"])` for `sftp://host/home/me`.
fn split_remote(uri: &str) -> (&str, Vec<&str>) {
    let root_len = root_len(uri).min(uri.len());
    let segments = uri[root_len..]
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    (&uri[..root_len], segments)
}

fn root_label(root: &str) -> String {
    let (scheme, rest) = root.split_once("://").unwrap_or((root, ""));
    let authority = rest.trim_end_matches('/');
    if authority.is_empty() {
        format!("{scheme}:")
    } else {
        decode_lossy(authority)
    }
}

pub fn percent_decode(input: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(input.len());
    let input = input.as_bytes();
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            let hex = std::str::from_utf8(input.get(index + 1..index + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            bytes.push(input[index]);
            index += 1;
        }
    }
    Some(bytes)
}

pub fn decode_lossy(input: &str) -> String {
    percent_decode(input)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_else(|| input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(uri: &str) -> Location {
        Location::Remote(uri.into())
    }

    #[test]
    fn parses_file_and_gio_uris_and_paths() {
        let home = Path::new("/home/me");
        assert_eq!(
            Location::parse_input("file:///tmp/a%20b", home),
            Some(Location::Local("/tmp/a b".into()))
        );
        assert_eq!(Location::parse_input("file://server/share", home), None);
        assert_eq!(
            Location::parse_input("  SFTP://me@host/srv/My Files/  ", home),
            Some(remote("sftp://me@host/srv/My%20Files"))
        );
        assert_eq!(
            Location::parse_input("~/Documents", home),
            Some(Location::Local("/home/me/Documents".into()))
        );
        assert_eq!(
            Location::parse_input("/etc", home),
            Some(Location::Local("/etc".into()))
        );
        assert_eq!(
            Location::parse_input("smb:/not-a-uri", home),
            Some(Location::Local("smb:/not-a-uri".into()))
        );
    }

    #[test]
    fn normalizes_uris_so_the_same_folder_compares_equal() {
        assert_eq!(
            normalize_uri("smb://server").as_deref(),
            Some("smb://server/")
        );
        assert_eq!(normalize_uri("network:///").as_deref(), Some("network:///"));
        assert_eq!(
            normalize_uri("mtp://Pixel/Internal storage/DCIM/").as_deref(),
            Some("mtp://Pixel/Internal%20storage/DCIM")
        );
        // Existing escapes stay; stray percent signs and non-ASCII are encoded.
        assert_eq!(
            normalize_uri("sftp://h/na%C3%AFve/100%/ü").as_deref(),
            Some("sftp://h/na%C3%AFve/100%25/%C3%BC")
        );
        assert_eq!(normalize_uri("1http://x"), None);
        assert_eq!(normalize_uri("no scheme"), None);
    }

    #[test]
    fn remote_locations_have_names_parents_and_breadcrumbs() {
        let folder = remote("smb://nas/media/My%20Photos");
        assert_eq!(folder.name(), "My Photos");
        assert_eq!(folder.display_uri(), "smb://nas/media/My Photos");
        assert_eq!(folder.parent(), Some(remote("smb://nas/media")));
        assert_eq!(
            remote("smb://nas/media").parent(),
            Some(remote("smb://nas/"))
        );
        assert_eq!(remote("smb://nas/").parent(), None);
        assert_eq!(remote("network:///").name(), "network:");
        assert_eq!(
            folder.remote_crumbs(),
            vec![
                ("nas".into(), remote("smb://nas/")),
                ("media".into(), remote("smb://nas/media")),
                ("My Photos".into(), folder.clone()),
            ]
        );
    }

    #[test]
    fn favorites_lines_round_trip_and_stay_compatible() {
        for location in [
            Location::Local("/home/me/Documents".into()),
            remote("sftp://me@host/srv"),
        ] {
            assert_eq!(
                Location::from_config_line(&location.to_config_line()),
                Some(location)
            );
        }
        assert_eq!(
            Location::from_config_line("/srv/odd://name"),
            Some(Location::Local("/srv/odd://name".into()))
        );
    }
}
