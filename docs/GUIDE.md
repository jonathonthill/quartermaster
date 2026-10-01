# Quartermaster guide

How the app behaves, in detail. For a quick start, see the [README](../README.md).

## The two modes, panes, and the Dock

The desktop app is called Quartermaster, and it has a few names of its own: an
archive is a datahold, a project a barrel, the trash Overboard, and the list
of transfers the Dock. It has two modes, chosen by the switch in the top bar:
**Stow** puts folders into a datahold (see [Barrels](#barrels-projects-in-a-datahold)), and **Transfer**
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
`~/.ssh/config`, keys, and known hosts all apply. Password, two-factor (2FA), and new-host
prompts appear in the app. The left pane shows this computer or a file server,
and the right pane an archive: → sends to the archive (you choose Copy or Move
in the confirmation), ← retrieves from it (always a copy). To type a folder
path, click the empty part of the path bar, press ⌘L, or type `/` or `~` in a
file list; Tab completes folder names. The bar along the bottom, the Dock,
sums up transfers; click it to see each one. Every connection the app makes to
a server shares one sign-in.
If your ssh config already shares connections (ControlMaster), the app uses
those; otherwise it keeps its own for two hours.

## Barrels (projects in a datahold)

Each folder sent to a datahold becomes a project, shown in the app as a sealed **barrel**.
Barrels are frozen: you can add files to one, and rename, move, or throw it overboard as a
whole, but nothing inside it can be renamed, replaced, or deleted on its own. Each barrel is
self-contained (duplicates are stored once within it, and its packs hold nothing else), so one
can be recovered or deleted by itself. Loose files go into an existing barrel or a new one.

## Adding servers

Choose **Add a file server…** or **Add a datahold…** from a pane's menu (or **Add server** in
Settings). The Type menu offers what fits the pane. **Save this server** is ticked by default;
unticked, the server is a one-off connection, shown as "not saved", that lasts until you quit
and is never written to your settings. To keep it after all, tick **Save this server** in
Settings. A transfer to an unsaved server that's still unfinished when you quit can't continue
after a restart, since the server is gone.

## Windows, closing, and quitting

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

## SFTP servers

A server where the helper can't be installed can be added in
Settings as an **SFTP server**. The app speaks SFTP over the system `ssh`, so
sign-in works as for other servers. An SFTP server takes part in Transfer mode
(browsing, Places, search, copying and moving either way, new folders, and the
Trash), but can't be a source for stowing. It can't compute checksums, so copies
to and from it are checked by size and date, and the Dock says "Size matches"
instead of "Checksums match". Turn on **Check uploads by reading them back** in
its settings to have each upload read back and its checksum compared, which takes
about twice as long.

## Deleting files and the Trash

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

## Pausing, resuming, and Abandon ship

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

## Transfers that run on a file server

Transfers between a file server and an archive run on the file server, so
the data goes directly between the two servers and you can close the app.
When you start one, the file server signs in to the archive server itself,
and its password or two-factor prompt appears in the app. The file server keeps that
connection open while its transfers run, and for two hours afterward. If the
connection closes, for example because the server restarted, the transfer
pauses. Its row in the Dock then offers **Sign in**, and the transfer
continues where it stopped. For archive servers that accept SSH keys, Settings
can instead give each file server a limited key, which can only add and read
data, so transfers never need a sign-in.

## Sending through this computer

If a file server can't reach the archive server, the app offers to send
through this computer instead. You can also set this per file server in
Settings (**Send through this computer**), for servers that don't let programs
keep running after you log out. The file server still reads, checksums, and
compresses the files, and in Move mode deletes the originals only once the
whole transfer is archived and verified. The data passes through this computer, so keep it awake with
the app open until the transfer finishes. A transfer that loses its connection
pauses, with a note saying so; press play to reconnect and continue where it
stopped. Nothing retries on its own.
