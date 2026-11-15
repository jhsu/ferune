//! Removable drives through UDisks2's `udisksctl` tool, so Ferune can list, mount, unmount and
//! safely remove USB sticks, SD cards and other media without linking a D-Bus library.
//! udisksctl has no eject command, so Eject calls the same UDisks2 `Drive.Eject` method
//! through GLib's `gdbus` tool. Every call here blocks on a subprocess (and possibly a polkit
//! prompt); run them off the UI thread.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DUMP_TIMEOUT: Duration = Duration::from_secs(15);
/// Long enough for the user to answer a polkit authentication dialog.
const ACTION_TIMEOUT: Duration = Duration::from_secs(120);
/// How long `watch` waits for a burst of change notifications to settle before re-listing.
const SETTLE: Duration = Duration::from_millis(400);

const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const ENCRYPTED: &str = "org.freedesktop.UDisks2.Encrypted";

/// Mount points that mark a drive as part of the running system, whatever UDisks hints say.
const SYSTEM_MOUNTS: [&str; 8] = [
    "/",
    "/boot",
    "/boot/efi",
    "/efi",
    "/usr",
    "/var",
    "/home",
    "/opt",
];

/// A mountable file system on a removable drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// UDisks object path of the block device, e.g. `/org/freedesktop/UDisks2/block_devices/sdb1`.
    pub object: String,
    /// Device file, e.g. `/dev/sdb1`.
    pub device: String,
    /// The file system label (or UDisks' name hint); `None` when the volume has neither.
    pub label: Option<String>,
    pub fs_type: String,
    pub size: u64,
    pub read_only: bool,
    /// Where it's mounted now: by Ferune, another file manager, an automounter, or fstab.
    pub mount_points: Vec<PathBuf>,
    /// The drive's UDisks object path; several volumes (partitions) can share one drive.
    pub drive: String,
    pub drive_name: String,
    pub can_power_off: bool,
    pub ejectable: bool,
}

impl Volume {
    pub fn is_mounted(&self) -> bool {
        !self.mount_points.is_empty()
    }
}

/// What happens to a drive once its file systems are unmounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// Spin down and cut power (USB sticks, USB disks, card readers).
    PowerOff,
    /// Eject the medium (optical discs, some card readers).
    Eject,
}

/// One `udisksctl`/`gdbus` command in a safe-removal plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Unmount(String),
    Lock(String),
    PowerOff(String),
    Eject(String),
}

/// Interfaces → property → value, for one UDisks object.
type Interfaces = BTreeMap<String, BTreeMap<String, String>>;

/// The parsed `udisksctl dump`: object path → interfaces.
#[derive(Debug, Default)]
pub struct Objects(BTreeMap<String, Interfaces>);

impl Objects {
    fn property(&self, object: &str, interface: &str, name: &str) -> Option<&str> {
        self.0
            .get(object)?
            .get(interface)?
            .get(name)
            .map(String::as_str)
    }

    fn has(&self, object: &str, interface: &str) -> bool {
        self.0
            .get(object)
            .is_some_and(|interfaces| interfaces.contains_key(interface))
    }

    fn flag(&self, object: &str, interface: &str, name: &str) -> bool {
        self.property(object, interface, name) == Some("true")
    }

    /// An object-path property, or `None` for the null path `/`.
    fn link(&self, object: &str, interface: &str, name: &str) -> Option<&str> {
        self.property(object, interface, name)
            .filter(|path| !path.is_empty() && *path != "/")
    }

    fn mount_points(&self, block: &str) -> Vec<PathBuf> {
        self.property(block, FILESYSTEM, "MountPoints")
            .map(|points| {
                points
                    .lines()
                    .filter(|point| !point.is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn blocks(&self) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .filter(|(_, interfaces)| interfaces.contains_key(BLOCK))
            .map(|(path, _)| path.as_str())
    }

    /// The drive a block device lives on. An unlocked encrypted volume's cleartext device has
    /// no drive of its own; follow it to the encrypted partition underneath.
    fn drive_of(&self, block: &str) -> Option<&str> {
        let mut block = block;
        for _ in 0..4 {
            if let Some(drive) = self.link(block, BLOCK, "Drive") {
                return Some(drive);
            }
            block = self.link(block, BLOCK, "CryptoBackingDevice")?;
        }
        None
    }

    /// Block devices on `drive`, including cleartext devices of encrypted partitions on it.
    fn blocks_on<'a>(&'a self, drive: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.blocks()
            .filter(move |block| self.drive_of(block) == Some(drive))
    }

    fn hosts_system_mount(&self, drive: &str) -> bool {
        self.blocks_on(drive).any(|block| {
            self.mount_points(block).iter().any(|point| {
                SYSTEM_MOUNTS
                    .iter()
                    .any(|system| point == &PathBuf::from(system))
            })
        })
    }

    fn drive_name(&self, drive: &str) -> String {
        let field = |name| {
            self.property(drive, DRIVE, name)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        match (field("Vendor"), field("Model")) {
            (Some(vendor), Some(model)) if !model.starts_with(vendor) => {
                format!("{vendor} {model}")
            }
            (_, Some(model)) => model.to_owned(),
            (Some(vendor), None) => vendor.to_owned(),
            (None, None) => "Removable drive".into(),
        }
    }

    /// UDisks describes removable media with `Removable`, `MediaRemovable` or `Ejectable`.
    /// Fixed external USB drives may have all three false, but are still appropriate to show
    /// and safely remove while connected over USB/SDIO/FireWire.
    fn is_removable_drive(&self, drive: &str) -> bool {
        self.flag(drive, DRIVE, "Removable")
            || self.flag(drive, DRIVE, "MediaRemovable")
            || self.flag(drive, DRIVE, "Ejectable")
            || self
                .property(drive, DRIVE, "ConnectionBus")
                .is_some_and(|bus| {
                    matches!(
                        bus.to_ascii_lowercase().as_str(),
                        "usb" | "sdio" | "firewire"
                    )
                })
    }
}

/// Parses `udisksctl dump`. Objects start at column 0, interfaces are indented two spaces,
/// and properties four. A multi-valued property (mount points, symlinks) puts each further
/// value on its own, more deeply indented line; those become newline-separated values.
pub fn parse_dump(output: &str) -> Objects {
    let mut objects: BTreeMap<String, Interfaces> = BTreeMap::new();
    let mut object: Option<String> = None;
    let mut interface: Option<String> = None;
    let mut property: Option<String> = None;
    for line in output.lines() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let content = line[indent..].trim_end();
        if content.is_empty() {
            continue;
        }
        match indent {
            0 => {
                object = content
                    .strip_suffix(':')
                    .filter(|path| path.starts_with('/'))
                    .map(str::to_owned);
                interface = None;
                property = None;
                if let Some(path) = &object {
                    objects.entry(path.clone()).or_default();
                }
            }
            2 => {
                interface = content.strip_suffix(':').map(str::to_owned);
                property = None;
                if let (Some(path), Some(name)) = (&object, &interface) {
                    objects
                        .entry(path.clone())
                        .or_default()
                        .entry(name.clone())
                        .or_default();
                }
            }
            4 => {
                let (Some(path), Some(name)) = (&object, &interface) else {
                    continue;
                };
                let (key, value) = content.split_once(':').unwrap_or((content, ""));
                let value = value.trim_start();
                // Object paths print quoted: '/org/freedesktop/UDisks2/drives/X'.
                let value = value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
                    .filter(|value| value.starts_with('/'))
                    .unwrap_or(value);
                objects
                    .entry(path.clone())
                    .or_default()
                    .entry(name.clone())
                    .or_default()
                    .insert(key.to_owned(), value.to_owned());
                property = Some(key.to_owned());
            }
            _ => {
                let (Some(path), Some(name), Some(key)) = (&object, &interface, &property) else {
                    continue;
                };
                if let Some(value) = objects
                    .get_mut(path)
                    .and_then(|interfaces| interfaces.get_mut(name))
                    .and_then(|properties| properties.get_mut(key))
                {
                    if !value.is_empty() {
                        value.push('\n');
                    }
                    value.push_str(content);
                }
            }
        }
    }
    Objects(objects)
}

/// The file systems Ferune lists as removable. Like GNOME's volume monitor it trusts UDisks'
/// `HintSystem`/`HintIgnore`, and it also skips any drive that hosts `/`, `/boot`, `/home`
/// or a similar system mount, so internal disks never show up even when hints are missing.
/// Locked encrypted partitions, swap and partition tables have no file system and are left
/// out; loop devices have no drive and are left out too.
pub fn removable_volumes(objects: &Objects) -> Vec<Volume> {
    let mut volumes = objects
        .blocks()
        .filter(|block| objects.has(block, FILESYSTEM))
        .filter(|block| {
            !objects.flag(block, BLOCK, "HintIgnore") && !objects.flag(block, BLOCK, "HintSystem")
        })
        .filter_map(|block| {
            let drive = objects.drive_of(block)?;
            if !objects.has(drive, DRIVE)
                || !objects.is_removable_drive(drive)
                || objects.hosts_system_mount(drive)
            {
                return None;
            }
            let text = |name| {
                objects
                    .property(block, BLOCK, name)
                    .unwrap_or("")
                    .to_owned()
            };
            let label = [text("HintName"), text("IdLabel")]
                .into_iter()
                .find(|label| !label.trim().is_empty());
            Some(Volume {
                object: block.to_owned(),
                device: Some(text("PreferredDevice"))
                    .filter(|device| !device.is_empty())
                    .unwrap_or_else(|| text("Device")),
                label,
                fs_type: text("IdType"),
                size: text("Size").parse().unwrap_or(0),
                read_only: objects.flag(block, BLOCK, "ReadOnly"),
                mount_points: objects.mount_points(block),
                drive: drive.to_owned(),
                drive_name: objects.drive_name(drive),
                can_power_off: objects.flag(drive, DRIVE, "CanPowerOff"),
                ejectable: objects.flag(drive, DRIVE, "Ejectable"),
            })
        })
        .collect::<Vec<_>>();
    // Keep a drive's partitions together, in device order (sdb1, sdb2, …, sdc1).
    volumes.sort_by(|a, b| {
        (&a.drive, a.device.len(), &a.device).cmp(&(&b.drive, b.device.len(), &b.device))
    });
    volumes
}

/// The commands that safely remove `drive`: unmount every file system on it (including ones
/// inside unlocked encrypted partitions), lock those encrypted partitions, then power the
/// drive off or eject its medium. `None` when the drive is gone or can't do `removal`.
pub fn plan_removal(objects: &Objects, drive: &str, removal: Removal) -> Option<Vec<Step>> {
    if !objects.has(drive, DRIVE)
        || !objects.is_removable_drive(drive)
        || objects.hosts_system_mount(drive)
    {
        return None;
    }
    let device = |block: &str| {
        objects
            .property(block, BLOCK, "Device")
            .unwrap_or_default()
            .to_owned()
    };
    let blocks = objects.blocks_on(drive).collect::<Vec<_>>();
    let mut steps = blocks
        .iter()
        .filter(|block| !objects.mount_points(block).is_empty())
        .map(|block| Step::Unmount(device(block)))
        .collect::<Vec<_>>();
    steps.extend(
        blocks
            .iter()
            .filter(|block| objects.link(block, ENCRYPTED, "CleartextDevice").is_some())
            .map(|block| Step::Lock(device(block))),
    );
    let finish = match removal {
        Removal::PowerOff if objects.flag(drive, DRIVE, "CanPowerOff") => {
            Step::PowerOff(drive.to_owned())
        }
        Removal::Eject if objects.flag(drive, DRIVE, "Ejectable") => Step::Eject(drive.to_owned()),
        _ => return None,
    };
    steps.push(finish);
    Some(steps)
}

/// Lists removable volumes. Fails when udisksctl is missing or the UDisks daemon isn't
/// reachable on the system bus.
pub fn list_volumes() -> Result<Vec<Volume>, String> {
    dump().map(|objects| removable_volumes(&objects))
}

fn dump() -> Result<Objects, String> {
    run("udisksctl", &["dump"], DUMP_TIMEOUT).map(|output| parse_dump(&output))
}

/// Mounts `volume` (unless something already did) and returns where it's mounted. UDisks picks
/// the mount point, normally `/run/media/$USER/LABEL`.
pub fn mount(volume: &Volume) -> Result<PathBuf, String> {
    let message = match run(
        "udisksctl",
        &["mount", "--block-device", &volume.device],
        ACTION_TIMEOUT,
    ) {
        Ok(message) => message,
        // Mounted meanwhile (by an automounter, another file manager, a second click).
        Err(error) if error.contains("already mounted") => String::new(),
        Err(error) => return Err(error),
    };
    // The daemon's own record of the mount point is authoritative; the message is a fallback.
    dump()
        .ok()
        .and_then(|objects| objects.mount_points(&volume.object).into_iter().next())
        .or_else(|| parse_mounted_path(&message))
        .ok_or_else(|| format!("{} was mounted, but UDisks didn't say where", volume.device))
}

/// `Mounted /dev/sdb1 at /run/media/me/STICK` (older udisksctl versions end with a period).
pub fn parse_mounted_path(message: &str) -> Option<PathBuf> {
    let line = message.lines().find(|line| line.starts_with("Mounted "))?;
    let (_, path) = line.split_once(" at ")?;
    let path = PathBuf::from(path.trim_end());
    if path.is_dir() {
        return Some(path);
    }
    let text = path.to_str()?;
    Some(PathBuf::from(text.strip_suffix('.').unwrap_or(text))).filter(|path| path.is_absolute())
}

pub fn unmount(volume: &Volume) -> Result<(), String> {
    match run(
        "udisksctl",
        &["unmount", "--block-device", &volume.device],
        ACTION_TIMEOUT,
    ) {
        Err(error) if !error.contains("not mounted") => Err(error),
        _ => Ok(()),
    }
}

/// Unmounts everything on the drive, then powers it off or ejects it. Stops at the first step
/// that fails (typically "target is busy" because a program still has a file open), so the
/// drive is never powered off with a file system still mounted.
pub fn remove_drive(drive: &str, removal: Removal) -> Result<(), String> {
    let objects = dump()?;
    let steps = plan_removal(&objects, drive, removal).ok_or_else(|| match removal {
        Removal::PowerOff => "This drive can't be powered off".to_owned(),
        Removal::Eject => "This drive can't be ejected".to_owned(),
    })?;
    for step in steps {
        let result = match &step {
            Step::Unmount(device) => run(
                "udisksctl",
                &["unmount", "--block-device", device],
                ACTION_TIMEOUT,
            )
            .map(drop),
            Step::Lock(device) => run(
                "udisksctl",
                &["lock", "--block-device", device],
                ACTION_TIMEOUT,
            )
            .map(drop),
            Step::PowerOff(drive) => run(
                "udisksctl",
                &["power-off", "--object-path", drive],
                ACTION_TIMEOUT,
            )
            .map(drop),
            Step::Eject(drive) => run(
                "gdbus",
                &[
                    "call",
                    "--system",
                    "--dest",
                    "org.freedesktop.UDisks2",
                    "--object-path",
                    drive,
                    "--method",
                    "org.freedesktop.UDisks2.Drive.Eject",
                    "{}",
                ],
                ACTION_TIMEOUT,
            )
            .map(drop),
        };
        // A file system that went away on its own is fine; anything else stops the removal.
        match result {
            Err(error) if matches!(step, Step::Unmount(_)) && error.contains("not mounted") => {}
            Err(error) => return Err(error),
            Ok(()) => {}
        }
    }
    Ok(())
}

/// Watches UDisks for added, removed and changed devices and calls `changed` with a fresh
/// volume list once each burst of notifications settles. Blocks for as long as
/// `udisksctl monitor` runs, restarting it if the daemon restarts; returns at once when
/// udisksctl isn't installed. Run it on its own thread.
pub fn watch(mut changed: impl FnMut(Result<Vec<Volume>, String>)) {
    loop {
        let mut command = Command::new("udisksctl");
        command
            .arg("monitor")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("LC_MESSAGES", "C");
        let Ok(mut child) = command.spawn() else {
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            return;
        };
        let (lines_tx, lines_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line.is_err() || lines_tx.send(()).is_err() {
                    break;
                }
            }
        });
        // Coalesce each burst (one plug-in announces several objects) into a single re-list.
        // The startup banner triggers one too, which also covers a daemon restart.
        while lines_rx.recv().is_ok() {
            while lines_rx.recv_timeout(SETTLE).is_ok() {}
            changed(list_volumes());
        }
        let _ = child.kill();
        let _ = child.wait();
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// Runs a tool with a deadline and returns its stdout, or its cleaned-up error message.
/// Messages stay in English (only `LC_MESSAGES` changes) because callers recognize "already
/// mounted" and "not mounted" by their text.
fn run(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(all) = std::env::var_os("LC_ALL") {
        command.env_remove("LC_ALL").env("LC_CTYPE", all);
    }
    command.env("LC_MESSAGES", "C");
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            match program {
                "udisksctl" => "The `udisksctl` tool isn't installed (it ships with UDisks2)",
                _ => "The `gdbus` tool isn't installed (it ships with GLib)",
            }
            .to_owned()
        } else {
            format!("Couldn't run {program}: {error}")
        }
    })?;
    let read = |mut pipe: Box<dyn Read + Send>| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = sender.send(bytes);
        });
        receiver
    };
    let stdout = child.stdout.take().map(|pipe| read(Box::new(pipe)));
    let stderr = child.stderr.take().map(|pipe| read(Box::new(pipe)));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(15)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Timed out after {} seconds waiting for {program}",
                    timeout.as_secs()
                ));
            }
            Err(error) => return Err(format!("Couldn't wait for {program}: {error}")),
        }
    };
    let collect = |receiver: Option<mpsc::Receiver<Vec<u8>>>| {
        receiver
            .and_then(|receiver| receiver.recv_timeout(Duration::from_secs(2)).ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    };
    let stdout = collect(stdout);
    if status.success() {
        Ok(stdout)
    } else {
        Err(clean_error(&collect(stderr)))
    }
}

/// `Error unmounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.DeviceBusy: Error
/// unmounting /dev/sdb1: target is busy` → `Error unmounting /dev/sdb1: target is busy`.
/// udisksctl may first complain that it can't start a terminal polkit agent; only the last
/// line is the real error.
pub fn clean_error(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("");
    let message = match line.find("GDBus.Error:") {
        Some(start) => {
            let (before, error) = line.split_at(start);
            let detail = error.split_once(": ").map_or("", |(_, detail)| detail);
            let before = before.trim_end().trim_end_matches(':');
            // UDisks repeats "Error unmounting /dev/sdb1:" inside the detail; say it once.
            match detail
                .strip_prefix(before)
                .map(|rest| rest.trim_start_matches(": "))
            {
                Some(rest) if !before.is_empty() => format!("{before}: {rest}"),
                _ if before.is_empty() => detail.to_owned(),
                _ => format!("{before}: {detail}"),
            }
        }
        None => line.to_owned(),
    };
    let message = message.trim_end_matches('.').replace('`', "'");
    if message.contains("NotAuthorized") || message.contains("Not authorized") {
        format!("{message} (no polkit authentication agent answered, or access was denied)")
    } else if message.is_empty() {
        "The command failed without an error message".into()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from real `udisksctl dump` output: an internal NVMe system disk, a USB stick
    /// with two partitions (one mounted by another program, with a label containing spaces),
    /// a USB drive whose UDisks hints are missing but which holds `/`, an SD card in a USB
    /// reader with an unlocked LUKS partition, and a loop device.
    const DUMP: &str = "\
/org/freedesktop/UDisks2/Manager:
  org.freedesktop.UDisks2.Manager:
    SupportedFilesystems:       ext2
                                vfat
    Version:                    2.12.0

/org/freedesktop/UDisks2/block_devices/nvme0n1p2:
  org.freedesktop.UDisks2.Block:
    CryptoBackingDevice:        '/'
    Device:                     /dev/nvme0n1p2
    Drive:                      '/org/freedesktop/UDisks2/drives/Internal'
    HintIgnore:                 false
    HintName:
    HintSystem:                 true
    IdLabel:                    Abyss
    IdType:                     ntfs
    IdUsage:                    filesystem
    PreferredDevice:            /dev/nvme0n1p2
    ReadOnly:                   false
    Size:                       2000381018112
    Symlinks:                   /dev/disk/by-id/nvme-x-part2
                                /dev/disk/by-path/pci-0000:05:00.0-nvme-1-part2
  org.freedesktop.UDisks2.Filesystem:
    MountPoints:        /mnt/media
    Size:               0

/org/freedesktop/UDisks2/block_devices/sdb:
  org.freedesktop.UDisks2.Block:
    CryptoBackingDevice:        '/'
    Device:                     /dev/sdb
    Drive:                      '/org/freedesktop/UDisks2/drives/Kingston'
    HintIgnore:                 false
    HintSystem:                 false
    IdUsage:
    PreferredDevice:            /dev/sdb
    Size:                       31000000000
  org.freedesktop.UDisks2.PartitionTable:
    Partitions:         /org/freedesktop/UDisks2/block_devices/sdb1
                        /org/freedesktop/UDisks2/block_devices/sdb2
    Type:               dos

/org/freedesktop/UDisks2/block_devices/sdb2:
  org.freedesktop.UDisks2.Block:
    CryptoBackingDevice:        '/'
    Device:                     /dev/sdb2
    Drive:                      '/org/freedesktop/UDisks2/drives/Kingston'
    HintIgnore:                 false
    HintName:
    HintSystem:                 false
    IdLabel:
    IdType:                     ext4
    IdUsage:                    filesystem
    PreferredDevice:            /dev/sdb2
    ReadOnly:                   false
    Size:                       15000000000
  org.freedesktop.UDisks2.Filesystem:
    MountPoints:
    Size:               0
  org.freedesktop.UDisks2.Partition:
    Number:             2

/org/freedesktop/UDisks2/block_devices/sdb1:
  org.freedesktop.UDisks2.Block:
    CryptoBackingDevice:        '/'
    Device:                     /dev/sdb1
    Drive:                      '/org/freedesktop/UDisks2/drives/Kingston'
    HintIgnore:                 false
    HintName:
    HintSystem:                 false
    IdLabel:                    MY STICK
    IdType:                     vfat
    IdUsage:                    filesystem
    PreferredDevice:            /dev/sdb1
    ReadOnly:                   true
    Size:                       16000000000
  org.freedesktop.UDisks2.Filesystem:
    MountPoints:        /run/media/me/MY STICK
                        /mnt/stick copy
    Size:               16000000000
  org.freedesktop.UDisks2.Partition:
    Number:             1

/org/freedesktop/UDisks2/block_devices/sdc1:
  org.freedesktop.UDisks2.Block:
    CryptoBackingDevice:        '/'
    Device:                     /dev/sdc1
    Drive:                      '/org/freedesktop/UDisks2/drives/LiveUSB'
    HintIgnore:                 false
    HintSystem:                 false
    IdLabel:                    root
    IdType:                     ext4
    IdUsage:                    filesystem
    PreferredDevice:            /dev/sdc1
    Size:                       64000000000
  org.freedesktop.UDisks2.Filesystem:
    MountPoints:        /
    Size:               0

/org/freedesktop/UDisks2/block_devices/sdc2:
  org.freedesktop.UDisks2.Block:
    CryptoBackingDevice:        '/'
    Device:                     /dev/sdc2
    Drive:                      '/org/freedesktop/UDisks2/drives/LiveUSB'
    HintIgnore:                 false
    HintSystem:                 false
    IdLabel:                    data
    IdType:                     vfat
    IdUsage:                    filesystem
    PreferredDevice:            /dev/sdc2
    Size:                       1000000000
  org.freedesktop.UDisks2.Filesystem:
    MountPoints:
    Size:               0

/org/freedesktop/UDisks2/block_devices/sdd1:
  org.freedesktop.UDisks2.Block:
    CryptoBackingDevice:        '/'
    Device:                     /dev/sdd1
    Drive:                      '/org/freedesktop/UDisks2/drives/CardReader'
    HintIgnore:                 false
    HintSystem:                 false
    IdLabel:
    IdType:                     crypto_LUKS
    IdUsage:                    crypto
    PreferredDevice:            /dev/sdd1
    Size:                       8000000000
  org.freedesktop.UDisks2.Encrypted:
    CleartextDevice:            '/org/freedesktop/UDisks2/block_devices/dm_2d0'
  org.freedesktop.UDisks2.Partition:
    Number:             1

/org/freedesktop/UDisks2/block_devices/dm_2d0:
  org.freedesktop.UDisks2.Block:
    CryptoBackingDevice:        '/org/freedesktop/UDisks2/block_devices/sdd1'
    Device:                     /dev/dm-0
    Drive:                      '/'
    HintIgnore:                 false
    HintName:                   Photos
    HintSystem:                 false
    IdLabel:                    photos-fs
    IdType:                     exfat
    IdUsage:                    filesystem
    PreferredDevice:            /dev/mapper/luks-1234
    Size:                       7980000000
  org.freedesktop.UDisks2.Filesystem:
    MountPoints:        /run/media/me/Photos
    Size:               0

/org/freedesktop/UDisks2/block_devices/loop0:
  org.freedesktop.UDisks2.Block:
    CryptoBackingDevice:        '/'
    Device:                     /dev/loop0
    Drive:                      '/'
    HintIgnore:                 false
    HintSystem:                 false
    IdLabel:                    ISO
    IdType:                     iso9660
    IdUsage:                    filesystem
    PreferredDevice:            /dev/loop0
    Size:                       700000000
  org.freedesktop.UDisks2.Filesystem:
    MountPoints:
    Size:               0

/org/freedesktop/UDisks2/drives/Internal:
  org.freedesktop.UDisks2.Drive:
    CanPowerOff:                false
    Ejectable:                  false
    Model:                      SHPP41-2000GM
    Removable:                  false
    Vendor:

/org/freedesktop/UDisks2/drives/Kingston:
  org.freedesktop.UDisks2.Drive:
    CanPowerOff:                true
    ConnectionBus:              usb
    Ejectable:                  false
    Model:                      DataTraveler 3.0
    Removable:                  true
    Vendor:                     Kingston

/org/freedesktop/UDisks2/drives/LiveUSB:
  org.freedesktop.UDisks2.Drive:
    CanPowerOff:                true
    ConnectionBus:              usb
    Ejectable:                  false
    Model:                      Flash Disk
    Vendor:                     Generic

/org/freedesktop/UDisks2/drives/CardReader:
  org.freedesktop.UDisks2.Drive:
    CanPowerOff:                true
    ConnectionBus:              usb
    Ejectable:                  true
    MediaRemovable:             true
    Model:                      SD Card Reader
    Vendor:                     Generic
";

    const KINGSTON: &str = "/org/freedesktop/UDisks2/drives/Kingston";
    const READER: &str = "/org/freedesktop/UDisks2/drives/CardReader";

    #[test]
    fn parses_dump_properties_quoted_paths_and_multi_line_values() {
        let objects = parse_dump(DUMP);
        let sdb1 = "/org/freedesktop/UDisks2/block_devices/sdb1";
        assert_eq!(objects.property(sdb1, BLOCK, "Drive"), Some(KINGSTON));
        assert_eq!(objects.property(sdb1, BLOCK, "IdLabel"), Some("MY STICK"));
        assert_eq!(objects.property(sdb1, BLOCK, "HintName"), Some(""));
        assert_eq!(objects.link(sdb1, BLOCK, "CryptoBackingDevice"), None);
        assert_eq!(
            objects.mount_points(sdb1),
            [
                PathBuf::from("/run/media/me/MY STICK"),
                PathBuf::from("/mnt/stick copy")
            ]
        );
        assert!(
            objects
                .mount_points("/org/freedesktop/UDisks2/block_devices/sdb2")
                .is_empty()
        );
        // Continuation lines that contain colons stay part of the multi-line value.
        assert_eq!(
            objects.property(
                "/org/freedesktop/UDisks2/block_devices/nvme0n1p2",
                BLOCK,
                "Symlinks"
            ),
            Some("/dev/disk/by-id/nvme-x-part2\n/dev/disk/by-path/pci-0000:05:00.0-nvme-1-part2")
        );
        assert_eq!(
            objects.property(
                "/org/freedesktop/UDisks2/Manager",
                "org.freedesktop.UDisks2.Manager",
                "Version"
            ),
            Some("2.12.0")
        );
    }

    #[test]
    fn lists_only_file_systems_on_removable_drives() {
        let volumes = removable_volumes(&parse_dump(DUMP));
        let summary = volumes
            .iter()
            .map(|volume| {
                (
                    volume.device.as_str(),
                    volume.label.as_deref(),
                    volume.drive_name.as_str(),
                    volume.is_mounted(),
                )
            })
            .collect::<Vec<_>>();
        // Not listed: the NVMe system disk (HintSystem), the drive holding `/` (and its other
        // partition), the partition table itself, the locked-looking LUKS container, and the
        // drive-less loop device.
        assert_eq!(
            summary,
            [
                (
                    "/dev/mapper/luks-1234",
                    Some("Photos"),
                    "Generic SD Card Reader",
                    true
                ),
                (
                    "/dev/sdb1",
                    Some("MY STICK"),
                    "Kingston DataTraveler 3.0",
                    true
                ),
                ("/dev/sdb2", None, "Kingston DataTraveler 3.0", false),
            ]
        );
        let stick = &volumes[1];
        assert!(stick.read_only && stick.can_power_off && !stick.ejectable);
        assert_eq!(stick.fs_type, "vfat");
        assert_eq!(stick.size, 16_000_000_000);
        assert_eq!(stick.drive, KINGSTON);
        assert!(
            volumes[0].ejectable,
            "the cleartext volume inherits its reader's drive"
        );
    }

    #[test]
    fn does_not_offer_a_non_removable_internal_data_disk() {
        let dump = "\
/org/freedesktop/UDisks2/block_devices/sda1:
  org.freedesktop.UDisks2.Block:
    Device:                     /dev/sda1
    Drive:                      '/org/freedesktop/UDisks2/drives/InternalData'
    HintIgnore:                 false
    HintSystem:                 false
    IdType:                     ext4
    PreferredDevice:            /dev/sda1
  org.freedesktop.UDisks2.Filesystem:
    MountPoints:                /mnt/archive

/org/freedesktop/UDisks2/drives/InternalData:
  org.freedesktop.UDisks2.Drive:
    CanPowerOff:                false
    ConnectionBus:              sata
    Ejectable:                  false
    MediaRemovable:             false
    Model:                      Archive Disk
    Removable:                  false
";
        let objects = parse_dump(dump);
        assert!(removable_volumes(&objects).is_empty());
        assert_eq!(
            plan_removal(
                &objects,
                "/org/freedesktop/UDisks2/drives/InternalData",
                Removal::PowerOff
            ),
            None
        );
    }

    #[test]
    fn plans_safe_removal_unmounting_and_locking_first() {
        let objects = parse_dump(DUMP);
        assert_eq!(
            plan_removal(&objects, KINGSTON, Removal::PowerOff),
            Some(vec![
                Step::Unmount("/dev/sdb1".into()),
                Step::PowerOff(KINGSTON.into())
            ])
        );
        assert_eq!(plan_removal(&objects, KINGSTON, Removal::Eject), None);
        assert_eq!(
            plan_removal(&objects, READER, Removal::Eject),
            Some(vec![
                Step::Unmount("/dev/dm-0".into()),
                Step::Lock("/dev/sdd1".into()),
                Step::Eject(READER.into())
            ])
        );
        // Never for system disks, even through a crafted call.
        assert_eq!(
            plan_removal(
                &objects,
                "/org/freedesktop/UDisks2/drives/LiveUSB",
                Removal::PowerOff
            ),
            None
        );
        assert_eq!(
            plan_removal(
                &objects,
                "/org/freedesktop/UDisks2/drives/Gone",
                Removal::PowerOff
            ),
            None
        );
    }

    #[test]
    fn reads_the_mount_point_from_udisksctl_messages() {
        let dir = std::env::temp_dir();
        assert_eq!(
            parse_mounted_path(&format!("Mounted /dev/sdb1 at {}\n", dir.display())),
            Some(dir.clone())
        );
        assert_eq!(
            parse_mounted_path("Mounted /dev/sdb1 at /run/media/me/MY STICK.\n"),
            Some(PathBuf::from("/run/media/me/MY STICK"))
        );
        assert_eq!(parse_mounted_path("something else"), None);
    }

    #[test]
    fn cleans_udisksctl_errors() {
        assert_eq!(
            clean_error(
                "Error creating textual authentication agent: Error opening current controlling terminal for the process (`/dev/tty'): No such device or address (polkit-error-quark, 0)\n\
Error unmounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.DeviceBusy: Error unmounting /dev/sdb1: target is busy\n"
            ),
            "Error unmounting /dev/sdb1: target is busy"
        );
        assert_eq!(
            clean_error(
                "Error mounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.AlreadyMounted: Device /dev/sdb1 is already mounted at `/run/media/me/X'.\n"
            ),
            "Error mounting /dev/sdb1: Device /dev/sdb1 is already mounted at '/run/media/me/X'"
        );
        assert!(
            clean_error("Error setting up loop device for /x: GDBus.Error:org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain: Not authorized to perform operation")
                .ends_with("(no polkit authentication agent answered, or access was denied)")
        );
        assert_eq!(
            clean_error(
                "Error: GDBus.Error:org.freedesktop.DBus.Error.ServiceUnknown: The name is not activatable"
            ),
            "Error: The name is not activatable"
        );
        assert_eq!(
            clean_error(""),
            "The command failed without an error message"
        );
    }

    /// Against the real daemon when it's available: whatever this machine has, internal
    /// system disks must never be offered as removable.
    #[test]
    fn real_dump_never_lists_system_disks() {
        let Ok(objects) = dump() else {
            eprintln!("skipping: udisksctl or the UDisks daemon isn't available");
            return;
        };
        for volume in removable_volumes(&objects) {
            assert!(
                volume.mount_points.iter().all(|point| !SYSTEM_MOUNTS
                    .iter()
                    .any(|system| point == &PathBuf::from(system))),
                "{volume:?}"
            );
        }
    }
}
