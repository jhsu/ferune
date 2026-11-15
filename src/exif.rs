//! Optional EXIF details for image previews. ExifTool is run directly on a worker thread;
//! location and other sensitive EXIF tags (notably GPS) are deliberately not requested.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT: u64 = 16 * 1024;
const TAGS: [&str; 9] = [
    "-Make",
    "-Model",
    "-LensModel",
    "-DateTimeOriginal",
    "-CreateDate",
    "-ExposureTime",
    "-FNumber",
    "-ISO",
    "-FocalLength",
];

/// Reads a short, selected set of EXIF fields. Returns `None` if ExifTool is missing,
/// the file has no useful metadata, or the helper fails.
pub fn read(path: &Path) -> Option<String> {
    let mut child = Command::new("exiftool")
        .args(["-s"])
        .args(TAGS)
        .arg("--")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.take(MAX_OUTPUT).read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(15));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let output = receiver.recv_timeout(Duration::from_secs(1)).ok()?;
    format_fields(&String::from_utf8_lossy(&output))
}

/// Formats ExifTool's `-s` tag/value lines, dropping empty and unrequested fields.
pub fn format_fields(output: &str) -> Option<String> {
    let mut make = None;
    let mut model = None;
    let mut lens = None;
    let mut captured = None;
    let mut exposure = None;
    let mut aperture = None;
    let mut iso = None;
    let mut focal = None;
    for line in output.lines() {
        let Some((tag, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match tag.trim() {
            "Make" => make = Some(value),
            "Model" => model = Some(value),
            "LensModel" => lens = Some(value),
            "DateTimeOriginal" | "CreateDate" if captured.is_none() => captured = Some(value),
            "ExposureTime" => exposure = Some(value),
            "FNumber" => aperture = Some(value),
            "ISO" => iso = Some(value),
            "FocalLength" => focal = Some(value),
            _ => {}
        }
    }

    let mut rows = Vec::new();
    let camera = match (make, model) {
        (Some(make), Some(model)) if !model.starts_with(make) => format!("{make} {model}"),
        (_, Some(model)) => model.to_owned(),
        (Some(make), None) => make.to_owned(),
        (None, None) => String::new(),
    };
    push(&mut rows, "Camera", &camera);
    push(&mut rows, "Lens", lens.unwrap_or(""));
    push(&mut rows, "Captured", captured.unwrap_or(""));
    let settings = [
        exposure.map(|value| value.to_owned()),
        aperture.map(|value| format!("ƒ/{value}")),
        iso.map(|value| format!("ISO {value}")),
        focal.map(|value| value.to_owned()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !settings.is_empty() {
        rows.push(format!("Exposure: {}", settings.join(" · ")));
    }
    (!rows.is_empty()).then(|| rows.join("\n"))
}

fn push(rows: &mut Vec<String>, label: &str, value: &str) {
    if !value.is_empty() {
        rows.push(format!("{label}: {value}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_capture_details_without_location_or_other_sensitive_tags() {
        let details = format_fields(
            "Make : Canon\nModel : Canon EOS R5\nLensModel : RF 50mm F1.8\n\
             DateTimeOriginal : 2025:02:03 04:05:06\nExposureTime : 1/250 sec\n\
             FNumber : 2.8\nISO : 100\nFocalLength : 50.0 mm\nGPSLatitude : 44.0",
        )
        .unwrap();
        assert_eq!(
            details,
            "Camera: Canon EOS R5\nLens: RF 50mm F1.8\nCaptured: 2025:02:03 04:05:06\n\
             Exposure: 1/250 sec · ƒ/2.8 · ISO 100 · 50.0 mm"
        );
        assert!(!details.contains("GPS"));
    }

    #[test]
    fn returns_no_details_when_no_requested_tags_are_present() {
        assert_eq!(
            format_fields("FileName : photo.png\nGPSLatitude : 44"),
            None
        );
    }
}
