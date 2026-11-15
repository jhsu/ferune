# Ferune

A native Linux file manager built with [Rune](vendor/rune). It uses Rune's
GPU-rendered window and supports Wayland through winit.

## Run

```sh
cargo run --release
```

## Install and set as the default file manager

Build and install the executable under `~/.local/bin`:

```sh
cargo install --path . --root "$HOME/.local"
```

Install the desktop entry and register Ferune to handle directories:

```sh
install -Dm644 data/ferune.desktop \
  "$HOME/.local/share/applications/ferune.desktop"
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
xdg-mime default ferune.desktop inode/directory
```

Make sure `~/.local/bin` is on your `PATH`, then verify the association:

```sh
xdg-mime query default inode/directory
```

It should print `ferune.desktop`. Desktop environments may also expose a
“Default Applications” setting that can select Ferune as the file manager.
To restore the previous handler, choose it in that setting or run
`xdg-mime default <previous-handler.desktop> inode/directory`.

### Niri window matching

Ferune sets its Wayland application ID to `ferune` and its window title to
`Ferune File Manager`. Niri can match rules against either value; inspect the
live values with `niri msg windows`, then use `app-id="ferune"` (or the title)
in a `window-rule`.

Open files with the desktop's default application through `xdg-open`. Ferune
supports tabs, editable paths, list/grid views, sorting, multi-selection,
hidden files, and resizable previews. Click a folder's star in the preview to
favorite it, click a sidebar star to remove it, or drag folders from another
application into the sidebar. Favorites and display preferences (zoom, preview
width, sort, list/grid, hidden files) are saved under `~/.config/ferune/`.

File drops rely on winit, which does not report file drags on native Wayland
yet; drops into the sidebar work on X11 or when Ferune runs under XWayland
(for example `env -u WAYLAND_DISPLAY ferune`; on Niri this needs
xwayland-satellite).

Single-click selects; double-click or Enter opens. Rename inline from the
right-click menu. Use Ctrl-click or
Shift-click for multi-selection; Ctrl+C/X/V copy, cut, and paste files; Ctrl+A
selects visible items. Ctrl+T opens a tab, Ctrl+W closes it, Ctrl+N opens the
current folder in a new tab, and Ctrl+Shift+N creates a folder. Ctrl++/Ctrl+-
change reading scale; Ctrl+0 resets it. Trash supports restore, empty (with
confirmation), and Ctrl+Z undo, which restores the whole last “Move to Trash”
batch. Restore never overwrites an item that already exists at the original
location.

Click an image preview to expand it over a dimmed window; click again or press
Escape to return. Image previews show camera, lens, capture time and exposure
details when ExifTool is installed; GPS/location tags are not read. Previews
cover text, Markdown, images, and PDFs. PDF text extraction uses `pdftotext`
(Poppler) when installed. File icons use the configured
Freedesktop icon theme when available, with built-in outline SVGs as fallback.

File types come from the desktop's shared-mime-info database, through GIO
(`standard::content-type`, the same type GNOME Files shows), rather than from
the file name alone: an extensionless PNG gets an image icon, preview and
thumbnail, a `README` previews as text, and an archive or executable isn't read
as text whatever it's called. Local folders are typed on a background thread,
with one `gio info` run per 128 files (up to 20,000 per folder); until it
answers, and whenever `gio` or the database is missing, the extension decides
as before. Remote GVfs entries use the type their listing already carried.
GLib trusts an unambiguous extension over the contents (a PDF named `scan.png`
stays `image/png`), so contents decide only for names with no or an unknown
extension (`photo.backup`, `picture`) and for ambiguous ones like `.ts`.
Runtime integrations include `xdg-open`, `gio` (trash, and the network locations
below), `udisksctl` (removable drives, below), `zenity`, and Wayland clipboard
tools `wl-copy`/`wl-paste`. EXIF display optionally uses `exiftool` (package
`perl-image-exiftool` / `libimage-exiftool-perl`).

## Desktop notifications

Copy, move (cut and paste) and duplicate run in the background with progress in
the status bar and a Cancel button (or Escape). When one takes at least 2
seconds and ends while Ferune's window isn't focused, Ferune sends a desktop
notification: “Copy finished”, “Move cancelled” (with how many items were done)
or “Copy failed” (with the error, at critical urgency). Quick operations, and
operations that end while you're looking at the window, only update the status
bar. Time spent answering a “Replace file?” prompt doesn't count towards the 2
seconds.

Notifications go straight to the `org.freedesktop.Notifications` D-Bus service
(no `notify-send` or shell involved), so they need a notification daemon such
as mako, dunst, swaync, fnott, or the one built into GNOME or KDE Plasma. This
is optional: without a daemon or a session bus, Ferune notes it on stderr and
otherwise works the same. Ferune shows at most one notification at a time: a new one
replaces the last, and focusing the window again closes it. File names are
escaped for daemons that render body markup. Notifications name the
`ferune.desktop` entry, so daemons that use it show Ferune's name.

Trash, restore, undo and permanent delete don't notify: they're usually instant
renames and still run on the UI thread.

Run the tests with `cargo test` (the thumbnail tests use a temporary cache
and `cp`/`false` as stand-in thumbnailers; the MIME tests type extensionless
and misnamed files in a temporary folder through the real `gio`, skipping
themselves without it; the UDisks parser tests use recorded
`udisksctl dump` output; one test also checks this machine's real drives and
skips itself when UDisks isn't running; the FileManager1 and notification tests start a private
`dbus-daemon` (the latter with a stand-in notification daemon) and skip themselves without one,
never touching your session bus or showing real notifications). `cargo test -- --ignored` also mounts GVfs's
`localtest` backend to exercise the mount-on-first-visit path, then unmounts it,
checks that thumbnails other programs stored in your cache sit under the names
Ferune computes (read-only), and runs an installed PNG thumbnailer against a
temporary cache.

## Network shares and phones (GIO/GVfs)

Ferune browses GVfs locations next to local folders: type a URI such as
`sftp://me@host/srv`, `smb://nas/share`, `ftp://host/` or `mtp://…` into the
location bar (click the path to edit it), or start Ferune with one
(`ferune sftp://host/`). `file://` URIs open as ordinary local folders. Mounted
remote locations (shares mounted elsewhere, phones in file-transfer mode)
appear in the sidebar above **Network** (`network:///`), and a remote folder
can be starred like any other favorite.

Remote locations are reached only through the `gio` command-line tool, never
through direct file system calls. Listing happens in the background, and a
location that isn't mounted yet is mounted on first visit. Errors (unknown
host, permission denied, missing backend) show in the status bar and the file
list. Remote folders are **browse-only** for now. You can open, open in a new
tab, favorite, copy the URI (Copy Path), and view properties (type, size,
modified). Copy, cut, paste, rename, trash, delete and new folder are
disabled there. Selecting a remote file shows its metadata rather than a
content preview. Open hands the URI to `gio open`, which gives GIO-aware
applications the URI and others a local path when GVfs's FUSE bridge
(`gvfsd-fuse`) is running.

Mounting is non-interactive. Locations that need a password or an SSH host-key
confirmation fail with a hint: mount them once with `gio mount <uri>` in a
terminal (or with ssh keys/a keyring that GVfs can use), then browse them in
Ferune.

## Thumbnails

Images, videos, PDFs and other documents in local folders show thumbnails in
the list, the grid and the preview pane, using the Freedesktop thumbnail cache
that GNOME Files, Nautilus, Thunar, Dolphin and others share
(`$XDG_CACHE_HOME/thumbnails`, normally `~/.cache/thumbnails`). A thumbnail is
reused only while its `Thumb::URI` and `Thumb::MTime` (and `Thumb::Size`, when
recorded) still match the file, so an edited file gets a new one. Missing or
stale thumbnails are made by the system's thumbnailers, the
`*.thumbnailer` entries under `~/.local/share/thumbnailers` and
`/usr/share/thumbnailers`, which Ferune runs directly with their argument list
(never through a shell), at most two at a time, each with a 30-second limit.
New thumbnails go into `normal/` (128 px) or, at higher reading scales,
`large/` (256 px); a larger cached size is used when the matching one is
missing. Files that a thumbnailer fails on are recorded under
`fail/ferune-<version>/` and not retried until they change.

Looking up, generating and reading thumbnails all happen on background
threads. Anything without a thumbnail (an unsupported type, no thumbnailer
installed, a failure) keeps its usual icon. Remote GVfs locations always use
icons: their files have no local `file://` URI to cache under, and making one
would download the whole file. Paths under GVfs's FUSE bridge
(`/run/user/<uid>/gvfs`) are skipped for the same reason.

Thumbnailers are optional runtime packages; install the ones you want:

| Files | Package (Arch / Debian & Ubuntu / Fedora) |
| --- | --- |
| Images (PNG, JPEG, WebP, GIF, TIFF, SVG, AVIF, HEIF, JPEG XL) | `glycin` (newer GNOME) or `gdk-pixbuf2` / `libgdk-pixbuf2.0-bin` / `gdk-pixbuf2` |
| Videos | `ffmpegthumbnailer`, or `totem` (GStreamer-based) |
| PDF, PostScript, DjVu, comics | `papers` or `evince` (each ships a thumbnailer) |
| Office documents (ODF, OOXML) | `libgsf` / `libgsf-bin` / `libgsf` |

Thumbnailers are chosen by the file's detected MIME type (see above), falling
back to its extension, so an extensionless image or video still gets a
thumbnail. D-Bus thumbnail services (Tumbler's `org.freedesktop.thumbnails.Thumbnailer1`) aren't used;
only `.thumbnailer` entries are.

## Removable drives (UDisks2)

USB sticks, USB disks, SD cards and other removable media appear under
**Devices** in the sidebar, whether or not they are mounted. Each file system on
a drive is its own row (a stick with two partitions shows two), labeled by its
file system label or its size (“16.0 GB Volume”), with the drive name and
whether it's mounted underneath. Unlocked encrypted partitions show their inner
file system.

- Click a device to open it. An unmounted one is mounted first through
  `udisksctl mount`; UDisks picks the mount point (normally
  `/run/media/$USER/<label>`). Volumes already mounted by something else
  (another file manager, an automounter, fstab) open where they are.
- Click ⏏ to unmount a mounted volume.
- Right-click for **Open**, **Mount**, **Unmount**, **Eject** (drives with
  ejectable media) and **Safely Remove (Power Off)**. Eject and Power Off first
  unmount every file system on the drive and lock its unlocked encrypted
  partitions, and stop at the first failure, so a drive is never powered off
  with a file system still mounted. “Target is busy” means a program still has
  a file open there.

Ferune never mounts anything on its own: plugging in a drive only lists it.
Listing, mounting, unmounting and removal run in the background and report
progress and errors in the status bar; a tab showing a volume that Ferune
unmounts goes back to Home. The list updates when drives come and go by
following `udisksctl monitor`.

Internal disks are not listed: like GNOME's volume monitor, Ferune trusts
UDisks' `HintSystem`/`HintIgnore` hints, and it also skips any drive holding
`/`, `/boot`, `/boot/efi`, `/efi`, `/usr`, `/var`, `/home` or `/opt`. Loop
devices, swap, and locked encrypted partitions are not listed either (unlock
one with `udisksctl unlock -b /dev/sdX1` and it appears).

## “Show in folder” (org.freedesktop.FileManager1)

While Ferune runs, it serves the Freedesktop file manager interface on the user's
session bus, which browsers (“Show in Folder” for a download), editors, terminals,
screenshot tools and `gio`/GTK/Qt apps use to ask the file manager to point at a file:

| | |
| --- | --- |
| Bus name | `org.freedesktop.FileManager1` (session bus) |
| Object | `/org/freedesktop/FileManager1` |
| Interface | `org.freedesktop.FileManager1` |
| `ShowFolders(as uris, s startup_id)` | Opens each folder. |
| `ShowItems(as uris, s startup_id)` | Opens each item's folder and selects the items. |
| `ShowItemProperties(as uris, s startup_id)` | Reveals the item and opens its Properties. |

`uris` are normally `file://` URIs (percent-encoded); plain absolute paths,
`file:/path` and `file://localhost/path` are accepted too, and GVfs URIs
(`sftp://`, `smb://`, …) are shown like ones typed into the location bar. Each
folder goes into a tab: a tab already showing it is brought to the front (and
re-read, so a just-finished download is there); otherwise a new tab opens, except
that a window nobody has used yet shows the first folder in its own tab. When
several folders are asked for, the last one ends up in front, and the selection is
made there. Items in the same folder share one tab and one selection. A hidden item
turns hidden files on for the session (the saved preference doesn't change). An
item that doesn't exist opens its folder with nothing selected and a note in the
status bar; if nothing in a call can be shown, the call fails with
`org.freedesktop.DBus.Error.InvalidArgs` so the caller can fall back. Properties
show for one item (the first) at a time, in the same dialog as the context menu's
**Properties**. `startup_id` is the caller's activation token
(`XDG_ACTIVATION_TOKEN` / `DESKTOP_STARTUP_ID`); Ferune passes it on when raising
its window, so compositors that honor tokens bring it to the front.

Try it from a terminal:

```sh
gdbus call --session --dest org.freedesktop.FileManager1 \
  --object-path /org/freedesktop/FileManager1 \
  --method org.freedesktop.FileManager1.ShowItems \
  "['file://$HOME/.bashrc']" ""
# or: busctl --user call org.freedesktop.FileManager1 /org/freedesktop/FileManager1 \
#   org.freedesktop.FileManager1 ShowFolders ass 1 "file://$HOME/Downloads" ""
```

The first file manager to claim the name keeps it: if Nautilus, Dolphin, Thunar or
another Ferune window already owns it, Ferune doesn't take it over and simply runs
without the service (it prints a note on stderr). Without a session bus at all,
Ferune starts as usual. To have desktop apps start Ferune when it isn't running,
install the D-Bus activation file with your executable's path (a service file in
`~/.local/share/dbus-1/services` takes precedence over the system's):

```sh
sed "s|^Exec=.*|Exec=$HOME/.local/bin/ferune|" data/org.freedesktop.FileManager1.service |
  install -Dm644 /dev/stdin \
  "$HOME/.local/share/dbus-1/services/org.freedesktop.FileManager1.service"
```

Remove that file to give the name back to the previous file manager's activation.

## Prerequisites

Build prerequisites are Rust (use `rust-toolchain.toml` in `vendor/rune`) and
the native graphics/windowing dependencies required by Rune and wgpu.

File-type detection uses GLib's `gio` tool (package `glib2`/`libglib2.0-bin`)
and the shared-mime-info database (package `shared-mime-info`, installed on
practically every desktop; it provides `/usr/share/mime`). Without either,
types, icons, previews and thumbnailers follow file extensions only.

Remote locations need the same `gio` tool and
GVfs with the backends you use (`gvfs`, plus e.g. `gvfs-smb`, `gvfs-mtp`,
`gvfs-gphoto2`; sftp and ftp ship with `gvfs` on most distributions) running in
the user's D-Bus session. Without them, local browsing works unchanged and
remote URIs report the missing tool or backend in the status bar.

Removable drives need UDisks2 (package `udisks2`, which provides `udisksctl`
and the `udisksd` system service) and, for Eject, GLib's `gdbus` (same package
as `gio`). Mounting removable media as a desktop user is normally allowed
without a password; when polkit asks for one, the desktop's polkit
authentication agent shows the prompt, and without an agent the action fails
with “Not authorized”. Without `udisksctl` the Devices section is simply
hidden.
