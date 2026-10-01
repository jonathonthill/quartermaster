# Quartermaster

A desktop app for moving research data between your computer, analysis
servers, and a department archive server, safely. Every copy is checked end to
end, interrupted transfers continue where they stopped, and nothing is deleted
until everything has arrived. It has two modes:

- **Transfer** copies or moves ordinary files between this computer and
  servers, like an SFTP client, but checksummed.
- **Stow** puts folders into an archive (a **datahold** in the app), where data
  is checksummed, compressed, deduplicated, bundled into large packs, and
  protected with PAR2 recovery data. You still see an ordinary folder tree and
  can retrieve any single file or folder without unpacking a bundle.

The app signs in with your computer's own `ssh`, so your `~/.ssh/config`, keys,
and known hosts all apply, and password and Duo prompts appear in the app.

Each folder sent to the archive becomes a **project** (a sealed **barrel** in
the app), and archived projects are frozen: you can add files to one, and
rename, move, or trash it as a whole (throw it **Overboard**, in the app), but
nothing inside it can be renamed, replaced, or deleted on its own. Projects
are self-contained (duplicates are stored once within a project, and each pack
holds one project), so one can be recovered or deleted by itself. Loose files
go into an existing project or a new one.

## Download

Get the app for macOS, Windows, or Linux from the
[Releases](../../releases) page. The builds aren't signed yet, so the first
launch needs one extra step:

- **macOS**: open the `.dmg` and drag Quartermaster to Applications. The first
  time, right-click (or Control-click) the app and choose **Open**, then
  **Open** again. If macOS says the app "is damaged", run
  `xattr -dr com.apple.quarantine /Applications/Quartermaster.app` in Terminal.
- **Windows**: run the installer. If SmartScreen appears, choose **More info**,
  then **Run anyway**. Windows builds are new and less tested than the Mac.
- **Linux**: use the `.AppImage` (make it executable) or the `.deb`.

Servers need nothing installed in advance. When you add a server, the app
offers to install its small helper program (`archive-helper`, for Linux and
FreeBSD on x86_64) into your home folder there. A server that can't run the
helper can still be added as an **SFTP server**, for Transfer mode.

Status: the storage engine, the `archive` command-line tool, the server
helper, and the desktop app work end to end over SSH, including against a
TrueNAS (FreeBSD) archive server. Direct server-to-server copies in Transfer
mode are planned. The on-disk format is in [docs/FORMAT.md](docs/FORMAT.md);
the wire protocol is in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Layout

```
crates/core     storage format, catalog, transfer engine, protocol, PAR2 maintenance
crates/cli      the `archive` command-line tool (local or ssh:// archives)
crates/helper   archive-helper: runs on servers (serve, worker, rebuild, cron)
app/            the desktop app (Tauri): app/src-tauri is the Rust backend, app/ui the interface
scripts/        cross-compiling the helper (FreeBSD 13.1 sysroot, Rust's lld)
docs/           format and protocol specifications
```

## Build and test

```
cargo build --release          # target/release/archive
cargo test                     # PAR2 tests need `par2` on PATH (brew install par2)
scripts/build-helpers.sh       # dist/helpers/archive-helper-freebsd-x86_64
```

## Desktop app

```
scripts/build-helpers.sh          # the server helpers the app installs (needs clang; macOS or Linux)
cd app && npm install
npx tauri build --bundles app     # target/release/bundle/macos/Quartermaster.app
```

Pushing a tag such as `v0.1.0` builds the app for macOS, Windows, and Linux
with GitHub Actions and attaches them to a draft release
([.github/workflows/release.yml](.github/workflows/release.yml)).

The desktop app is called Quartermaster, and it has a few names of its own: an
archive is a datahold, a project a barrel, the trash Overboard, and the list
of transfers the Dock. It has two modes, chosen by the switch in the top bar:
**Stow** puts folders into a datahold (described below), and **Transfer**
copies or moves ordinary files exactly as they are between this computer and
file servers, like an SFTP client, but checksummed. In Transfer mode both
panes show this computer or a file server (at least one must be a file
server), and → and ← copy in either direction. Each file is verified against a
SHA-256 from the source before it takes its final name, and a copy that's
interrupted continues from where it stopped. Between two servers the data
passes through this computer. Move deletes the originals only once every file
has arrived and been verified, so stopping or abandoning it (the skull) before
then undoes it completely, as in Stow. Stow and Transfer keep their own panes
and share the Dock. The app uses the system `ssh`, so hosts and aliases in
`~/.ssh/config`, keys, and known hosts all apply. Password, Duo, and new-host
prompts appear in the app. The left pane shows this computer or a file server,
and the right pane an archive: → sends to the archive (you choose Copy or Move
in the confirmation), ← retrieves from it (always a copy). To type a folder
path, click the empty part of the path bar, press ⌘L, or type `/` or `~` in a
file list; Tab completes folder names. The bar along the bottom, the Dock,
sums up transfers; click it to see each one. Every connection the app makes to
a server shares one sign-in.
If your ssh config already shares connections (ControlMaster), the app uses
those; otherwise it keeps its own for two hours. To work on the interface
without the backend, serve `app/ui` over HTTP; it runs with sample data.

On a Mac the app keeps running after its last window closes (its Dock icon
opens a window again); ⌘N opens another window. Each window has its own
connections, transfers, and Dock, and asks for passwords in the window that
needs them. Closing a window, or quitting, while transfers from this computer
are moving asks first: **Pause and close** (the default) keeps them, to be
played again in another window or the next time you open one; **Abandon ship
and close** undoes them. There is no plain stop that leaves a half-finished
transfer: a transfer is paused, to be finished later, or abandoned. Transfers running on a file
server belong to the server and carry on. A window is where this computer's work
lives, and a server is where its own jobs live.

**SFTP servers.** A server where the helper can't be installed can be added in
Settings as an **SFTP server**. The app speaks SFTP over the system `ssh`, so
sign-in works as for other servers. An SFTP server takes part in Transfer mode
(browsing, Places, search, copying and moving either way, new folders, and the
Trash), but can't be a source for stowing. It can't compute checksums, so copies
to and from it are checked by size and date, and the Dock says "Size matches"
instead of "Checksums match". Turn on **Check uploads by reading them back** in
its settings to have each upload read back and its checksum compared, which takes
about twice as long.

Files can be removed from the panes: right-click ▸ **Throw overboard**, or press Delete
(or ⌘⌫), and the selection goes straight to the Trash, with no popup, since it can be
undone. On this
computer that is the Mac's Trash. A file server has no Trash, so the helper keeps one:
the item is moved (instantly, however big) into a hidden folder, and the **Trash**
button in a server pane lists what's there. Select items (Select all, then untick any to
keep) to restore them or delete them for good; deleting for good asks first and says what
will go.
Nothing leaves the Trash by itself, and items in it still use space on the server. Only
if a server can't make a Trash for something (a folder you can't write to) does a popup
appear, offering to delete it permanently instead. It never trashes or deletes the top of
a disk, your home folder, or a mounted disk. In a datahold, Throw overboard works as before
(restorable for 30 days).

Any transfer can be paused and played again; it continues where it stopped,
and the Dock's header pauses or plays them all. Unfinished transfers from this
computer are remembered when the app quits and come back paused. Abandon ship
(the skull) stops a transfer and undoes it: a send's new barrel (project)
goes Overboard (the datahold's trash, restorable for 30 days), and a retrieve's new
folders are deleted. Only what the transfer itself created is removed. In Move
mode the originals are deleted only once the whole transfer is archived and
verified, so stopping or abandoning a transfer never loses them. When a send
continues, files not yet archived go first while the rest are checksummed in
the background.

Transfers between a file server and an archive run on the file server, so
the data goes directly between the two servers and you can close the app.
When you start one, the file server signs in to the archive server itself,
and its password or Duo prompt appears in the app. The file server keeps that
connection open while its transfers run, and for two hours afterward. If the
connection closes, for example because the server restarted, the transfer
pauses. Its row in the Dock then offers **Sign in**, and the transfer
continues where it stopped. For archive servers that accept SSH keys, Settings
can instead give each file server a limited key, which can only add and read
data, so transfers never need a sign-in.

If a file server can't reach the archive server, the app offers to send
through this computer instead. You can also set this per file server in
Settings (**Send through this computer**), for servers that don't let programs
keep running after you log out. The file server still reads, checksums, and
compresses the files, and in Move mode deletes the originals only once the
whole transfer is archived and verified. The data passes through this computer, so keep it awake with
the app open until the transfer finishes. A transfer that loses its connection
pauses, with a note saying so; press play to reconnect and continue where it
stopped. Nothing retries on its own.

## Setting up a server

```
archive helper install --host storage.example.edu     # uploads and checksums ~/.local/bin/archive-helper
export ARCHIVE_ROOT=ssh://storage.example.edu/mnt/pool/lab/archive
archive init
```

Maintenance runs on the server by itself after every upload and whenever the
desktop app first connects to the archive in a session. It seals packs left by
interrupted uploads, adds PAR2 to new packs, checks packs not verified in 90
days against their PAR2 data, and once a day purges old trash, saves a
PAR2-protected catalog snapshot, and rewrites `INDEX.tsv`: a plain-text list
of every file and where its bytes are, so any file can be found with `grep`
and extracted with standard tools (see [docs/FORMAT.md](docs/FORMAT.md)). `archive status` shows the last report.
`archive-helper` finds par2 in the archive's `tools/`, `~/bin`,
`~/.local/bin`, `/usr/local/bin`, or on the PATH.

Bundles made by the earlier move2storage tool (`<part>.bundle.tar`) are
imported on the server. Each bundle is checked against its `.sha256` file and
its manifest, unpacked into a staging folder, and moved into the archive, where
its folder becomes a project (later parts of the same folder are added to it).
The bundles themselves are left alone, and a receipt in `imports/` lets an
interrupted import be run again:

```
~/.local/bin/archive-helper import-bundles --root <archive> --to /Projects /path/to/backups/*.bundle.tar
```

Parts that tool staged but never bundled are imported from its job folder
(the one holding `allowed-parts.json`). Each file is checked against its
receipt, bundled parts are skipped, and the job folder is left untouched:

```
~/.local/bin/archive-helper import-staged --root <archive> --to /Projects /path/to/backups/.move2storage-<job>
```

An hourly cron entry is optional, for archives that go untouched for long
periods: `~/.local/bin/archive-helper cron install --root <archive>`.

## Command-line use

```
export ARCHIVE_ROOT=/mnt/pool/me/archive                  # or ssh://host/path
archive init
archive put ~/data/2024_sequencing --to /Projects --move   # a new project; deletes local files once all are archived and verified
archive put ~/notes/*.pdf --to /Projects --new-project "Notes 2024"   # loose files need a project
archive put ~/data/extra_run --to /Projects/2024_sequencing      # adds to that project
archive ls -l /Projects/2024_sequencing
archive info /Projects/2024_sequencing                      # space used, duplicates, protection
archive find dairy methylation type:csv after:2023          # words match names and folders; filters optional
archive get /Projects/2024_sequencing/raw ~/restore         # any file or subfolder
archive protect                                             # PAR2 for sealed packs (local archives)
archive scrub                                               # verify everything, repair damage
archive maintain && archive status                          # the same, on a server
archive mv /Projects/old /Finished/old                      # projects move as a whole
archive rm /Projects/old && archive trash list && archive trash restore <id>
archive trash purge                                         # after the trash period (30 days)
archive index                                               # write INDEX.tsv now (local archives)
```

Interrupted transfers resume by re-running the same `put` (files already
stored are recognized by checksum). Use `--job <key>` to continue writing into
the same packs. A file already in a project is never replaced: `--on-conflict
skip` (the default) leaves it, and `keep-both` stores the new one as "name
(2)". Archives on a network share are refused; use `ssh://host/path` so the
server opens the catalog on its own disk. `--json` prints machine-readable events.
