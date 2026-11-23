//! The encrypted connection.
//!
//! Noise protocol (the framework WireGuard is built on) with a pre-shared key:
//! Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s. Both sides must hold the same 32-byte key file; a peer
//! without it fails on the first handshake message and gets nothing. Every connection gets fresh
//! session keys (X25519), so a key file leaked later does not decrypt recorded traffic.
//!
//! On the wire every Noise message is `u16 length + ciphertext` (at most 65535 bytes). An
//! application message is `u32 length + payload`, cut into as many Noise messages as it needs.

use crate::Res;
use snow::{Builder, TransportState};
use std::io::{Read, Write};
use std::net::TcpStream;

const PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const MAX_NOISE: usize = 65_535;
const TAG: usize = 16;
const CHUNK: usize = MAX_NOISE - TAG;

pub struct Secure {
    stream: TcpStream,
    noise: TransportState,
    buf: Vec<u8>,
    pending: Vec<u8>,
    pub bytes_sent: u64,
}

fn read_frame(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len)?;
    let mut frame = vec![0u8; u16::from_be_bytes(len) as usize];
    stream.read_exact(&mut frame)?;
    Ok(frame)
}

fn write_frame(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(2 + data.len());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    stream.write_all(&out)
}

fn io_err(what: &str, e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::UnexpectedEof => format!("{what}: the other side closed the connection"),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => format!("{what}: no answer (timed out)"),
        _ => format!("{what}: {e}"),
    }
}

impl Secure {
    /// The sender's side of the handshake.
    pub fn connect(mut stream: TcpStream, key: &[u8; 32]) -> Res<Secure> {
        let mut hs = Builder::new(PARAMS.parse().map_err(|e| format!("noise: {e}"))?)
            .psk(0, key)
            .and_then(|b| b.build_initiator())
            .map_err(|e| format!("noise: {e}"))?;
        let mut buf = vec![0u8; MAX_NOISE];
        let n = hs.write_message(&[], &mut buf).map_err(|e| format!("handshake: {e}"))?;
        write_frame(&mut stream, &buf[..n]).map_err(|e| io_err("handshake", e))?;
        let reply = read_frame(&mut stream).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                "handshake refused by the receiver: the key files on the two servers are not the same".to_string()
            } else {
                io_err("handshake", e)
            }
        })?;
        hs.read_message(&reply, &mut buf).map_err(|_| {
            "handshake failed: the key files on the two servers are not the same".to_string()
        })?;
        let noise = hs.into_transport_mode().map_err(|e| format!("handshake: {e}"))?;
        Ok(Secure { stream, noise, buf, pending: Vec::new(), bytes_sent: 0 })
    }

    /// The receiver's side of the handshake. Fails (and sends nothing back) on a wrong key.
    pub fn accept(mut stream: TcpStream, key: &[u8; 32]) -> Res<Secure> {
        let mut hs = Builder::new(PARAMS.parse().map_err(|e| format!("noise: {e}"))?)
            .psk(0, key)
            .and_then(|b| b.build_responder())
            .map_err(|e| format!("noise: {e}"))?;
        let mut buf = vec![0u8; MAX_NOISE];
        let first = read_frame(&mut stream).map_err(|e| io_err("handshake", e))?;
        hs.read_message(&first, &mut buf)
            .map_err(|_| "wrong key (or not a wormhole sender); connection dropped".to_string())?;
        let n = hs.write_message(&[], &mut buf).map_err(|e| format!("handshake: {e}"))?;
        write_frame(&mut stream, &buf[..n]).map_err(|e| io_err("handshake", e))?;
        let noise = hs.into_transport_mode().map_err(|e| format!("handshake: {e}"))?;
        Ok(Secure { stream, noise, buf, pending: Vec::new(), bytes_sent: 0 })
    }

    pub fn send(&mut self, payload: &[u8]) -> Res<()> {
        let mut plain = Vec::with_capacity(4 + payload.len());
        plain.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        plain.extend_from_slice(payload);
        let mut out = Vec::with_capacity(plain.len() + (plain.len() / CHUNK + 1) * (2 + TAG));
        for chunk in plain.chunks(CHUNK) {
            let n = self.noise.write_message(chunk, &mut self.buf).map_err(|e| format!("encrypt: {e}"))?;
            out.extend_from_slice(&(n as u16).to_be_bytes());
            out.extend_from_slice(&self.buf[..n]);
        }
        self.stream.write_all(&out).map_err(|e| io_err("send", e))?;
        self.bytes_sent += out.len() as u64;
        Ok(())
    }

    pub fn recv(&mut self) -> Res<Vec<u8>> {
        loop {
            if self.pending.len() >= 4 {
                let len = u32::from_be_bytes(self.pending[..4].try_into().unwrap()) as usize;
                if self.pending.len() >= 4 + len {
                    let msg = self.pending[4..4 + len].to_vec();
                    self.pending.drain(..4 + len);
                    return Ok(msg);
                }
            }
            let frame = read_frame(&mut self.stream).map_err(|e| io_err("receive", e))?;
            let n = self
                .noise
                .read_message(&frame, &mut self.buf)
                .map_err(|_| "receive: a message failed decryption; connection dropped".to_string())?;
            self.pending.extend_from_slice(&self.buf[..n]);
        }
    }
}
