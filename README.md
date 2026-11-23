# wormhole

Keeps a live copy of a folder on another Linux server. One program, run on both servers:

- **sender** (the server that owns the files) watches the folder and sends every change, usually
  within 0.2 s;
- **receiver** (the server that gets the copy) writes each change into its copy.

It copies one way only: the receiver's copy becomes exactly the sender's folder (new, changed and
deleted files, folders, permissions). A typical use is moving a running service to a new server:
keep its data folder mirrored until the moment you switch, so nothing written before the switch is
lost.

## Install

On any Linux server (x86_64 or arm64, glibc 2.17 or newer: Ubuntu, Debian, Amazon Linux, RHEL):

```
curl -fsSL https://cdn.broadwayinc.com/get/wormhole/v1 | sh
```

This puts the `wormhole` program in `/usr/local/bin` (and this guide plus the license files in
`/usr/local/share/doc/wormhole`), so every user can run it. It uses sudo for that folder (on most
cloud images sudo asks nothing) and checks the download's SHA-256 first. Running it again updates to
the newest version. Options, given before `sh`: `WORMHOLE_VERSION=0.1.2` (a specific version),
`WORMHOLE_INSTALL_DIR=/some/bin` (another folder).

```
wormhole keygen  <key file>
wormhole receive --root <dir> --listen <ip:port> --key <key file>
wormhole send    --root <dir> --to <ip:port> --key <key file> [--exclude <path>]...
wormhole verify  --root <dir> --to <ip:port> --key <key file> [--exclude <path>]...
```

## Setup

The examples below use these names; put in your own:

| | Sender | Receiver |
|---|---|---|
| Private IP | `10.0.1.10` | `10.0.1.20` |
| Folder | `/srv/app/data` | `/srv/app/data` |
| Key | `~/wormhole/wormhole.key` | `~/wormhole/wormhole.key` |
| Port | | `7700` |

Use the servers' private network addresses when they share one (same VPC or LAN).

### 1. Install on both servers

```
curl -fsSL https://cdn.broadwayinc.com/get/wormhole/v1 | sh
wormhole --version
```

### 2. Make the shared key (sender)

```
mkdir -p ~/wormhole
wormhole keygen ~/wormhole/wormhole.key
```

Both servers need this same file. Anyone who has it can write into the receiver's folder, so it is
created readable by you only (mode 600), and it is never sent over the connection. Keep it out of git.

### 3. Put the same key on the receiver

The key is one line of 64 characters. On the sender, show it:

```
cat ~/wormhole/wormhole.key
```

On the receiver, write that same line into a file only you can read (`umask 077` makes the file
mode 600):

```
mkdir -p ~/wormhole
(umask 077; echo "<the 64 characters shown above>" > ~/wormhole/wormhole.key)
```

(Or copy it over SSH: `scp -p ~/wormhole/wormhole.key <user>@10.0.1.20:~/wormhole/`.)

### 4. Let the sender reach the receiver's port

Open TCP port 7700 on the receiver for the sender only. On AWS, add an inbound rule to the
receiver's security group that allows the sender's security group:

```
aws ec2 authorize-security-group-ingress --region <region> \
  --group-id <receiver security group id> \
  --ip-permissions 'IpProtocol=tcp,FromPort=7700,ToPort=7700,UserIdGroupPairs=[{GroupId=<sender security group id>,Description="wormhole"}]'
```

With a host firewall instead (ufw): `sudo ufw allow from 10.0.1.10 to any port 7700 proto tcp`.

Never open the port to the whole internet. The key is a second lock: a connection without the right
key is dropped at the first message.

### 5. Start the receiver

If a service on the receiver uses the folder, stop it first: the copy replaces the folder's
contents. Keep a backup of what is there now:

```
cp -a /srv/app/data ~/data-backup-$(date +%F)
```

Then run the receiver under pm2 (or systemd, below) so it comes back after a crash or a reboot:

```
pm2 start /usr/local/bin/wormhole --name WORMHOLE_RECV --interpreter none -- \
  receive --root /srv/app/data --listen 10.0.1.20:7700 --key /home/<user>/wormhole/wormhole.key
pm2 save
pm2 logs WORMHOLE_RECV --lines 5 --nostream
```

You should see `receiving into /srv/app/data, listening on 10.0.1.20:7700`.

### 6. Start the sender

```
pm2 start /usr/local/bin/wormhole --name WORMHOLE_SEND --interpreter none -- \
  send --root /srv/app/data --to 10.0.1.20:7700 --key /home/<user>/wormhole/wormhole.key
pm2 save
pm2 logs WORMHOLE_SEND --lines 5 --nostream
```

You should see `connected to 10.0.1.20:7700` and then `catch-up done in ... s`. A folder of a few
thousand small files is copied in about a second.

To skip scratch files, add `--exclude <path inside the folder>` (repeatable), for example
`--exclude tmp`. Excluded paths are left alone on both sides: not sent, and not deleted on the
receiver.

### 7. Check that both sides are equal (sender)

```
wormhole verify --root /srv/app/data --to 10.0.1.20:7700 --key ~/wormhole/wormhole.key
```

Use the same `--exclude` options as the sender. It prints `IN SYNC` (exit code 0) or every
difference (exit code 1). While files are being written, one changed in the last fraction of a
second can show up as a difference: run it again.

### Running with systemd instead of pm2

```
sudo tee /etc/systemd/system/wormhole-send.service >/dev/null <<'EOF'
[Unit]
Description=wormhole sender
After=network-online.target
Wants=network-online.target

[Service]
User=<user>
ExecStart=/usr/local/bin/wormhole send --root /srv/app/data --to 10.0.1.20:7700 --key /home/<user>/wormhole/wormhole.key
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload && sudo systemctl enable --now wormhole-send
journalctl -u wormhole-send -n 5
```

The receiver is the same with `receive --root ... --listen ...` in `ExecStart`.

## Switching over to the new server

1. Stop the service on the sender. From here on nothing changes in the folder.
2. Run `wormhole verify` until it says `IN SYNC` (the last changes arrive within a second).
3. Start the service on the receiver. A service that reads its folder into memory at start must be
   started (or restarted) only now, after the last change has arrived.
4. Send traffic to the receiver (DNS, load balancer).
5. Stop wormhole on both servers and close the port.

Never run the service on both servers with live traffic at the same time: each would change its own
copy, and the copies would drift apart.

To go back, run wormhole the other way (the new server sends, the old one receives) so whatever the
new server wrote comes back, then switch traffic back.

## Running day to day

If the receiver restarts or the network drops, the sender retries (1 s, then up to every 30 s) and
on reconnect compares everything, so nothing is lost while they are apart. Every 10 minutes the
sender also compares the whole folder with the receiver (`--rescan-secs`), in case a change was
missed.

What the log lines mean:

| Line | Meaning |
|---|---|
| `connected to ...; the receiver has N files and folders` | connection made; the comparison follows |
| `catch-up done in X s: ...` / `already in sync` | after (re)connecting, the receiver now matches |
| `last minute: N files sent ...` | normal traffic, at most one line a minute, only when something changed |
| `rescan found differences the watcher had not sent` | the 10-minute check fixed something; fine if rare |
| `... ; trying again in N s` | the connection dropped; it retries on its own |
| `receiver could not apply <path>: <reason>` | the receiver could not write that file (disk full, permissions); retried at the next rescan |

## When something is wrong

| Message | What it means / what to do |
|---|---|
| `cannot connect to <ip:port>: Connection timed out` | the port is not open to the sender: step 4 |
| `cannot connect to <ip:port>: Connection refused` | the receiver is not running: check its log |
| `handshake refused by the receiver: the key files on the two servers are not the same` | copy the key file again (step 3) and restart both |
| receiver: `wrong key (or not a wormhole sender); connection dropped` | the same, seen from the receiver; or something else knocked on the port |
| `the receiver refused: message format ...` | the two servers run different wormhole versions: install the same one on both |
| `warning: key file ... can be read by other users` | `chmod 600 <key file>` |
| `cannot listen on <ip:port>: Address already in use` | another receiver is already running: `pm2 ls` |
| `--root ...: No such file or directory` (sender) | the folder path is wrong |

## Options

| Option | Default | Meaning |
|---|---|---|
| `--root <dir>` | (required) | folder to copy from (send, verify) or into (receive; created if missing) |
| `--to <ip:port>` | (send, verify) | the receiver's address |
| `--listen <ip:port>` | (receive) | where the receiver listens |
| `--key <file>` | (required) | the shared key from `wormhole keygen` |
| `--exclude <path>` | none | a path inside `--root` to leave alone on both sides; repeatable |
| `--debounce-ms <n>` | 200 | send a file once it has been quiet this long (a file written 50 times in a row is sent once) |
| `--rescan-secs <n>` | 600 | how often the sender compares the whole folder with the receiver |

## How it works

- **Watching:** the sender uses inotify (the Linux kernel's file-change notices) on the whole
  folder, including folders created later. Only writes, creates, deletes, renames and permission
  changes count; reads are ignored, so a service reading its own files costs nothing.
- **What to send:** a notice only says where to look. The sender compares that part of the folder
  with what it knows the receiver has (BLAKE2s content hashes) and sends the difference. After a
  reconnect or the 10-minute rescan it asks the receiver for its full list and compares everything.
  So a missed or doubled notice cannot leave the copy wrong.
- **Writing:** each file is written to a temp file (`.wormhole-tmp-...`) in the same folder and
  renamed over the old one, so a program reading the copy never sees half a file. Permissions and
  modified time are kept.
- **Connection:** one TCP connection, encrypted and authenticated with the Noise protocol
  (`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`, the framework WireGuard uses) and the shared key. Each
  connection gets fresh session keys, so a key leaked later cannot decrypt recorded traffic.
  Messages over 512 bytes are LZ4-compressed when that helps. The sender pings every 20 s when idle;
  120 s of silence ends the connection and the sender reconnects.
- **Safety on the receiver:** it writes only under `--root`, refuses paths with `..` or absolute
  paths, never follows or copies symlinks, and deletes its own leftover temp files at start.

## Measured (2026-11-23, a test folder of 2,120 small JSON files)

| | |
|---|---|
| First full copy | 0.2 s |
| One changed file, write to copy | about 0.2 s (the debounce) |
| Memory | receiver 3.5 MB, sender 4.5 MB |
| Receiver CPU over a minute with changes | under 1 s |
| Tested | create, change, delete, rename, temp file + rename, folder created / deleted, file turned folder and back, permission change, 50 rapid rewrites sent once, receiver restart with changes made while it was down, wrong key refused, copy edited by hand repaired by the rescan, excluded folder left alone, symlink not copied, 10 s of heavy reads triggering nothing |

The sender gets one kernel notice per file opened (the watcher library always asks for them) and
throws it away. A test that read 730,000 files in 10 s cost it 4 s of CPU; normal reading is a tiny
fraction of that.

## Building from source

```
cargo build --release          # result: target/release/wormhole
```

Releases: bump `version` in `Cargo.toml`, then `sh tools/package.sh` (builds linux x86_64 and
aarch64 against glibc 2.17 with cargo-zigbuild and zig, packs `dist/get/wormhole/`) and
`sh tools/upload.sh` (uploads to the CDN; `--dry-run` shows what it would do). Version folders never
change once uploaded; `latest` and `v1` (the installer) always point at the newest.

## License

wormhole is licensed under the Apache License, Version 2.0: see [LICENSE](LICENSE) and
[NOTICE](NOTICE) (Copyright 2026 BROADWAYINC PTE. LTD.). The libraries compiled into the
binary keep their own licenses, listed in [THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES).

## Code

| File | What |
|---|---|
| `src/main.rs` | commands, options, key file, log lines |
| `src/send.rs` | send and verify |
| `src/receive.rs` | receive and the safe writes |
| `src/tree.rs` | folder scan, hashes, path rules |
| `src/proto.rs` | the messages and their encoding |
| `src/secure.rs` | the encrypted connection |
| `install.sh` | the installer, served as `https://cdn.broadwayinc.com/get/wormhole/v1` |
| `tools/package.sh` | builds and packs a release into `dist/` |
| `tools/upload.sh` | uploads `dist/` to the CDN |
| `tools/third_party.py` | writes `THIRD-PARTY-NOTICES` (licenses of the libraries in the binary) |
