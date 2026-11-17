# drive-index

A terminal app that catalogs your external drives. Plug a drive in: drive-index
recognises it, mounts it read-only, indexes every file into a local SQLite catalog,
and ejects it again. Later you can search all your drives at once, offline: by name,
date, size, camera, codec and more. It also finds duplicates, and shows which drives
hold how much of a collection ("where are my RAW photos, and which ones exist on only
one drive?").

Runs on macOS and Linux. Written in Rust.

```
 drive-index  1 Drives  2 Search  3 Browse  4 Dupes  5 Collections  6 Log  7 Setup     ● 1 online
╭ Search  sort: path ↑ ────────────────────────────────────────────────────────────────────────────╮
│> ext:cr2,crw taken:2014 camera:canon                                                             │
│  Σ 1,912 files · 41.3 GiB · D1 1,204 · D3 708                                                    │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
```

## Features

- **Hands-off scanning.** In the TUI, connecting a drive is enough. Known drives are
  recognised by their serials, WWN and partition and filesystem UUIDs, even in a different
  enclosure, and filesystems are rescanned when their last scan is older than 30 days. New
  drives get a label prompt. `drive-index watch` does the same without a UI.
- **Read-only, always.** Filesystems are mounted read-only (on Linux, also without journal
  replay), and nothing is ever written to a scanned drive. Drives already mounted read-write
  by the system are scanned as they are.
- **Fast.** A parallel directory walker uses `getattrlistbulk` on macOS and
  `getdents64` + `statx` in inode order on Linux. Results go into a per-scan staging database
  first, so an interrupted scan (unplugged, stopped, crashed) resumes where it stopped.
- **Per file:** path, size, allocated size, modified/created/changed times, mode, owner,
  inode and link count, flags (hidden, compressed, sparse, dataless, …) and symlink targets.
  **Per filesystem:** type, label, UUID, size and usage.
  **Per drive:** model, serial, WWN, bus, USB ids, capacity, block sizes, partition table,
  partitions and SMART health.
- **Media metadata:** EXIF capture time, camera, lens, ISO, aperture, focal length and GPS for
  photos (including RAW). Duration, resolution and codecs per stream for video (with
  `ffprobe`). Tags for audio. Read in a second, gentler content phase that touches only the
  files it needs.
- **Duplicates** at three confidence levels: *exact* (full BLAKE3 hash), *likely* (size plus a
  hash of the first and last 64 KiB) and *probable* (same name and size).
- **Collections and coverage:** saved queries such as `in:photos-raw`, with how much of each
  is on every drive, how much has only one copy, and items per year.
- **One catalog, two hosts.** The catalog is a single file. Move it between a Mac and a Linux
  machine by copying it; drive-index notices stale copies before they overwrite newer work.

## Install

You need Rust 1.88 or newer ([rustup](https://rustup.rs)).

```sh
git clone https://github.com/luite/drive-index.git && cd drive-index
cargo install --locked --path crates/drive-index
drive-index doctor
```

Or without a checkout: `cargo install --locked --git https://github.com/luite/drive-index.git drive-index`.

`doctor` checks the optional tools and permissions, and prints the command that installs
what is missing:

| Tool | Package | Used for |
|------|---------|----------|
| `smartctl` 7.0+ | smartmontools | drive health, and the real disk serial behind USB bridges |
| `ffprobe` | ffmpeg | video stream details (codecs, frame rates, audio tracks) |
| `exiftool` | exiftool / libimage-exiftool-perl | metadata of files the built-in parsers can't read |
| `udisks2` (Linux) | udisks2 | detecting, mounting and powering off drives without root |

Everything works without them; each missing tool only disables its own feature.

### macOS

macOS asks before a terminal app may read external volumes. If scans fail with "Operation
not permitted", allow your terminal under **System Settings › Privacy & Security › Files &
Folders › Removable Volumes**, or grant it Full Disk Access. SMART works for internal and
Thunderbolt drives but not over USB, because macOS doesn't pass SMART commands through USB
bridges.

### Linux

Mounting goes through udisks2, so a desktop session can mount without root. Over SSH,
polkit usually refuses. To allow members of the `plugdev` group to mount, unmount and
power off drives (but not format them), install the included rule:

```sh
sudo cp contrib/polkit/50-drive-index.rules /etc/polkit-1/rules.d/
sudo usermod -aG plugdev "$USER"    # then log in again
```

Reading SMART needs root, unless you install the helper described in
[SMART on Linux without root](#smart-on-linux-without-root).

## Usage

Run `drive-index` in a terminal to start the TUI. It watches for drives while it runs.

```sh
drive-index                          # TUI
drive-index scan /dev/disk4          # scan a drive (macOS: disk4; Linux: /dev/sdb)
drive-index scan ~/Pictures          # or any directory
drive-index watch --scan-new         # headless: scan whatever gets plugged in
drive-index search ext:cr2,crw taken:2014
drive-index dupes --confidence likely --min-size 100M --in in:video
drive-index coverage photos-raw
drive-index smart disk4              # live SMART reading (D3: stored readings)
drive-index info                     # drives and partitions as the OS sees them
```

Useful `scan` options: `--label NAME`, `--after eject|unmount|keep`, `--verify` (full-hash
likely duplicates), `--no-content` (skip the metadata/hash phase), `--no-smart`, and
`--restart` (discard an interrupted scan). `scan` walks every filesystem of a drive; with
`--rescan-after 1d` it skips filesystems scanned within the last day.

In the TUI and `watch`, a connected known drive is scanned automatically when one of its
filesystems was last scanned more than `--rescan-after` ago (default 30 days). Only those
filesystems are walked again.

### The TUI

| Screen | Keys |
|--------|------|
| 1 Drives | `s` scan · `v` verify duplicates · `p` pause/resume · `x` stop · `u` unmount · `e` eject · `l` label · `n` notes · `b` browse |
| 2 Search | type a query · `Tab` completes · `F1` all filters · `⏎` details · `s` sort · `d` reverse · `c` copy path · `b` browse to the file |
| 3 Browse | `⏎`/`→` open · `←`/`⌫` up · `s` sort by size, count, name or date |
| 4 Dupes | `c` confidence · `m` minimum size · `x` same drive only · `f` scope query · `→` copies |
| 5 Collections | `←`/`→` collection · `a` files with a single copy · `⏎` files on the selected drive · `o` files only on that drive |
| 6 Log | everything that happened, newest first |
| 7 Setup | the `doctor` report · `c` copy the install command · `H` write a copy of the catalog for another host |

`1`–`7` or `Tab` switch screens, `/` jumps to search, `?` shows help and `q` quits. The mouse
works for tabs, rows and scrolling; `drive-index tui --no-mouse` keeps the terminal's own text
selection.

### Search

A query is a list of terms, all of which must match. A bare word matches file names. `key:value`
terms filter, commas mean "or", and a leading `-` excludes.

```
ext:cr2,crw date:2014               Canon RAW files modified in 2014
in:photos-raw copies:1              RAW photos that exist on only one drive
type:video size:>1G -trash:yes      big videos, not in a trash folder
camera:"EOS 5D*" iso:>=3200         high-ISO shots from one camera
res:4k codec:hevc duration:>10m     long 4K HEVC videos
dup:likely size:>100M drive:D3      large files on D3 that have a copy somewhere
```

There are filters for names and paths, type and extension, size, dates (modified, created,
capture date), drive and filesystem, photo and video metadata, music tags, duplicates and
file flags. `drive-index search --filters` (or `F1` in the TUI) lists them all with
examples. In the TUI the hint line explains the term under the cursor and `Tab` completes
filter names and values from your catalog.

### Moving the catalog between hosts

The catalog lives in `~/Library/Application Support/drive-index/index.db` (macOS) or
`~/.local/share/drive-index/index.db` (Linux); `--db` or `DRIVE_INDEX_DB` overrides it.

```sh
drive-index db handoff               # writes a consistent copy to the current directory
# copy it over, then on the other host:
drive-index db import drive-index-mac-….db
```

Each host remembers which catalog version it last wrote. Importing or writing to an older
copy is refused unless you pass `--force` / `--accept-stale`.

## SMART on Linux without root

smartctl needs root on Linux. Adding yourself to the `disk` group is not enough: that lets
smartctl open the device, but the kernel only accepts the SMART commands themselves (ATA
pass-through, which USB bridges use, and NVMe log pages) from processes with
`CAP_SYS_RAWIO` / `CAP_SYS_ADMIN`. The `disk` group would also give you raw write access to
every disk.

Instead, drive-index ships a tiny helper, `drive-index-smart`, meant to be installed setuid
root and executable by one group. It runs exactly one command,
`smartctl -j -i -H -A [-d TYPE] /dev/DISK`, which reads identity, health and attributes and
changes nothing. It accepts nothing else:

- no other smartctl options (no self-tests, no setting changes);
- device types only from a fixed list;
- devices only whole disks (`sdX`, `nvmeNnM`, `vdX`, `hdX`) that are root-owned block devices,
  not symlinks;
- smartctl only from `/usr/sbin`, `/usr/bin`, `/sbin` or `/usr/local/sbin`, owned by root and
  not writable by others, run with an empty environment.

Install it:

```sh
cargo build --release --locked -p drive-index-smart
sudo groupadd --system drive-index
sudo usermod -aG drive-index "$USER"          # log in again afterwards
sudo install -o root -g drive-index -m 4750 target/release/drive-index-smart /usr/local/bin/
drive-index doctor                            # smartctl: ok, through the setuid helper
```

drive-index uses the helper automatically when it isn't running as root. It looks next to
its own executable, then on `PATH`, then in `/usr/local/bin`, `/usr/local/libexec`,
`/usr/libexec` and `/usr/bin`. It uses the helper only if the file is owned by root, has the
setuid bit, and is executable by you. Mode `4750` limits it to the `drive-index` group.

Alternatives:

- Run drive-index with `sudo`.
- Set `DRIVE_INDEX_SMARTCTL="sudo -n smartctl"` together with a sudoers rule for smartctl.
  This is broader: sudoers argument matching can't restrict smartctl to read-only queries.

## Building and testing

```sh
cargo test                      # unit, end-to-end and TUI render tests
cargo clippy --all-targets
```

The device tests attach and eject a real disk image, so they are opt-in (macOS):

```sh
hdiutil create -size 64m -fs ExFAT -volname DI_TEST /tmp/di-test.dmg
DI_TEST_IMAGE=/tmp/di-test.dmg cargo test -p di-tui --test controller_flow -- --ignored --nocapture
```

The workspace has these crates:

- `di-model`: shared types and units.
- `di-walk`: the directory walker.
- `di-platform`: drive detection, mounting and SMART, using DiskArbitration/diskutil on macOS
  and udisks2/sysfs on Linux.
- `di-core`: the catalog, search language, duplicates and coverage.
- `di-scan`: scans, merges, the content phase and the drive controller.
- `di-media`: metadata extraction.
- `di-doctor`: environment checks.
- `di-tui`: the terminal UI.
- `drive-index`: the command-line interface.
- `drive-index-smart`: the SMART helper.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
