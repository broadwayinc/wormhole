//! The messages the two sides exchange, and their binary encoding.
//!
//! Every message is one tag byte and its fields (big-endian integers, length-prefixed bytes),
//! then either sent as is or LZ4-compressed when that makes it smaller (first byte 0 or 1).

use crate::Res;

/// Bumped when the message format changes; both sides must run the same one.
pub const PROTO_VERSION: u8 = 1;

pub const ROLE_SYNC: u8 = 1;
pub const ROLE_VERIFY: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Dir,
}

/// One file or folder as one side sees it. `hash` is BLAKE2s-256 of the content (zero for folders).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub kind: Kind,
    pub mode: u32,
    pub hash: [u8; 32],
}

pub enum Op {
    /// Write this file (whole content), with these permission bits and modified time.
    Put { path: String, mode: u32, mtime: (i64, u32), data: Vec<u8> },
    /// Make sure this folder exists with these permission bits.
    Mkdir { path: String, mode: u32 },
    /// Remove this file or folder (with everything in it).
    Del { path: String },
}

impl Op {
    pub fn path(&self) -> &str {
        match self {
            Op::Put { path, .. } | Op::Mkdir { path, .. } | Op::Del { path } => path,
        }
    }
}

pub enum Msg {
    Hello { version: u8, role: u8 },
    ManifestReq,
    Manifest(Vec<(String, Entry)>),
    Batch { seq: u64, ops: Vec<Op> },
    /// `errors` lists the ops of that batch that could not be applied: (path, reason).
    Ack { seq: u64, errors: Vec<(String, String)> },
    Ping,
    Pong,
    Refuse(String),
}

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
    }
    fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
}

struct R<'a>(&'a [u8]);
impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Res<&'a [u8]> {
        if self.0.len() < n {
            return Err("malformed message (too short)".into());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Res<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Res<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Res<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Res<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> Res<String> {
        String::from_utf8(self.bytes()?).map_err(|_| "malformed message (bad text)".into())
    }
}

pub fn encode(msg: &Msg) -> Vec<u8> {
    let mut w = W(Vec::new());
    match msg {
        Msg::Hello { version, role } => {
            w.u8(1);
            w.u8(*version);
            w.u8(*role);
        }
        Msg::ManifestReq => w.u8(2),
        Msg::Manifest(entries) => {
            w.u8(3);
            w.u32(entries.len() as u32);
            for (path, e) in entries {
                w.str(path);
                w.u8(if e.kind == Kind::File { 1 } else { 2 });
                w.u32(e.mode);
                w.0.extend_from_slice(&e.hash);
            }
        }
        Msg::Batch { seq, ops } => {
            w.u8(4);
            w.u64(*seq);
            w.u32(ops.len() as u32);
            for op in ops {
                match op {
                    Op::Put { path, mode, mtime, data } => {
                        w.u8(1);
                        w.str(path);
                        w.u32(*mode);
                        w.u64(mtime.0 as u64);
                        w.u32(mtime.1);
                        w.bytes(data);
                    }
                    Op::Mkdir { path, mode } => {
                        w.u8(2);
                        w.str(path);
                        w.u32(*mode);
                    }
                    Op::Del { path } => {
                        w.u8(3);
                        w.str(path);
                    }
                }
            }
        }
        Msg::Ack { seq, errors } => {
            w.u8(5);
            w.u64(*seq);
            w.u32(errors.len() as u32);
            for (p, e) in errors {
                w.str(p);
                w.str(e);
            }
        }
        Msg::Ping => w.u8(6),
        Msg::Pong => w.u8(7),
        Msg::Refuse(why) => {
            w.u8(8);
            w.str(why);
        }
    }
    let body = w.0;
    if body.len() >= 512 {
        let packed = lz4_flex::compress_prepend_size(&body);
        if packed.len() < body.len() {
            let mut out = Vec::with_capacity(1 + packed.len());
            out.push(1);
            out.extend_from_slice(&packed);
            return out;
        }
    }
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(0);
    out.extend_from_slice(&body);
    out
}

pub fn decode(raw: &[u8]) -> Res<Msg> {
    let (flag, rest) = raw.split_first().ok_or("empty message")?;
    let unpacked;
    let body: &[u8] = match flag {
        0 => rest,
        1 => {
            unpacked = lz4_flex::decompress_size_prepended(rest).map_err(|e| format!("malformed message: {e}"))?;
            &unpacked
        }
        _ => return Err("malformed message (unknown encoding)".into()),
    };
    let mut r = R(body);
    let msg = match r.u8()? {
        1 => Msg::Hello { version: r.u8()?, role: r.u8()? },
        2 => Msg::ManifestReq,
        3 => {
            let n = r.u32()? as usize;
            let mut entries = Vec::with_capacity(n.min(1 << 20));
            for _ in 0..n {
                let path = r.str()?;
                let kind = if r.u8()? == 1 { Kind::File } else { Kind::Dir };
                let mode = r.u32()?;
                let hash: [u8; 32] = r.take(32)?.try_into().unwrap();
                entries.push((path, Entry { kind, mode, hash }));
            }
            Msg::Manifest(entries)
        }
        4 => {
            let seq = r.u64()?;
            let n = r.u32()? as usize;
            let mut ops = Vec::with_capacity(n.min(1 << 16));
            for _ in 0..n {
                ops.push(match r.u8()? {
                    1 => Op::Put {
                        path: r.str()?,
                        mode: r.u32()?,
                        mtime: (r.u64()? as i64, r.u32()?),
                        data: r.bytes()?,
                    },
                    2 => Op::Mkdir { path: r.str()?, mode: r.u32()? },
                    3 => Op::Del { path: r.str()? },
                    _ => return Err("malformed message (unknown change)".into()),
                });
            }
            Msg::Batch { seq, ops }
        }
        5 => {
            let seq = r.u64()?;
            let n = r.u32()? as usize;
            let mut errors = Vec::with_capacity(n.min(1 << 16));
            for _ in 0..n {
                errors.push((r.str()?, r.str()?));
            }
            Msg::Ack { seq, errors }
        }
        6 => Msg::Ping,
        7 => Msg::Pong,
        8 => Msg::Refuse(r.str()?),
        _ => return Err("malformed message (unknown type)".into()),
    };
    Ok(msg)
}
