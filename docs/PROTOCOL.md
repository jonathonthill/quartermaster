# Client–helper protocol (version 8)

Clients (the CLI, the desktop app, and file-server jobs) run
`archive-helper serve --root <archive>` on an archive server, or
`archive-helper files --start <folder>` on a file server, over SSH and talk to
it on the channel's stdin and stdout. Authentication, encryption, and the
user's identity all come from SSH. The helper runs as that user, so Unix
permissions keep each person's archive separate.

## Framing

Every frame is `kind (u8) | length (u32, little-endian) | payload`:

| Kind | Payload |
|---|---|
| 1 `Msg` | One JSON `Request` or `Response` (`{"op": ..., "args": ...}` / `{"r": ..., "v": ...}`) |
| 2 `Data` | Raw bytes: an upload chunk, or one zstd frame of a download |
| 3 `End` | JSON trailer closing a byte stream (`{"digest": "<sha256>"}` for uploads) |
| 4 `Abort` | UTF-8 error message; the sender gave up partway through a stream |

The session starts with `Hello`. Client and helper must speak the same
protocol version; otherwise the helper answers with an error asking for an
update, and the app offers to update it. Requests are then answered strictly
in order.

## Streams

- **Upload** (`PutFile`, `PutSolid`). The server first answers `Proceed`, or a final outcome such as "skipped" when there's nothing to send. The client then streams the seekable-zstd payload as `Data` frames and closes with `End{digest}`. The server writes and fsyncs, answers with the outcome, and reads back, decompresses, and verifies the bytes against the digest on another thread while the next upload arrives; the file is committed once its check passes. `Settle` waits for those checks and answers `Failed` with any file whose check failed (it wasn't kept, so the client sends it again). `FinishJob` settles first, and fails if any check failed since the last `Settle`. The server always consumes the stream through `End` or `Abort` before answering, so an error never leaves the two sides out of step.
- **Download** (`ReadFile`, `ReadMany`). For each file the server sends `FileStart{entry, frames, skip, len}`, one `Data` per zstd frame, then `End`. Large files travel as their stored frames, with no recompression. A small file inside a shared block is decompressed on the server and sent as its own frame, so transfer size tracks file size. `ReadMany` sends several of these back to back. The client checks every file's SHA-256 before it is written under its real name.

## Batching

`SizesPresent`, `Have`, `Mkdirs`, `LinkMany`, and `ReadMany` each carry many
items, so dedup checks, folder creation, dedup references, and retrieval of
many small files cost one round trip per batch rather than one per file.

## Projects

Version 3 added projects (see [FORMAT.md](FORMAT.md)). `Mkdirs` marks each
folder that was sent with `"project": true`, and it becomes a project unless it
lands inside one. Deduplication is per project: `SizesPresent{scope, sizes}`
and `Have{scope, hashes}` answer for the project at `scope`, which is any path
in or below the project's top folder. Uploads, links, and symlinks are refused
outside projects. `Replace` is refused unless the file being replaced was
stored earlier by the same job, so a file that changed while it was read can be
sent again. `Rename`, `Trash`, `Restore`, and `CreateFolder` refuse anything
inside a project. `Stat` fills in `project_path` for items in a project, and
entries carry `is_project` and `in_project`.

## Restricted mode

`serve --restricted` (for limited keys that file servers can use) refuses
`Init`, `Rename`, `Trash`, `Restore`, `TrashList`, and the key-management
requests. Such a connection can read and add data, but never remove or
reorganize it.

## File servers and signing in

A `files` session browses (`FsList`, `FsSearch`, `FsMeasure`, `FsMkdir`, and
`FsPlaces` for its home folder and mounted drives) and runs transfer jobs
(`JobStart`, `JobList`, `JobCancel`; `JobPause` and `JobResume`, which stop a
job at the next file and let it continue where it stopped; and `JobDiscard`,
which removes what a stopped retrieve created on the file server, recorded in
the job's `created` list). A job reaches its
archive in one of four ways (`ArchiveTarget`):
- directly, when the archive is on the same machine;
- with a limited key;
- over a **signed-in link**;
- **relayed** through the client.

### Plain file transfers

Transfer mode in the app copies ordinary files with no archive involved
(`crates/core/src/copy.rs`). A `files` session also has these operations, in
which paths are absolute on the server (`~` is expanded):
- `FsStat{path}` answers `Stat` (size, times, mode, or none if missing).
- `FsWalk{root}` answers `Walked`: everything under `root`, links listed but
  not followed, and any folders that couldn't be read.
- `FsMkdirs{paths}` creates folders and missing parents; existing ones are fine.
- `FsRemove{path}` removes a file or an **empty** folder. It never removes
  anything recursively.
- `FsRead{path, offset}` answers `FsStart{size}`, then `Data` frames and `End`.
  The trailer's digest is the SHA-256 of the *whole* file, though only the bytes
  from `offset` are sent (the server reads and hashes the part before it).
- `FsWrite{path, size, mtime_ns, mode, offset}` answers `Proceed`. The client
  sends `Data` frames and `End{digest}` (the whole file's SHA-256). The server
  writes to a hidden `.<name>.qm-part` file beside the target, checks the byte
  count and digest, sets the mode and modification time, and only then renames it
  into place; `Ok` means all of that happened. A mismatch deletes the part file
  and answers `Verify`. An `Abort`, or a lost connection, keeps the part file, and
  a later `FsWrite` with `offset` set to its length continues it (the server
  re-hashes the part it already has).

- `FsTrash{paths}` sends items to the helper's own Trash (a plain file server has none; see
  `crates/core/src/trash.rs`). Each item is *moved*, so it is instant however big. It goes
  into a hidden trash folder under the user's home, or, if it is on another disk, into a
  hidden `.qm-trash/` beside where it was; a note records where it came from. Answers a
  report: the items trashed, and any that couldn't be (a place the user can't write), with
  whether deleting them for good would be allowed. Items that may never be touched (the
  top of a disk, the home folder and anything holding it, mount points, something already in
  the Trash) or are missing fail the whole request, and nothing moves. Nothing is deleted
  automatically. `FsTrashList` lists the Trash, `FsTrashRestore{id}` puts an item back
  (never overwriting), and `FsTrashEmpty{ids}` deletes the given items, or all, for good.
- `FsDelete{paths}` deletes files and folders (everything inside a folder too)
  immediately and for good, since a file server has no Trash. It refuses the top of
  a disk, the home folder and anything holding it, mount points, and any folder that
  holds another disk (a delete never crosses onto another disk, and never follows a
  link), and it checks every item before deleting any, so a refused request deletes
  nothing. Answers `Id(n)`, how many items were deleted.

The client copies between any two sides (this computer or a file server) by
reading from one and writing to the other, so the bytes always pass through the
client, and the destination verifies against the digest the source computed. A
file with the same size and modification time as the source is left alone, so a
repeated or resumed copy doesn't redo work. Moving is a copy until every file has
arrived and verified; only then are the originals removed, and only those unchanged
since the scan (a file already at the destination that a Move didn't copy itself
is first read on both sides and compared). If any file failed or the copy was
stopped, nothing is removed. These transfers run in the app, not as server jobs.

A relayed job isn't started by the server. The client runs `archive-helper job
run --id <id>` over SSH and joins that process's stdin and stdout to an
`archive-helper serve` session of its own, so the job speaks this protocol to
the archive through the client. When the client disconnects, the job ends in
the `interrupted` state. The server doesn't restart relayed jobs itself
(`JobList{resume_interrupted}` skips them); the client restarts them.

A signed-in link is the file server's own SSH connection to the archive
server, kept open so jobs need no sign-in of their own. `SignIn{target}` opens
it. While ssh signs in, the helper sends each of its questions to the client as
`Prompt{id, text}`, such as a password, a two-factor code or choice, or a yes/no about an unknown
host. The client answers each one with `PromptAnswer{id, answer}`, where a null
answer cancels the sign-in. The exchange ends with `Ok` or an error. Behind
this, the helper runs `ssh -f -N` as a ControlMaster with a private socket and
`SSH_ASKPASS` pointing at itself, and passes each question over a Unix socket.
Answers are never stored. `LinkCheck` reports whether the link is open, and
`SignOut` closes it. The link closes by itself two hours after the last job
using it ends. A job whose link has closed waits in the `waiting` state,
naming the server to sign in to, and continues once the link is open again.
Files already archived are recognized and skipped.
