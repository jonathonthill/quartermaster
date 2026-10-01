<p align="center">
  <img src="app/src-tauri/icons/128x128@2x.png" width="128" alt="Quartermaster icon: a frigate under sail by moonlight">
</p>

<h1 align="center">Quartermaster</h1>

<p align="center">
  A safe, verifiable home for finished research data.<br>
  Stow past projects in an archive on your lab's storage server, and get any file back years later.
</p>

<p align="center">
  <a href="https://github.com/jonathonthill/quartermaster/releases"><b>Download for macOS, Windows, or Linux</b></a>
</p>

## Why

Finished projects pile up on analysis servers, laptops, and external drives. Moving them to
the lab's storage server by hand is slow and easy to get wrong. A large copy can be silently
corrupted, and years later nobody can say whether an archived project is complete, or still
readable. Quartermaster makes archiving a project as simple as dragging a folder, and keeps
checking that it stays intact.

## Dataholds

A **datahold** is an archive on your storage server. Each folder you stow becomes a sealed
project, called a **barrel**.

| | |
|---|---|
| **Checked end to end** | Every file is checksummed (SHA-256) where it starts and again where it lands. A file counts as archived only once the two match. |
| **Originals kept until it's safe** | Choose *Move*, and the originals are deleted only after the whole project is archived and verified. |
| **Protected against damage** | Data is bundled into large packs with PAR2 recovery data, so damaged blocks can be repaired, not just detected. |
| **Checked over time** | The server rechecks every pack on a schedule and repairs what it can, so problems surface early, not years later. |
| **Sealed barrels** | An archived project can be added to, renamed, or moved as a whole, but nothing inside it can be changed or deleted by accident. |
| **Compact** | Files are compressed, and duplicates within a project are stored once. |
| **Easy to get back** | Browse the datahold like ordinary folders, search it, and retrieve any single file or folder without unpacking anything. |
| **Never locked in** | A plain-text index lists every file and where its bytes are, so data can be found with `grep` and extracted with standard tools, even without Quartermaster. The format is [documented](docs/FORMAT.md). |
| **Forgiving** | Something thrown *Overboard* can be restored for 30 days. |

Getting data there is just as careful:

- **Straight from the analysis server.** Projects on an analysis server go directly to the datahold, server to server, so you can close your laptop while they move.
- **Interruptions don't matter.** Pause, lose the connection, or restart: a transfer continues where it stopped.
- **Undo a mistake.** *Abandon ship* (the skull) stops a transfer and removes only what it created.
- **Your usual sign-in.** The app uses your computer's own `ssh`, so your `~/.ssh/config`, keys, passwords, and Duo all work, with the prompts shown in the app.

## Also: everyday transfers

The same careful copying works for ordinary files too. Switch the top bar from **Stow** to
**Transfer**, and both panes can show this computer or any server, like an SFTP client. Files
arrive exactly as they were, each one checksummed, and they can be resumed and undone the same
way. Servers you can't install anything on can be added as **SFTP servers**.

## Getting started

1. **Download** the app from [Releases](https://github.com/jonathonthill/quartermaster/releases). The builds aren't signed yet, so the first launch needs one extra step:
   - **macOS**: drag Quartermaster to Applications, then right-click it and choose **Open**, and **Open** again. If macOS says the app "is damaged", run `xattr -dr com.apple.quarantine /Applications/Quartermaster.app` in Terminal.
   - **Windows**: if SmartScreen appears, choose **More info**, then **Run anyway**. (Windows builds are newer and less tested.)
   - **Linux**: use the `.AppImage` (make it executable) or the `.deb`.
2. **Add your datahold.** In the right pane's menu, choose **Add a datahold…**. Enter the storage server's address and the folder for the archive, then press **Connect**. If there's no datahold there yet, **Test connection** offers to create one.
3. **Let it install its helper**, if asked. A small program goes into your home folder on the server; nothing needs an administrator.
4. **Stow a project.** On the left, open the folder holding it (on this computer, or an analysis server added with **Add a file server…**). Select it and press **→**. Choose **Copy** or **Move**, and watch it in the **Dock** along the bottom.

## Words you'll see

| In the app | Means |
|---|---|
| **Datahold** | An archive on a storage server |
| **Barrel** | A sealed project in a datahold |
| **Dock** | The list of transfers along the bottom |
| **Abandon ship** | Stop a transfer and undo it |
| **Throw overboard** | Delete: to the Trash, or to a datahold's **Overboard** (restorable for 30 days) |

## More

- [Guide](docs/GUIDE.md): how everything behaves, in detail
- [Developing](docs/DEVELOPING.md): building, setting up servers by hand, importing old backups, and the `archive` command-line tool
- [Storage format](docs/FORMAT.md) and [protocol](docs/PROTOCOL.md)

Quartermaster is MIT licensed. The server helper runs on Linux and FreeBSD (including TrueNAS) on x86_64.
