//! `wormhole receive` (the side that holds the copy).
//!
//! Listens for the sender, answers with what it has (the manifest), and applies the sender's
//! changes. A file is written as a temp file in the same folder, then renamed over the old one, so a program reading the folder never sees half a file. Nothing outside
//! --root is ever touched: paths are checked, symlinks are never followed.

use crate::proto::{self, Msg, Op};
use crate::secure::Secure;
use crate::tree::{self, TMP_PREFIX};
use crate::{log, Opts, Res};
use std::fs;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn run(o: &Opts) -> Res<ExitCode> {
    let root_arg = o.need_root()?;
    fs::create_dir_all(root_arg).map_err(|e| format!("--root {}: {e}", root_arg.display()))?;
    let root = fs::canonicalize(root_arg).map_err(|e| format!("--root {}: {e}", root_arg.display()))?;
    let key = o.need_key()?;
    let listen = o.listen.clone().ok_or("--listen is required")?;
    let removed = remove_leftover_tmp(&root);
    if removed > 0 {
        log(&format!("removed {removed} half-written temp files left by an earlier run"));
    }
    let listener = TcpListener::bind(&listen).map_err(|e| format!("cannot listen on {listen}: {e}"))?;
    log(&format!("wormhole {} receiving into {}, listening on {listen}", crate::VERSION, root.display()));

    // One change at a time, even if two connections are open (an old one not yet timed out).
    let lock = Arc::new(Mutex::new(()));
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let root = root.clone();
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into());
                    if let Err(e) = handle(stream, &peer, &root, &key, &lock) {
                        log(&format!("{peer}: {e}"));
                    }
                });
            }
            Err(e) => log(&format!("accept failed: {e}")),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn handle(stream: TcpStream, peer: &str, root: &Path, key: &[u8; 32], lock: &Mutex<()>) -> Res<()> {
    let ctl = stream.try_clone().map_err(|e| e.to_string())?;
    let _ = ctl.set_nodelay(true);
    let _ = ctl.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = ctl.set_write_timeout(Some(Duration::from_secs(60)));
    let mut s = Secure::accept(stream, key)?;
    // The sender pings every 20 s when idle, so 120 s of silence means it is gone.
    let _ = ctl.set_read_timeout(Some(Duration::from_secs(120)));

    let role = match proto::decode(&s.recv()?)? {
        Msg::Hello { version, role } if version == proto::PROTO_VERSION => role,
        Msg::Hello { version, .. } => {
            let why = format!(
                "message format {version} from the sender, {} here; run the same wormhole version on both servers",
                proto::PROTO_VERSION
            );
            let _ = s.send(&proto::encode(&Msg::Refuse(why.clone())));
            return Err(why);
        }
        _ => return Err("unexpected first message".into()),
    };
    let manifest = |lock: &Mutex<()>| -> Res<Msg> {
        let _g = lock.lock().unwrap_or_else(|p| p.into_inner());
        Ok(Msg::Manifest(tree::scan(root, "", &[])?.into_iter().collect()))
    };

    if role == proto::ROLE_VERIFY {
        s.send(&proto::encode(&manifest(lock)?))?;
        log(&format!("{peer}: answered a verify"));
        return Ok(());
    }
    if role != proto::ROLE_SYNC {
        return Err("unknown role".into());
    }

    log(&format!("sender connected from {peer}"));
    s.send(&proto::encode(&manifest(lock)?))?;
    let (mut written, mut folders, mut deleted, mut failed) = (0u64, 0u64, 0u64, 0u64);
    let mut last_stats = Instant::now();
    loop {
        let raw = match s.recv() {
            Ok(raw) => raw,
            Err(e) => {
                if written + folders + deleted + failed > 0 {
                    log(&format!("since last report: {written} files written, {folders} folders, {deleted} deleted, {failed} failed"));
                }
                return Err(format!("sender disconnected ({e})"));
            }
        };
        match proto::decode(&raw)? {
            Msg::ManifestReq => s.send(&proto::encode(&manifest(lock)?))?,
            Msg::Ping => s.send(&proto::encode(&Msg::Pong))?,
            Msg::Batch { seq, ops } => {
                let mut errors = Vec::new();
                {
                    let _g = lock.lock().unwrap_or_else(|p| p.into_inner());
                    for op in &ops {
                        match apply(root, op) {
                            Ok(()) => match op {
                                Op::Put { .. } => written += 1,
                                Op::Mkdir { .. } => folders += 1,
                                Op::Del { .. } => deleted += 1,
                            },
                            Err(e) => {
                                failed += 1;
                                log(&format!("could not apply {}: {e}", op.path()));
                                errors.push((op.path().to_string(), e));
                            }
                        }
                    }
                }
                s.send(&proto::encode(&Msg::Ack { seq, errors }))?;
            }
            _ => return Err("unexpected message from the sender".into()),
        }
        if last_stats.elapsed() >= Duration::from_secs(60) {
            if written + folders + deleted + failed > 0 {
                log(&format!("last minute: {written} files written, {folders} folders, {deleted} deleted, {failed} failed"));
            }
            (written, folders, deleted, failed) = (0, 0, 0, 0);
            last_stats = Instant::now();
        }
    }
}

fn not_found(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

/// Applies one change under `root`.
fn apply(root: &Path, op: &Op) -> Res<()> {
    let parts = tree::safe_parts(op.path())?;
    if parts.iter().any(|p| p.starts_with(TMP_PREFIX)) {
        return Err("refused a temp-file name".into());
    }
    let is_del = matches!(op, Op::Del { .. });
    let err = |what: &str, e: std::io::Error| format!("{what}: {e}");

    // Walk down the parent folders without following symlinks; for a write, a file or symlink
    // where a folder should be is replaced by a folder (the sender's side has a folder there).
    let mut dir = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        dir.push(part);
        match fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) if is_del => return Ok(()),
            Ok(_) => {
                fs::remove_file(&dir).map_err(|e| err("replace file with folder", e))?;
                fs::create_dir(&dir).map_err(|e| err("make folder", e))?;
            }
            Err(e) if not_found(&e) && is_del => return Ok(()),
            Err(e) if not_found(&e) => fs::create_dir(&dir).map_err(|e| err("make folder", e))?,
            Err(e) => return Err(err("read folder", e)),
        }
    }
    let target = dir.join(parts[parts.len() - 1]);
    let existing = match fs::symlink_metadata(&target) {
        Ok(m) => Some(m),
        Err(e) if not_found(&e) => None,
        Err(e) => return Err(err("read", e)),
    };

    match op {
        Op::Del { .. } => match existing {
            Some(m) if m.is_dir() => fs::remove_dir_all(&target).map_err(|e| err("delete folder", e)),
            Some(_) => fs::remove_file(&target).map_err(|e| err("delete", e)),
            None => Ok(()),
        },
        Op::Mkdir { mode, .. } => {
            match existing {
                Some(m) if m.is_dir() => {}
                Some(_) => {
                    fs::remove_file(&target).map_err(|e| err("replace file with folder", e))?;
                    fs::create_dir(&target).map_err(|e| err("make folder", e))?;
                }
                None => fs::create_dir(&target).map_err(|e| err("make folder", e))?,
            }
            fs::set_permissions(&target, fs::Permissions::from_mode(*mode)).map_err(|e| err("set permissions", e))
        }
        Op::Put { mode, mtime, data, .. } => {
            if existing.as_ref().is_some_and(|m| m.is_dir()) {
                fs::remove_dir_all(&target).map_err(|e| err("replace folder with file", e))?;
            }
            let tmp = dir.join(format!(
                "{TMP_PREFIX}{}-{}",
                std::process::id(),
                TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let result = (|| -> std::io::Result<()> {
                let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
                f.write_all(data)?;
                f.set_permissions(fs::Permissions::from_mode(*mode))?;
                if mtime.0 >= 0 {
                    f.set_modified(UNIX_EPOCH + Duration::new(mtime.0 as u64, mtime.1.min(999_999_999)))?;
                }
                // No fsync per file: rename over the old file is crash-safe on ext4 for this
                // pattern, and after a crash the sender's full compare on reconnect resends
                // anything the disk lost. Flushing 2,000 files one by one took 6 s.
                drop(f);
                fs::rename(&tmp, &target)
            })();
            if let Err(e) = result {
                let _ = fs::remove_file(&tmp);
                return Err(err("write", e));
            }
            Ok(())
        }
    }
}

/// Deletes temp files a killed receiver left behind (they are never part of the copy).
fn remove_leftover_tmp(root: &Path) -> usize {
    let mut removed = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for item in rd.flatten() {
            let Ok(meta) = item.metadata() else { continue };
            if meta.is_dir() && !meta.file_type().is_symlink() {
                stack.push(item.path());
            } else if item.file_name().to_string_lossy().starts_with(TMP_PREFIX) && fs::remove_file(item.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}
