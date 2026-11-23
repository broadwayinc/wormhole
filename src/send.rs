//! `wormhole send` and `wormhole verify` (the side that owns the files).
//!
//! The sender keeps `remote`: what it knows the receiver has (from the receiver's manifest, then
//! updated by every acknowledged batch). Every change is worked out the same way: scan a part of
//! the local folder, compare it with the same part of `remote`, send the difference. So an
//! inotify event only says WHERE to look; the folder itself is the truth, and a missed or doubled
//! event cannot leave the receiver wrong for longer than the next rescan.

use crate::proto::{self, Entry, Kind, Msg, Op};
use crate::secure::Secure;
use crate::tree::{self, Manifest};
use crate::{human_bytes, log, Opts, Res};
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::fs;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const BATCH_BYTES: usize = 4 * 1024 * 1024;
const BATCH_OPS: usize = 2000;
const PING_EVERY: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const STATS_EVERY: Duration = Duration::from_secs(60);

enum Dirty {
    Path(PathBuf),
    All,
}

#[derive(Default)]
struct Stats {
    files: u64,
    folders: u64,
    deleted: u64,
    failed: u64,
    bytes: u64,
}

impl Stats {
    fn any(&self) -> bool {
        self.files + self.folders + self.deleted + self.failed > 0
    }
    fn add(&mut self, o: &Stats) {
        self.files += o.files;
        self.folders += o.folders;
        self.deleted += o.deleted;
        self.failed += o.failed;
        self.bytes += o.bytes;
    }
    fn describe(&self) -> String {
        let mut parts = vec![format!("{} files sent ({} on the wire)", self.files, human_bytes(self.bytes))];
        if self.deleted > 0 {
            parts.push(format!("{} deleted", self.deleted));
        }
        if self.folders > 0 {
            parts.push(format!("{} folders made or updated", self.folders));
        }
        if self.failed > 0 {
            parts.push(format!("{} FAILED on the receiver (see above; retried at the next rescan)", self.failed));
        }
        parts.join(", ")
    }
}

fn root_of(o: &Opts) -> Res<PathBuf> {
    let root = o.need_root()?;
    let abs = fs::canonicalize(root).map_err(|e| format!("--root {}: {e}", root.display()))?;
    if !abs.is_dir() {
        return Err(format!("--root {} is not a folder", root.display()));
    }
    Ok(abs)
}

fn connect(to: &str, key: &[u8; 32], role: u8) -> Res<(Secure, Manifest)> {
    let addr = to
        .to_socket_addrs()
        .map_err(|e| format!("bad --to address {to}: {e}"))?
        .next()
        .ok_or_else(|| format!("bad --to address {to}"))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|e| format!("cannot connect to {to}: {e}"))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let mut s = Secure::connect(stream, key)?;
    s.send(&proto::encode(&Msg::Hello { version: proto::PROTO_VERSION, role }))?;
    let manifest = expect_manifest(&mut s)?;
    Ok((s, manifest))
}

fn expect_manifest(s: &mut Secure) -> Res<Manifest> {
    loop {
        match proto::decode(&s.recv()?)? {
            Msg::Manifest(entries) => return Ok(entries.into_iter().collect()),
            Msg::Pong => {}
            Msg::Refuse(why) => return Err(format!("the receiver refused: {why}")),
            _ => return Err("unexpected message from the receiver".into()),
        }
    }
}

// ---------------------------------------------------------------- send

pub fn run_send(o: &Opts) -> Res<ExitCode> {
    let root = root_of(o)?;
    let key = o.need_key()?;
    let to = o.to.clone().ok_or("--to is required")?;

    let (tx, rx) = mpsc::channel::<Dirty>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
        Ok(ev) => {
            if ev.need_rescan() {
                let _ = tx.send(Dirty::All);
                return;
            }
            // Opening or reading a file changes nothing; the MCP server reads its states all the time.
            let relevant = match ev.kind {
                EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
                EventKind::Access(_) => false,
                _ => true,
            };
            if relevant {
                for p in ev.paths {
                    let _ = tx.send(Dirty::Path(p));
                }
            }
        }
        Err(e) => {
            log(&format!("file watcher error: {e}; rescanning everything"));
            let _ = tx.send(Dirty::All);
        }
    })
    .map_err(|e| format!("cannot start the file watcher: {e}"))?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| format!("cannot watch {}: {e}", root.display()))?;

    log(&format!(
        "wormhole {} sending {} to {to}{}",
        crate::VERSION,
        root.display(),
        if o.excludes.is_empty() { String::new() } else { format!(" (leaving alone: {})", o.excludes.join(", ")) }
    ));

    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let err = match session(&root, &key, &to, o, &rx) {
            Ok(()) => unreachable!(),
            Err(e) => e,
        };
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        log(&format!("{err}; trying again in {} s", backoff.as_secs()));
        // Events while disconnected are not needed: the next connection compares everything.
        let until = Instant::now() + backoff;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if let Err(RecvTimeoutError::Disconnected) = rx.recv_timeout(left) {
                return Err("the file watcher stopped".into());
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// One connection's life. Only returns with the reason it ended.
fn session(root: &Path, key: &[u8; 32], to: &str, o: &Opts, rx: &Receiver<Dirty>) -> Res<()> {
    // Anything queued so far is covered by the full comparison below.
    while rx.try_recv().is_ok() {}
    let (mut s, mut remote) = connect(to, key, proto::ROLE_SYNC)?;
    log(&format!("connected to {to}; the receiver has {} files and folders", remote.len()));

    let mut seq = 0u64;
    let t = Instant::now();
    let st = sync_scope(&mut s, &mut seq, &mut remote, root, "", &o.excludes)?;
    if st.any() {
        log(&format!("catch-up done in {:.1} s: {}", t.elapsed().as_secs_f64(), st.describe()));
    } else {
        log(&format!("already in sync ({} files and folders)", remote.len()));
    }

    let debounce = Duration::from_millis(o.debounce_ms);
    let max_wait = (debounce * 5).max(Duration::from_secs(1));
    let rescan_every = Duration::from_secs(o.rescan_secs);
    let mut dirty: BTreeSet<String> = BTreeSet::new();
    let mut all = false;
    let mut first_event: Option<Instant> = None;
    let mut last_event = Instant::now();
    let mut last_rescan = Instant::now();
    let mut last_sent = Instant::now();
    let mut last_stats = Instant::now();
    let mut minute = Stats::default();

    loop {
        let now = Instant::now();
        let mut wait = (last_sent + PING_EVERY).saturating_duration_since(now);
        wait = wait.min((last_rescan + rescan_every).saturating_duration_since(now));
        if let Some(first) = first_event {
            wait = wait
                .min((last_event + debounce).saturating_duration_since(now))
                .min((first + max_wait).saturating_duration_since(now));
        }
        match rx.recv_timeout(wait) {
            Ok(Dirty::Path(p)) => {
                match tree::rel_of(root, &p) {
                    Some(rel) if rel.is_empty() => all = true,
                    Some(rel) if !tree::excluded(&rel, &o.excludes) && !tree::is_tmp(&rel) => {
                        dirty.insert(rel);
                    }
                    _ => {}
                }
                last_event = Instant::now();
                first_event.get_or_insert(last_event);
            }
            Ok(Dirty::All) => {
                all = true;
                last_event = Instant::now();
                first_event.get_or_insert(last_event);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("the file watcher stopped".into()),
        }

        let now = Instant::now();
        if let Some(first) = first_event {
            if now >= last_event + debounce || now >= first + max_wait {
                let st = if all {
                    sync_scope(&mut s, &mut seq, &mut remote, root, "", &o.excludes)?
                } else {
                    let mut st = Stats::default();
                    let mut done: Vec<&String> = Vec::new();
                    for scope in &dirty {
                        // "a/b" is already covered when "a" was just compared.
                        if done.iter().any(|d| tree::under(scope, d)) {
                            continue;
                        }
                        st.add(&sync_scope(&mut s, &mut seq, &mut remote, root, scope, &o.excludes)?);
                        done.push(scope);
                    }
                    st
                };
                if st.any() {
                    last_sent = Instant::now();
                }
                minute.add(&st);
                dirty.clear();
                all = false;
                first_event = None;
            }
        }

        if now >= last_rescan + rescan_every {
            s.send(&proto::encode(&Msg::ManifestReq))?;
            remote = expect_manifest(&mut s)?;
            let st = sync_scope(&mut s, &mut seq, &mut remote, root, "", &o.excludes)?;
            if st.any() {
                log(&format!("rescan found differences the watcher had not sent: {}", st.describe()));
            }
            last_rescan = Instant::now();
            last_sent = last_rescan;
        }

        if Instant::now() >= last_sent + PING_EVERY {
            s.send(&proto::encode(&Msg::Ping))?;
            loop {
                match proto::decode(&s.recv()?)? {
                    Msg::Pong => break,
                    Msg::Refuse(why) => return Err(format!("the receiver refused: {why}")),
                    _ => return Err("unexpected message from the receiver".into()),
                }
            }
            last_sent = Instant::now();
        }

        if last_stats.elapsed() >= STATS_EVERY {
            if minute.any() {
                log(&format!("last minute: {}", minute.describe()));
            }
            minute = Stats::default();
            last_stats = Instant::now();
        }
    }
}

/// What the receiver will hold for one op once it is acknowledged.
enum After {
    File(Entry),
    Dir(u32),
    Gone,
}

/// Makes the receiver's copy of `scope` ("" = everything) equal to the local folder.
fn sync_scope(
    s: &mut Secure,
    seq: &mut u64,
    remote: &mut Manifest,
    root: &Path,
    scope: &str,
    excludes: &[String],
) -> Res<Stats> {
    let local = tree::scan(root, scope, excludes)?;
    let remote_here: Vec<(&String, &Entry)> = remote
        .range(scope.to_string()..)
        .take_while(|(p, _)| p.starts_with(scope))
        .filter(|(p, _)| tree::under(p, scope) && !tree::excluded(p, excludes))
        .collect();

    let mut dels: Vec<String> = Vec::new();
    for (p, _) in &remote_here {
        if !local.contains_key(*p) && !dels.iter().any(|d| tree::under(p, d)) {
            dels.push((*p).clone());
        }
    }
    let mut dirs: Vec<(String, u32)> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for (p, e) in &local {
        if remote.get(p) == Some(e) {
            continue;
        }
        match e.kind {
            Kind::Dir => dirs.push((p.clone(), e.mode)),
            Kind::File => files.push(p.clone()),
        }
    }

    let mut st = Stats::default();
    let mut ops: Vec<Op> = Vec::new();
    let mut after: Vec<After> = Vec::new();
    let mut size = 0usize;
    for path in dels {
        ops.push(Op::Del { path });
        after.push(After::Gone);
    }
    for (path, mode) in dirs {
        ops.push(Op::Mkdir { path, mode });
        after.push(After::Dir(mode));
    }
    flush_if_full(s, seq, remote, &mut ops, &mut after, &mut size, &mut st)?;
    for path in files {
        let abs = root.join(&path);
        let Ok(meta) = fs::symlink_metadata(&abs) else { continue };
        if !meta.is_file() {
            continue;
        }
        let data = match fs::read(&abs) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot read {}: {e}", abs.display())),
        };
        let mode = tree::mode_of(&meta);
        let hash = tree::hash_bytes(&data);
        size += data.len();
        ops.push(Op::Put { path, mode, mtime: (meta.mtime(), meta.mtime_nsec() as u32), data });
        after.push(After::File(Entry { kind: Kind::File, mode, hash }));
        flush_if_full(s, seq, remote, &mut ops, &mut after, &mut size, &mut st)?;
    }
    if !ops.is_empty() {
        send_batch(s, seq, remote, std::mem::take(&mut ops), std::mem::take(&mut after), &mut st)?;
    }
    Ok(st)
}

fn flush_if_full(
    s: &mut Secure,
    seq: &mut u64,
    remote: &mut Manifest,
    ops: &mut Vec<Op>,
    after: &mut Vec<After>,
    size: &mut usize,
    st: &mut Stats,
) -> Res<()> {
    if *size >= BATCH_BYTES || ops.len() >= BATCH_OPS {
        send_batch(s, seq, remote, std::mem::take(ops), std::mem::take(after), st)?;
        *size = 0;
    }
    Ok(())
}

/// Sends one batch, waits for the receiver to confirm it, and records the result in `remote`.
fn send_batch(
    s: &mut Secure,
    seq: &mut u64,
    remote: &mut Manifest,
    ops: Vec<Op>,
    after: Vec<After>,
    st: &mut Stats,
) -> Res<()> {
    *seq += 1;
    let sent_before = s.bytes_sent;
    let paths: Vec<String> = ops.iter().map(|op| op.path().to_string()).collect();
    s.send(&proto::encode(&Msg::Batch { seq: *seq, ops }))?;
    let errors = loop {
        match proto::decode(&s.recv()?)? {
            Msg::Ack { seq: got, errors } if got == *seq => break errors,
            Msg::Pong => {}
            Msg::Refuse(why) => return Err(format!("the receiver refused: {why}")),
            _ => return Err("unexpected message from the receiver".into()),
        }
    };
    let failed: BTreeSet<String> = errors.iter().map(|(p, _)| p.clone()).collect();
    for (p, why) in &errors {
        log(&format!("receiver could not apply {p}: {why}"));
    }
    for (path, a) in paths.into_iter().zip(after) {
        // A written file or a delete replaces whatever was at or under this path on the receiver
        // (a folder update keeps the folder's contents).
        if !matches!(a, After::Dir(_)) || failed.contains(&path) {
            let stale: Vec<String> = remote
                .range(path.clone()..)
                .take_while(|(p, _)| p.starts_with(&path))
                .filter(|(p, _)| tree::under(p, &path))
                .map(|(p, _)| p.clone())
                .collect();
            for p in stale {
                remote.remove(&p);
            }
        }
        if failed.contains(&path) {
            st.failed += 1;
            continue;
        }
        match a {
            After::File(e) => {
                st.files += 1;
                remote.insert(path, e);
            }
            After::Dir(mode) => {
                st.folders += 1;
                remote.insert(path, Entry { kind: Kind::Dir, mode, hash: [0; 32] });
            }
            After::Gone => st.deleted += 1,
        }
    }
    st.bytes += s.bytes_sent - sent_before;
    Ok(())
}

// ---------------------------------------------------------------- verify

pub fn run_verify(o: &Opts) -> Res<ExitCode> {
    let root = root_of(o)?;
    let key = o.need_key()?;
    let to = o.to.clone().ok_or("--to is required")?;
    let local = tree::scan(&root, "", &o.excludes)?;
    let (_s, remote) = connect(&to, &key, proto::ROLE_VERIFY)?;
    let remote: Manifest = remote.into_iter().filter(|(p, _)| !tree::excluded(p, &o.excludes)).collect();

    let mut diffs: Vec<String> = Vec::new();
    for (p, e) in &local {
        match remote.get(p) {
            None => diffs.push(format!("  missing on receiver:  {p}")),
            Some(r) if r.kind != e.kind => diffs.push(format!("  file vs folder:       {p}")),
            Some(r) if r.hash != e.hash => diffs.push(format!("  different content:    {p}")),
            Some(r) if r.mode != e.mode => {
                diffs.push(format!("  different permission: {p} ({:o} here, {:o} there)", e.mode, r.mode))
            }
            _ => {}
        }
    }
    for p in remote.keys() {
        if !local.contains_key(p) {
            diffs.push(format!("  only on receiver:     {p}"));
        }
    }

    let count = |m: &Manifest| {
        let files = m.values().filter(|e| e.kind == Kind::File).count();
        (files, m.len() - files)
    };
    let (lf, ld) = count(&local);
    let (rf, rd) = count(&remote);
    println!("here     {}: {lf} files, {ld} folders", root.display());
    println!("receiver {to}: {rf} files, {rd} folders");
    if diffs.is_empty() {
        println!("IN SYNC: every file has the same content and permissions on both sides.");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("NOT IN SYNC: {} differences", diffs.len());
        for d in &diffs {
            println!("{d}");
        }
        println!("(While the sender is running, files written in the last second can show up here; run it again.)");
        Ok(ExitCode::from(1))
    }
}
