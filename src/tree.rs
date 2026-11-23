//! Walking and hashing the folder, and the path rules both sides follow.
//!
//! Paths on the wire are relative to --root, '/'-separated, UTF-8, without "." or "..".
//! Symlinks are never followed or copied. Files whose names start with TMP_PREFIX are the
//! receiver's own half-written files and are invisible to both sides.

use crate::proto::{Entry, Kind};
use crate::Res;
use blake2::{Blake2s256, Digest};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

pub const TMP_PREFIX: &str = ".wormhole-tmp-";

pub type Manifest = BTreeMap<String, Entry>;

/// `path` is `ex` or inside `ex`.
pub fn under(path: &str, ex: &str) -> bool {
    ex.is_empty() || path == ex || (path.len() > ex.len() && path.starts_with(ex) && path.as_bytes()[ex.len()] == b'/')
}

pub fn excluded(path: &str, excludes: &[String]) -> bool {
    excludes.iter().any(|ex| under(path, ex))
}

pub fn is_tmp(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(|n| n.starts_with(TMP_PREFIX))
}

pub fn mode_of(meta: &fs::Metadata) -> u32 {
    meta.permissions().mode() & 0o7777
}

pub fn hash_bytes(data: &[u8]) -> [u8; 32] {
    Blake2s256::digest(data).into()
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut f = fs::File::open(path)?;
    let mut h = Blake2s256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.finalize().into());
        }
        h.update(&buf[..n]);
    }
}

/// The relative wire path of an absolute path under `root` ("" for root itself), or None when
/// it is outside root or not UTF-8.
pub fn rel_of(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?),
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// Checks a wire path from the other side and returns its components. Refuses anything that
/// could point outside root: empty, absolute, ".", "..", empty segments, NUL.
pub fn safe_parts(rel: &str) -> Res<Vec<&str>> {
    if rel.is_empty() || rel.contains('\0') {
        return Err(format!("refused path \"{rel}\""));
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.iter().any(|p| p.is_empty() || *p == "." || *p == "..") {
        return Err(format!("refused path \"{rel}\""));
    }
    Ok(parts)
}

/// Every file and folder at or under `scope` ("" = the whole tree, root itself not listed),
/// minus excluded paths, temp files, symlinks and anything that is not a file or folder.
/// Files that disappear while we look are skipped.
pub fn scan(root: &Path, scope: &str, excludes: &[String]) -> Res<Manifest> {
    let mut out = Manifest::new();
    let mut stack: Vec<(String, PathBuf)> = Vec::new();
    if scope.is_empty() {
        stack.push((String::new(), root.to_path_buf()));
    } else {
        if excluded(scope, excludes) || is_tmp(scope) {
            return Ok(out);
        }
        let abs = root.join(scope);
        match fs::symlink_metadata(&abs) {
            Ok(m) if m.is_dir() => {
                out.insert(scope.to_string(), Entry { kind: Kind::Dir, mode: mode_of(&m), hash: [0; 32] });
                stack.push((scope.to_string(), abs));
            }
            Ok(m) if m.is_file() => {
                if let Ok(hash) = hash_file(&abs) {
                    out.insert(scope.to_string(), Entry { kind: Kind::File, mode: mode_of(&m), hash });
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot read {}: {e}", abs.display())),
        }
    }
    while let Some((rel_dir, abs_dir)) = stack.pop() {
        let rd = match fs::read_dir(&abs_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot read folder {}: {e}", abs_dir.display())),
        };
        for item in rd {
            let Ok(item) = item else { continue };
            let Some(name) = item.file_name().to_str().map(str::to_string) else {
                crate::log(&format!("skipped a name that is not UTF-8 in {}", abs_dir.display()));
                continue;
            };
            if name.starts_with(TMP_PREFIX) {
                continue;
            }
            let rel = if rel_dir.is_empty() { name } else { format!("{rel_dir}/{name}") };
            if excluded(&rel, excludes) {
                continue;
            }
            let abs = item.path();
            let Ok(meta) = fs::symlink_metadata(&abs) else { continue };
            if meta.is_dir() {
                out.insert(rel.clone(), Entry { kind: Kind::Dir, mode: mode_of(&meta), hash: [0; 32] });
                stack.push((rel, abs));
            } else if meta.is_file() {
                match hash_file(&abs) {
                    Ok(hash) => {
                        out.insert(rel, Entry { kind: Kind::File, mode: mode_of(&meta), hash });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("cannot read {}: {e}", abs.display())),
                }
            }
        }
    }
    Ok(out)
}
