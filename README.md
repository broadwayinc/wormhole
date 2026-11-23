# wormhole

Keeps a live copy of a folder on another Linux server. One program, run on both servers:

- **sender** (the server that owns the files) watches the folder and sends every change, usually
  within 0.2 s;
- **receiver** (the server that gets the copy) writes each change into its copy.

It copies one way only: the receiver's copy becomes exactly the sender's folder (new, changed and
deleted files, folders, permissions). It is built to move the MCP server's `states/` folder from
mcp.broadwayinc.computer to the mcp.skapi.com server so users stay signed in. See
[PLAN.md](PLAN.md) for the reasons behind the migration steps.

```
wormhole keygen  <key file>
wormhole receive --root <dir> --listen <ip:port> --key <key file>
wormhole send    --root <dir> --to <ip:port> --key <key file> [--exclude <path>]...
wormhole verify  --root <dir> --to <ip:port> --key <key file> [--exclude <path>]...
```

## The two servers

|  | Sender: this box | Receiver: the "scripts" box |
|---|---|---|
| Serves today | mcp.broadwayinc.computer | mcp.skapi.com |
| Instance | i-067417c5067d94007 (r5.xlarge) | i-010ef8bebb1e7c725 (t3.small) |
| Private IP | 172.31.73.239 | 172.31.38.0 |
| Security group | sg-07922b75909422ffb (dev:git-server) | sg-08b2c07363aad45ef (internal) |
| Folder | /home/ubuntu/skapi/dev/mcp/states | `<mcp folder on that box>`/states |
| Program + key | /home/ubuntu/wormhole/ | /home/ubuntu/wormhole/ |

The two talk over the private network (same VPC), not through the load balancer.

## Setup

### 1. Build (this box)

```
cd /home/ubuntu/skapi/dev/wormhole
PATH=~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH cargo build --release
mkdir -p ~/wormhole && cp target/release/wormhole ~/wormhole/
~/wormhole/wormhole --version
```

The result is a single 900 KB file with nothing to install. The same file runs on both servers
(both are x86_64 Ubuntu: 22.04 here, 24.04 there).

### 2. Make the shared key (this box)

```
~/wormhole/wormhole keygen ~/wormhole/wormhole.key
```

Both servers need this same file. Anyone who has it can write into the receiver's folder, so it is
created readable by you only (mode 600), and it is never sent over the connection. Keep it out of git.

### 3. Copy the program and the key to the receiver

This box needs to be able to log in to the receiver once. Print this box's public key:

```
cat ~/.ssh/id_ed25519.pub
```

On the receiver (log in with your `script-server-virginia` .pem), add that line:

```
echo "<the line printed above>" >> ~/.ssh/authorized_keys
```

Back on this box (`-p` keeps the key file at mode 600):

```
scp -p -r ~/wormhole ubuntu@172.31.38.0:~/
ssh ubuntu@172.31.38.0 'ls -l ~/wormhole && ~/wormhole/wormhole --version'
```

### 4. Open port 7700 on the receiver, for this box only

Run this from anywhere the AWS CLI works (for example this box):

```
aws ec2 authorize-security-group-ingress --region us-east-1 \
  --group-id sg-08b2c07363aad45ef \
  --ip-permissions 'IpProtocol=tcp,FromPort=7700,ToPort=7700,UserIdGroupPairs=[{GroupId=sg-07922b75909422ffb,Description="wormhole from the mcp.broadwayinc.computer box"}]'
```

This lets in only servers in this box's security group. The "internal" group is shared with other
servers, but nothing else listens on 7700. The key is a second lock: a connection without the right
key is dropped at the first message.

### 5. Start the receiver (receiver box)

First stop the MCP there. The copy replaces its `states/`, and a running MCP would not see the new
files anyway (it reads sign-ins into memory when it starts). Keep a backup of what is there now:

```
pm2 stop MCPSERVER_SKAPI BQ_CLOUD_IMPORT_SKAPI
MCP_DIR=/home/ubuntu/<mcp folder>          # the folder that has ecosystem.skapi.config.js
cp -a $MCP_DIR/states ~/states-backup-$(date +%F)
```

Then:

```
pm2 start ~/wormhole/wormhole --name WORMHOLE_RECV --interpreter none -- \
  receive --root $MCP_DIR/states --listen 172.31.38.0:7700 --key /home/ubuntu/wormhole/wormhole.key
pm2 save
pm2 logs WORMHOLE_RECV --lines 5 --nostream
```

You should see `receiving into .../states, listening on 172.31.38.0:7700`.

### 6. Start the sender (this box)

```
pm2 start ~/wormhole/wormhole --name WORMHOLE_SEND --interpreter none -- \
  send --root /home/ubuntu/skapi/dev/mcp/states --to 172.31.38.0:7700 \
  --key /home/ubuntu/wormhole/wormhole.key --exclude cloud-import/tmp
pm2 save
pm2 logs WORMHOLE_SEND --lines 5 --nostream
```

You should see `connected to 172.31.38.0:7700` and then `catch-up done in ... s`. The first copy of
the whole folder (about 2,200 files, 10 MB) takes about a second.

`--exclude cloud-import/tmp` skips the import worker's scratch files. Excluded paths are left
alone on both sides: not sent, and not deleted on the receiver.

### 7. Check that both sides are equal (this box)

```
~/wormhole/wormhole verify --root /home/ubuntu/skapi/dev/mcp/states --to 172.31.38.0:7700 \
  --key ~/wormhole/wormhole.key --exclude cloud-import/tmp
```

It prints `IN SYNC` (exit code 0) or every difference (exit code 1). While the MCP is running, a
file written in the last fraction of a second can show up as a difference: run it again.

## Running day to day

Both run under pm2, so they come back after a crash or a reboot (`pm2 save` done above). If the
receiver restarts or the network drops, the sender retries (1 s, then up to every 30 s) and on
reconnect compares everything, so nothing is lost while they are apart. Every 10 minutes the sender
also compares the whole folder with the receiver (`--rescan-secs`), in case a change was missed.

What the log lines mean:

| Line | Meaning |
|---|---|
| `connected to ...; the receiver has N files and folders` | connection made; the comparison follows |
| `catch-up done in X s: ...` / `already in sync` | after (re)connecting, the receiver now matches |
| `last minute: N files sent ...` | normal traffic, at most one line a minute, only when something changed |
| `rescan found differences the watcher had not sent` | the 10-minute check fixed something; fine if rare |
| `... ; trying again in N s` | the connection dropped; it retries on its own |
| `receiver could not apply <path>: <reason>` | the receiver could not write that file (disk full, permissions); retried at the next rescan |

## Cutover day (the move itself)

Before this day, make sure sign-ins will work on the new server (PLAN.md section 5, Phase 0): the
dashboards must allow `https://mcp.skapi.com` as a sign-in callback, or the receiver's MCP is started
without `MCP_HOST` at first.

1. This box: stop the MCP. From here on it changes nothing in `states/`.
   ```
   pm2 stop MCPSERVER BQ_CLOUD_IMPORT
   ```
2. This box: check (repeat until it says `IN SYNC`; the last changes arrive within a second):
   ```
   ~/wormhole/wormhole verify --root /home/ubuntu/skapi/dev/mcp/states --to 172.31.38.0:7700 --key ~/wormhole/wormhole.key --exclude cloud-import/tmp
   ```
3. Receiver: start the MCP (it reads `states/` into memory now):
   ```
   pm2 start MCPSERVER_SKAPI BQ_CLOUD_IMPORT_SKAPI
   ```
   (or `pm2 start ecosystem.skapi.config.js` from the mcp folder if they were never started there)
4. Point mcp.broadwayinc.computer (rule 2) at the receiver's target group `mcp`:
   ```
   aws elbv2 modify-rule --region us-east-1 \
     --rule-arn arn:aws:elasticloadbalancing:us-east-1:136564153953:listener-rule/app/ubuntunvirginia/c64c4e722c3f65f6/d4ba29cde537a7a3/76f9fd4db49cd4f3 \
     --actions Type=forward,TargetGroupArn=arn:aws:elasticloadbalancing:us-east-1:136564153953:targetgroup/mcp/dbf49a533a39ae9d
   ```
   If you had pointed mcp.skapi.com (rule 1) at this box earlier (PLAN.md Phase 1b), point it back
   to `mcp` the same way with rule 1's ARN:
   `arn:aws:elasticloadbalancing:us-east-1:136564153953:listener-rule/app/ubuntunvirginia/c64c4e722c3f65f6/d4ba29cde537a7a3/4d63c262b9eac425`
5. Check the receiver's MCP log: existing users' calls work, no `Session file not found`.
6. When you are happy: stop wormhole on both sides and close the port.
   ```
   pm2 delete WORMHOLE_SEND && pm2 save                          # this box
   pm2 delete WORMHOLE_RECV && pm2 save                          # receiver
   aws ec2 revoke-security-group-ingress --region us-east-1 --group-id sg-08b2c07363aad45ef \
     --ip-permissions 'IpProtocol=tcp,FromPort=7700,ToPort=7700,UserIdGroupPairs=[{GroupId=sg-07922b75909422ffb}]'
   ```

Between steps 1 and 4 requests to mcp.broadwayinc.computer fail for a few seconds; MCP clients retry.
Never run both MCPs with live traffic at the same time: they would each change their own copy.

### Going back (rollback)

Run wormhole the other way so that whatever the new server wrote comes back:

1. Receiver box: `pm2 stop MCPSERVER_SKAPI BQ_CLOUD_IMPORT_SKAPI`.
2. Open port 7700 on THIS box's group (sg-07922b75909422ffb) for sg-08b2c07363aad45ef (step 4 with
   the two group ids swapped).
3. This box: `~/wormhole/wormhole receive --root /home/ubuntu/skapi/dev/mcp/states --listen 172.31.73.239:7700 --key ~/wormhole/wormhole.key`
4. Receiver box: `~/wormhole/wormhole send --root $MCP_DIR/states --to 172.31.73.239:7700 --key ~/wormhole/wormhole.key --exclude cloud-import/tmp`,
   then `verify` with the same arguments until `IN SYNC`, then stop both (Ctrl+C).
5. This box: `pm2 start MCPSERVER BQ_CLOUD_IMPORT`, then rule 2 back to target group
   `skapi-mcp-8000` (`arn:aws:elasticloadbalancing:us-east-1:136564153953:targetgroup/skapi-mcp-8000/aeda7af23aba32a5`).

## When something is wrong

| Message | What it means / what to do |
|---|---|
| `cannot connect to 172.31.38.0:7700: Connection timed out` | port 7700 is not open to this box: step 4 |
| `cannot connect to 172.31.38.0:7700: Connection refused` | the receiver is not running: `pm2 logs WORMHOLE_RECV` on the receiver |
| `handshake refused by the receiver: the key files on the two servers are not the same` | copy the key file again (step 3) and restart both |
| receiver: `wrong key (or not a wormhole sender); connection dropped` | the same, seen from the receiver; or something else knocked on the port |
| `the receiver refused: message format ...` | the two servers run different wormhole builds: copy the same file to both |
| `warning: key file ... can be read by other users` | `chmod 600 <key file>` |
| `cannot listen on 172.31.38.0:7700: Address already in use` | another receiver is already running: `pm2 ls` |
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
  changes count; reads are ignored, so the MCP reading its own states costs nothing.
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

## Measured (this box, 2026-11-23, a 2,120-file test folder shaped like states/)

| | |
|---|---|
| First full copy | 0.2 s |
| One changed file, write to copy | about 0.2 s (the debounce) |
| Memory | receiver 3.5 MB, sender 4.5 MB |
| Receiver CPU over a minute with changes | under 1 s |
| Tested | create, change, delete, rename, temp file + rename, folder created / deleted, file turned folder and back, permission change, 50 rapid rewrites sent once, receiver restart with changes made while it was down, wrong key refused, copy edited by hand repaired by the rescan, excluded folder left alone, symlink not copied, 10 s of heavy reads triggering nothing |

The sender gets one kernel notice per file opened (the watcher library always asks for them) and
throws it away. A test that read 730,000 files in 10 s cost it 4 s of CPU; the MCP's real reading
is a tiny fraction of that, and the sender runs on the large box.

## Code

| File | What |
|---|---|
| `src/main.rs` | commands, options, key file, log lines |
| `src/send.rs` | send and verify |
| `src/receive.rs` | receive and the safe writes |
| `src/tree.rs` | folder scan, hashes, path rules |
| `src/proto.rs` | the messages and their encoding |
| `src/secure.rs` | the encrypted connection |
