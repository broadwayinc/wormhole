//! wormhole: keeps a live, one-way copy of a folder on another Linux server.
//!
//! One binary, run on both ends:
//!   wormhole keygen  <key file>                                   make the shared key (once)
//!   wormhole receive --root <dir> --listen <ip:port> --key <file>  on the box that gets the copy
//!   wormhole send    --root <dir> --to <ip:port> --key <file>      on the box that owns the files
//!   wormhole verify  --root <dir> --to <ip:port> --key <file>      on the sending box: equal or not?
//!
//! The sender watches the folder (inotify) and sends every change within --debounce-ms. The
//! receiver writes each file atomically (temp file + rename). The connection is encrypted and
//! authenticated with the shared key (Noise protocol, see secure.rs).

mod proto;
mod receive;
mod secure;
mod send;
mod tree;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

pub type Res<T> = Result<T, String>;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "wormhole: live one-way copy of a folder to another server

Usage:
  wormhole keygen  <key file>
  wormhole receive --root <dir> --listen <ip:port> --key <key file>
  wormhole send    --root <dir> --to <ip:port> --key <key file> [--exclude <path>]...
  wormhole verify  --root <dir> --to <ip:port> --key <key file> [--exclude <path>]...

Options:
  --root <dir>         the folder to copy from (send, verify) or into (receive)
  --to <ip:port>       the receiver's address
  --listen <ip:port>   the address the receiver listens on
  --key <file>         the shared key made by `wormhole keygen` (same file on both servers)
  --exclude <path>     a path inside --root to leave alone on both sides (repeatable)
  --debounce-ms <n>    send a changed file after it has been quiet this long (default 200)
  --rescan-secs <n>    compare the whole folder with the receiver this often (default 600)

See README.md for the server setup.";

/// Parsed command-line options; which ones are required depends on the command.
#[derive(Default)]
pub struct Opts {
    pub root: Option<PathBuf>,
    pub to: Option<String>,
    pub listen: Option<String>,
    pub key: Option<PathBuf>,
    pub excludes: Vec<String>,
    pub debounce_ms: u64,
    pub rescan_secs: u64,
    pub positional: Vec<String>,
}

impl Opts {
    fn parse(args: &[String]) -> Res<Opts> {
        let mut o = Opts { debounce_ms: 200, rescan_secs: 600, ..Default::default() };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let mut value = |name: &str| -> Res<String> {
                it.next().cloned().ok_or_else(|| format!("{name} needs a value"))
            };
            match a.as_str() {
                "--root" => o.root = Some(PathBuf::from(value(a)?)),
                "--to" => o.to = Some(value(a)?),
                "--listen" => o.listen = Some(value(a)?),
                "--key" => o.key = Some(PathBuf::from(value(a)?)),
                "--exclude" => {
                    let raw = value(a)?;
                    let clean = raw.trim_matches('/').to_string();
                    if clean.is_empty() || clean.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
                        return Err(format!("--exclude \"{raw}\" must be a plain path inside --root, like cloud-import/tmp"));
                    }
                    o.excludes.push(clean);
                }
                "--debounce-ms" => o.debounce_ms = parse_num(a, &value(a)?)?,
                "--rescan-secs" => o.rescan_secs = parse_num(a, &value(a)?)?.max(1),
                s if s.starts_with("--") => return Err(format!("unknown option {s}")),
                s => o.positional.push(s.to_string()),
            }
        }
        Ok(o)
    }

    pub fn need_root(&self) -> Res<&Path> {
        self.root.as_deref().ok_or_else(|| "--root is required".to_string())
    }
    pub fn need_key(&self) -> Res<[u8; 32]> {
        load_key(self.key.as_deref().ok_or_else(|| "--key is required".to_string())?)
    }
}

fn parse_num(name: &str, v: &str) -> Res<u64> {
    v.parse().map_err(|_| format!("{name} needs a whole number, got \"{v}\""))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if matches!(cmd.as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if matches!(cmd.as_str(), "-V" | "--version" | "version") {
        println!("wormhole {VERSION}");
        return ExitCode::SUCCESS;
    }
    let opts = match Opts::parse(&args[1..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("wormhole: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match cmd.as_str() {
        "keygen" => keygen(&opts),
        "receive" => receive::run(&opts),
        "send" => send::run_send(&opts),
        "verify" => send::run_verify(&opts),
        other => Err(format!("unknown command \"{other}\"; run `wormhole --help`")),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wormhole: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `wormhole keygen <file>`: 32 random bytes as hex, mode 0600, never over an existing file.
fn keygen(o: &Opts) -> Res<ExitCode> {
    let [path] = o.positional.as_slice() else {
        return Err("usage: wormhole keygen <key file>".into());
    };
    let mut key = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut key))
        .map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => format!("{path} already exists; not overwriting it"),
            _ => format!("cannot create {path}: {e}"),
        })?;
    writeln!(f, "{}", hex(&key)).map_err(|e| format!("cannot write {path}: {e}"))?;
    println!("Wrote a new key to {path} (readable by you only). Copy this same file to the other server.");
    Ok(ExitCode::SUCCESS)
}

fn load_key(path: &Path) -> Res<[u8; 32]> {
    let shown = path.display();
    let meta = fs::metadata(path).map_err(|e| format!("cannot read key file {shown}: {e}"))?;
    if meta.permissions().mode() & 0o077 != 0 {
        log(&format!("warning: key file {shown} can be read by other users; run: chmod 600 {shown}"));
    }
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read key file {shown}: {e}"))?;
    let text = text.trim();
    let bad = || format!("key file {shown} is not a wormhole key (expected 64 hex characters from `wormhole keygen`)");
    if text.len() != 64 || !text.is_ascii() {
        return Err(bad());
    }
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(|_| bad())?;
    }
    Ok(key)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One log line with a UTC timestamp, to stdout (pm2 / systemd keep it).
pub fn log(msg: &str) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    println!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC  {msg}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    );
}

/// "4.2 KB" style sizes for log lines.
pub fn human_bytes(n: u64) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)),
    }
}
