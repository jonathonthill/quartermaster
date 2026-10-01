# Developing Quartermaster

Building, testing, setting up archive servers by hand, importing old backups, and the
`archive` command-line tool. For using the app, see the [README](../README.md). The
on-disk format is in [FORMAT.md](FORMAT.md) and the wire protocol in
[PROTOCOL.md](PROTOCOL.md).

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
([release.yml](../.github/workflows/release.yml)).

To work on the interface without the backend, serve `app/ui` over HTTP; it runs with
sample data.

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
and extracted with standard tools (see [FORMAT.md](FORMAT.md)). `archive status` shows the last report.
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
