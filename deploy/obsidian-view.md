# How to view a read-only vault in Obsidian

A read-only instance ([ADR-0030](../docs/adr/0030-read-only-vault.md)) stops
agents from writing. It does not stop a person who opens the same directory in
Obsidian. This guide gives Obsidian a view of the vault that the kernel keeps
read-only, while Obsidian still saves its own settings.

The server keeps writing the vault through its real path: `git` fast-forwards
there, and `.md-mcp/` lives there. Obsidian opens a second path that shows the
same files.

Tested with Obsidian 1.15.1. The Windows steps were tested on Windows 11 with
WSL 2.7 (Ubuntu 26.04) and Samba 4.23.

## 1. Mount the view

Pick three paths:

- `<vault>`: `MD_VAULT`, the directory the server writes.
- `<view>`: an empty directory. Obsidian opens this one.
- `<obsidian-state>`: an empty directory outside the vault. It holds Obsidian's
  settings.

Create the mount point for `.obsidian` inside the vault. A read-only mount
cannot create it later, and the mount fails without it.

```sh
mkdir <vault>/.obsidian <view> <obsidian-state>
```

git ignores the empty directory, and the server's fast-forward is not affected.
If the upstream repository tracks its own `.obsidian/`, the view hides it behind
`<obsidian-state>`.

Add both mounts to `/etc/fstab`, in this order:

```
<vault>           <view>            none  bind,ro  0 0
<obsidian-state>  <view>/.obsidian  none  bind     0 0
```

```sh
sudo systemctl daemon-reload
sudo mount <view> && sudo mount <view>/.obsidian
findmnt -R <view> -o TARGET,OPTIONS    # <view> is ro, <view>/.obsidian is rw
```

On Linux, open `<view>` as a vault in Obsidian. You are done.

Do not use overlayfs for the view. Every edit appears to succeed and is lost
when the overlay is unmounted.

## 2. Windows: share the view from WSL

Obsidian on Windows cannot open `\\wsl.localhost\<distro>\...`. That path does
not support file-change notifications, and Obsidian stops loading with
`EISDIR: illegal operation on a directory, watch`. Share the view over SMB
instead.

Create an account that can read the vault and cannot write it. Samba checks
this account's permissions before it accepts a delete; with only the read-only
mount, Windows reports a delete as successful although the file stays.

```sh
sudo apt install samba
sudo useradd -r -M -s /usr/sbin/nologin obsview
sudo chown -R obsview: <obsidian-state>
sudo smbpasswd -a obsview
```

The vault files must be readable by others (git's default modes are).

`/etc/samba/smb.conf`:

```ini
[global]
   server role = standalone server
   map to guest = never
   server min protocol = SMB3
   smb ports = 4450
   smb2 leases = no
   smb3 directory leases = no
   load printers = no
   disable spoolss = yes

[vault]
   path = <view>
   valid users = obsview
   read only = no
   volume = md
   oplocks = no
   level2 oplocks = no
   veto files = /.git/.md-mcp/
```

```sh
sudo systemctl enable --now smbd && sudo systemctl restart smbd
```

Why these settings:

| Setting | Reason |
|---|---|
| `smb ports = 4450` | Windows owns port 445 on `localhost`. |
| `smb3 directory leases = no` | The server changes files outside Samba. With directory leases, Windows caches the folder listing and never shows a pulled file. |
| `smb2 leases = no`, `oplocks = no`, `level2 oplocks = no` | Same cause, for file contents. |
| `volume = md` | Samba reports the share name as the volume label. With a label longer than three characters, Obsidian's file watcher fails on Windows: the vault opens, and pulled changes never appear. |
| `read only = no` | `.obsidian` must stay writable. The mount and the `obsview` account refuse writes to notes. |
| `veto files` | Hides `.git` and `.md-mcp` from Obsidian. |

WSL stops a distro about 15 seconds after its last session ends, even while
`smbd` and `md-server` run, and the share goes away with it. Turn that off in
`%USERPROFILE%\.wslconfig`, then run `wsl --shutdown` once:

```ini
[general]
instanceIdleTimeout=-1
```

On Windows 11 24H2 or later, connect and open the share:

```powershell
net use \\localhost\vault /user:obsview /TCPPORT:4450
```

In Obsidian, choose **Open folder as vault** and enter `\\localhost\vault`.

## 3. Verify

- Start Obsidian. The file list is complete and links resolve. The console shows
  one error for `.OBSIDIANTEST`; that is Obsidian's case-sensitivity test
  failing to write, and it is harmless.
- Edit a note. Obsidian reports `EROFS` or `EPERM` and the file does not change.
- Change a setting and restart Obsidian. The setting is kept.
- Call `sync_vault` after an upstream commit. New and changed notes appear in
  Obsidian within seconds.
- `git -C <vault> status --porcelain` prints nothing.

## Limits

- Something must start the distro after Windows boots. Until it runs, the share
  does not exist and Obsidian fails to load the vault.
- SMB matches names without regard to case. Two notes whose names differ only
  in case read as the same file on Windows.
- The editor keeps unsaved text after a refused save, so the screen can differ
  from the file until you reload the note.
- Not tested: macOS, mobile, storing the `net use` credential across logons.
