# Wormhole: plan

Goal: move the prod MCP from this box (i-067417c5067d94007, r5.xlarge, 172.31.73.239) to the
"scripts" box (i-010ef8bebb1e7c725, t3.small, 172.31.38.0) so that no user has to sign in again
or change anything.

Status: wormhole BUILT 2026-11-23 (Rust, see README.md for setup); owner chose wormhole over rsync.
Nothing installed on the servers yet.

## 1. What has to travel

Everything is in `mcp/states/` (9.8 MB, about 2,200 small JSON files, checked 2026-11-23):

| Folder / file | Files | What breaks if it is missing |
|---|---|---|
| registeredClients/ | 1,532 (6.2 MB) | every connector (claude.ai, ChatGPT, Cursor...) gets "invalid_client" |
| sessions/ | 178 | users' Cognito tokens: "Session file not found", indexing fails |
| localStorage/ | 389 | cached admin sessions, rebuilt slowly with extra logins |
| accessTokens/ | 20 | skmcp_ tokens (Claude Code etc.) stop working |
| cloud-import/ (+ its `secret`) | 5 | Drive / Dropbox / OneDrive connections; they are encrypted with `cloud-import/secret`, so that key MUST travel too |
| downloads/ + download_token_secret | 60 | download links already handed out stop working |
| authCodes/, stateStorage/, host-uploads/ | 0 now | short-lived; mirrored anyway |

Not needed: `logs/` (stays here), `states-dev/` (dev stays on this box), code (from git).

## 2. Facts that shape the plan

1. `oauth.js`, `access-tokens.js`, `auth-codes.js` read their folders into memory ONCE at
   startup. A server that is already running does not see files that arrive later. So the new
   box's server must be started (or restarted) AFTER the last sync.
2. The OAuth issuer and discovery documents are built from the request's Host header
   (`oauth.js:140`). One server with one states folder answers correctly for BOTH hostnames.
3. The sign-in callback is `MCP_HOST + /oauth/callback` (`environment.js:34`). The dashboards'
   `isAllowedOAuthRedirect` (web-skapi + web-bunnyquery `src/oauth.ts`) only allow
   mcp.broadwayinc.computer today.
4. Both hostnames are rules on the SAME ALB `ubuntunvirginia`:
   rule 1 `mcp.skapi.com` -> TG `mcp` (new box :8000),
   rule 2 `mcp.broadwayinc.computer` -> TG `skapi-mcp-8000` (this box).
   Same VPC, so the boxes can talk over private IPs.
5. Prod memory today: MCPSERVER 124 MB + BQ_CLOUD_IMPORT 75 MB. Fits a t3.small (2 GB).

## 3. The two rules that make it invisible

**Users never change address.** MCP clients store the URL the user typed and do not follow a
move. So mcp.broadwayinc.computer must keep answering, from the new box, with the same states.
mcp.skapi.com is the address for new users and docs. Both hostnames point at one box and one
states folder, for as long as old connectors exist.

**Only one writer at a time.** The server rewrites state all the time (Cognito refresh writes
sessions, grant_liveness, new tokens) and caches it in memory. Two live servers on two copies
would drift apart and lose sign-ins however you merge them. So wormhole is a ONE-WAY mirror
(old -> new), and cutover moves the single writer. No two-way sync, no conflict handling.

## 4. Wormhole design

### Language: Rust

- Toolchain + cargo-zigbuild are already on this box: one static musl binary, `scp` it to the
  new box, nothing to install there.
- About 3-5 MB RAM idle. Node would also work (recursive fs.watch on Node 22) but costs about
  60 MB per process, which matters on a 2 GB box. Go is equally good but not installed.
  Python is slowest and needs an extra library.

### Why not just rsync?

`rsync` over SSH in a loop works too, and is the zero-code fallback (10 MB tree, delta in under
a second). Wormhole adds: changes arrive in under a second instead of on a timer, no SSH key
between the boxes, a `verify` command that proves both sides are equal before cutover, and it
can be reused for later moves. If you prefer zero code, the runbook below works with rsync
instead of wormhole unchanged.

### Commands (one binary)

```
wormhole send    --root /home/ubuntu/skapi/dev/mcp/states --to 172.31.38.0:7700 --key wormhole.key
wormhole receive --root <mcp checkout on new box>/states  --listen 172.31.38.0:7700 --key wormhole.key
wormhole verify  (either side: file count + one hash over every path/size/sha256; "in sync" or the differing paths)
```

Run both under pm2 (`WORMHOLE_SEND` here, `WORMHOLE_RECV` there) so they come back after a reboot.

### Network and security

- One long-lived TCP connection over the VPC private IP, port 7700. Open it in the new box's
  security group to this box's private IP only. Not through the ALB, never public.
- The content is live tokens, so it is encrypted and authenticated: Noise protocol
  (Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s, as in WireGuard) with a shared 32-byte key file
  (mode 0600) on both boxes. A peer without the key is dropped at the first message. (Changed
  from the first draft's TLS + pinned certificate: same protection, no certificates to manage.)

### How it syncs

1. **Connect:** the receiver sends its manifest (path, size, sha256 of every file; about 50 ms
   for 2,200 files).
2. **Catch up:** the sender compares that with its own tree and sends PUT for missing or
   different files and DEL for files that exist only on the receiver (exact mirror).
3. **Live:** inotify on the whole tree (write finished, moved in, deleted, moved out, new
   folder). Changes to the same file within 200 ms are merged (a file written 5 times is sent
   once), then sent together as one batch. Each item is `PUT(path, mode, mtime, bytes)`,
   `MKDIR(path, mode)` or `DEL(path)`; batches are LZ4-compressed (cheaper CPU than zstd).
4. **Write:** the receiver writes a temp file in the same folder, then renames it
   over the old one (atomic: a reader never sees half a file), keeps the file mode (0600
   secrets stay 0600), and acknowledges the batch. No fsync per file (that took 6 s for 2,000
   files); after a crash the next full compare resends whatever the disk lost.
5. **Reconnect:** after a dropped connection, retry with backoff and go back to step 1. A full
   compare is cheap at this size, so there is no journal to get wrong. The same full compare
   also runs every 10 minutes in case inotify misses an event.

Skipped: `cloud-import/tmp/` (worker scratch space) and temp files from writes still in progress.
The receiver refuses absolute paths, `..` and symlinks, and writes only under `--root`.

## 5. Migration runbook

### Phase 0: prep (no user impact)

- New box: same mcp commit checked out, `npm i`, states at the default `states/`.
- Sign-in callback: either (a) deploy the dashboards with `https://mcp.skapi.com` allowed in
  `isAllowedOAuthRedirect` (web-skapi + web-bunnyquery) BEFORE the new box runs with
  `MCP_HOST=https://mcp.skapi.com`, or (b) start the new box WITHOUT `MCP_HOST` at first (the
  callback stays on mcp.broadwayinc.computer, which by then also routes to the new box) and
  switch `MCP_HOST` later. Without one of these, every new sign-in fails at the callback.
- Stop `MCPSERVER_SKAPI` + `BQ_CLOUD_IMPORT_SKAPI` on the new box. The mirror replaces its
  states, so any sign-in made on mcp.skapi.com so far is dropped. Check first that nobody uses it.
- Create `wormhole.key`, copy it to both boxes (0600), open port 7700 as above.

### Phase 1: mirror (a day or more, no user impact)

- Start the receiver, then the sender. `wormhole verify` must say "in sync".
- Leave it running through a day of normal traffic and verify again.

### Phase 1b (recommended): change the domain first

Point ALB rule 1 (`mcp.skapi.com`) at TG `skapi-mcp-8000` (THIS box). Both domains are now
served by one server with one states folder: mcp.skapi.com is fully usable and can be announced,
and old connectors keep working. The box move then becomes a separate step that changes nothing
users can see.

### Phase 2: cutover (a few seconds of failed requests)

1. This box: `pm2 stop MCPSERVER BQ_CLOUD_IMPORT` (graceful, up to 10 s for in-flight calls).
   From now on this box writes nothing.
2. `wormhole verify` -> "in sync" (the last changes arrive within about 1 s).
3. New box: `pm2 start ecosystem.skapi.config.js` (loads states into memory at start).
4. When TG `mcp` is healthy: ALB rule 2 (`mcp.broadwayinc.computer`), and rule 1 if you did
   Phase 1b, -> TG `mcp`.
5. Stop the wormhole sender. Check the new box's log: existing users' calls work, no
   "Session file not found".

Between steps 1 and 4 the ALB still sends to the stopped box, so requests in that window fail.
MCP clients retry and reconnect. Never run both servers with live traffic at once.

### Rollback

Stop the new server, run wormhole the other way (new -> old) so anything written since cutover
comes back, start the old server, point the rules back at `skapi-mcp-8000`.

### After

- Keep rule 2 (`mcp.broadwayinc.computer`) for as long as old connectors call it (the access
  log's Host header shows when traffic on the old name stops).
- The dev MCP (`MCPSERVER_DEV` :8080, `states-dev`) stays on this box in this plan.

## 6. Open points

1. ~~Wormhole (Rust) or plain rsync?~~ Wormhole (2026-11-23).
2. Phase 1b (domain first) yes/no?
3. Callback: dashboards allow mcp.skapi.com first (a), or new box without MCP_HOST first (b)?
4. Has anyone signed in on mcp.skapi.com yet (their sign-ins are dropped in Phase 0)?
5. Google / Dropbox / OneDrive import sign-in: if their consoles list a redirect address on
   mcp.broadwayinc.computer, mcp.skapi.com needs adding too. Not found in `cloud-import/*.js`;
   not checked further.
