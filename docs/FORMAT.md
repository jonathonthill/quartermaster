# Archive on-disk format (version 1)

This document is the contract for data stored by the Archive tool. Anyone
holding this file and an archive folder can read the data back, with or
without the Archive software.

## Projects

Data is stored in **projects**. Each folder sent to the archive becomes a
project; folders above projects only organize them. Projects are frozen:
nothing inside one is ever renamed, moved, replaced, or deleted on its own,
though new files can be added. A project as a whole can be renamed, moved
between organizing folders, or moved to the trash. Projects are
self-contained: identical files are stored once *within* a project, never
shared between projects, and every pack holds one project's data. So a
project can be recovered from its own packs, and deleting one frees exactly
its packs.

Data stored before projects existed (catalog schema 2 and earlier) belongs to
no project. It stays readable and can be reorganized, but nothing new is
written outside a project.

## Folder layout

```
<archive root>/
  archive.json          format version, archive id, pack size, PAR2 %, trash days
  catalog.db            SQLite: the folder tree users browse (see "Catalog")
  INDEX.tsv             every file and where its bytes are, rewritten daily (see "Finding one file")
  INDEX.tsv*.par2       PAR2 for the index
  snapshots/            daily catalog copies (catalog-YYYY-MM-DD.db), each with PAR2
  packs/YYYY-MM/
    pack-00000042.tar                 immutable once sealed
    pack-00000042.tar.par2            PAR2 index
    pack-00000042.tar.vol*.par2       PAR2 recovery blocks (10% by default)
```

## Packs

A pack is a plain, uncompressed POSIX tar (ustar, with pax headers for long or
non-ASCII names). Sizes of 8 GiB and above use the GNU base-256 size field.
GNU tar, bsdtar, and Python's `tarfile` all read it. Members, in order:

| Member | Contents |
|---|---|
| `.archive/PACK.json` | Format version, archive id, pack name, creation time, and `project`: its id and path in the archive when the pack was written. Always first. |
| `<path>/` | Folder entries (mode and mtime). |
| `<path>.zst` | One large file (above 1 MiB by default), compressed as described below. `<path>` is its path in the archive when it was stored. |
| `.archive/solid-<pack>-<n>.tar.zst` | Many small files. Decompressed, this is a tar whose members are the files at their full archive paths, uncompressed. |
| `<path>` (symlink) | A symbolic link, stored as a tar symlink member. |
| `.archive/MANIFEST.json` | The project (id and path, as in `PACK.json`), and every entry in the pack: path, kind, SHA-256, size, mtime (ns), mode, tar member, offset within a solid block, and `same_as` for deduplicated files. Entry paths start with the project's path. |
| `.archive/DUPLICATES.tsv` | Present if any file was deduplicated: `copy<TAB>stored copy<TAB>sha256<TAB>mtime` per line, the copy's own modification time as a `touch -t` stamp in UTC (`CCYYMMDDhhmm.SS`). Packs sealed before this column existed have three columns. |
| `.archive/RECOVERY.txt`, `.archive/recover.sh` | Instructions and a POSIX shell script for recovery with standard tools. |
| two zero blocks | End of archive, written when the pack is sealed. |

A pack still being written (state `open`) has no manifest or end marker, but is
still a valid tar prefix: `tar -tf` lists it and `tar -xf` extracts it.

### Compression: zstd seekable format

Every `.zst` member uses the [zstd seekable format]: independent zstd frames
(8 MiB of input each by default, each with a content checksum), followed by a
skippable frame (magic `0x184D2A5E`) holding a seek table (per frame:
compressed size u32, decompressed size u32; footer: frame count u32,
descriptor u8, magic `0x8F92EAB1`). Stock `zstd -d` decompresses it. The seek
table lets a reader decompress only the frames covering the bytes it needs.

Incompressible data (already-compressed formats such as `.gz`, `.bam`, or
images) is detected from a sample and compressed at level 1, where zstd stores
it in raw blocks. So every file is always exactly one `.zst` member.

[zstd seekable format]: https://github.com/facebook/zstd/blob/dev/contrib/seekable_format/zstd_seekable_compression_format.md

### Recovery without the software

```
par2 verify packs/2026-09/pack-00000042.tar.par2    # repair with "par2 repair" if damaged
tar -xf pack-00000042.tar .archive/recover.sh
sh .archive/recover.sh pack-00000042.tar OUTPUT_DIR
```

Recover a project's packs in order (lowest number first) into the same output
folder; `PACK.json` says which project a pack belongs to. A deduplicated file
is recreated by copying its stored twin, which may be in an earlier pack of the
same project. A file stored twice (re-sent by the same transfer because it
changed while being read) ends up as the later copy.

### Finding one file

`INDEX.tsv` lists every file outside the trash, one per line after `#` comment
lines and a header: `path`, `size`, `sha256`, `pack`, `start`, `length`,
`offset`, `member`, `stored_as`. Tabs, newlines, and backslashes in names are
written as `\t`, `\n`, and `\\`. To extract a file:

```
tail -c +$((START + 1)) packs/PACK | head -c LENGTH | zstd -dc | tail -c +$((OFFSET + 1)) | head -c SIZE > FILE
```

`START` and `LENGTH` locate the compressed tar member in the pack, and
`OFFSET` the file within it once decompressed (0 unless it is in a solid
block). A `SIZE` of 0 is an empty file. `member` and `stored_as` name the same
bytes for `tar`; `stored_as` is the path at the time it was stored, which
differs from `path` if the project was renamed or moved since.

## Catalog

`catalog.db` is SQLite in WAL mode. It maps the virtual tree to stored bytes:

- `nodes`: the tree (folders, files, links), with recursive size and file counts on folders, and `project` (the project id) on every node in a project. Node 1 is the root and node 2 is the trash.
- `projects`: each project's id and top folder.
- `contents`: one row per distinct SHA-256 per project (deduplicated within the project; `''` for data from before projects), with a reference count and its location (blob and offset within the blob's decompressed stream).
- `blobs`: tar members holding data (`f` for one file, `s` for a solid block), with data offset and length within the pack.
- `packs`: project, state (`open`, `sealed`, `protected`, `damaged`), committed length, and last integrity check.
- `pack_entries`: what each pack's manifest records.
- `name_index`: an FTS5 word index with one row per node outside the trash. `words` holds its name, split into camelCase and letter/digit pieces; `folders` holds the same for every folder above it. It is derived data: `catalog::reindex_all` rebuilds it, and a catalog without it (schema 1) gets it on first open.

Renames, moves, and trash are catalog-only. Pack contents never change once
sealed, apart from PAR2 repair, which restores the original bytes. A
project's packs still being written are sealed before the project is renamed,
moved, or trashed, so every pack's paths match where its project was.

The catalog can be rebuilt from the packs alone (`archive-helper rebuild`).
Projects come back as projects, each where its newest pack says. Other
renames, moves, and the trash are catalog-only, so restore a snapshot for
those.

Schema 3 added projects. Older catalogs are upgraded when first opened. The
catalog must be opened on the machine whose disk holds it: the software
refuses an archive on a network share (SMB, NFS, AFP, WebDAV), where SQLite's
write-ahead log can be damaged.

## Write protocol (why a crash can't corrupt committed data)

1. The file's bytes are appended to the job's open pack and fsynced.
2. The bytes are read back (on Linux, after evicting them from the page cache), decompressed, and hashed. The hash must equal the sender's SHA-256 of the original.
3. One SQLite transaction then adds the blob, content, and tree node, and records the pack's new committed length.

Step 2 runs on its own thread, so the next file can arrive while it does (up
to four checks at once). Files are committed (step 3) strictly in the order
they were written, each only after its own check passes; until then its bytes
lie past the committed length. Anything else (listing, linking, sealing, or
switching packs) first waits for every running check.

If a check fails, the pack is truncated back to that file's start, which also
discards every file written after it; they are reported to the sender (the
`Settle` request) and sent again. If the process dies anywhere, the next
writer to the same job truncates the pack to its committed length. A sender
in Move mode deletes its local copies only after the whole transfer is
committed and sealed, and only files confirmed unchanged since they were read.
