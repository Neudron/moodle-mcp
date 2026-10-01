//! Read the live Moodle session cookie out of Firefox's session store.
//!
//! `MoodleSession*` is a session-only cookie, so it lives in
//! `sessionstore-backups/recovery.jsonlz4` (mozlz4), not `cookies.sqlite`.
//! Error text never carries cookie values.

use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"mozLz40\0";
const MAX_OUT: usize = 256 * 1024 * 1024;
const MOODLE_HOST: &str = "educaciodigital.cat";

#[derive(Debug, PartialEq, Eq)]
pub enum FfError {
    /// No profile, no session file or no Moodle cookie in it.
    NoSession,
    /// Corrupt input (static reason, never data).
    Bad(&'static str),
}

impl std::fmt::Display for FfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSession => f.write_str("no firefox moodle session"),
            Self::Bad(m) => write!(f, "bad firefox data: {m}"),
        }
    }
}

/// Standard-alphabet base64 (padding optional). `None` on any bad byte.
#[must_use]
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn lz4_block(src: &[u8], size: usize) -> Result<Vec<u8>, FfError> {
    const BAD: FfError = FfError::Bad("lz4");
    let mut out: Vec<u8> = Vec::with_capacity(size);
    let mut i = 0;
    let len = |i: &mut usize, mut n: usize| -> Result<usize, FfError> {
        if n == 15 {
            loop {
                let b = *src.get(*i).ok_or(BAD)?;
                *i += 1;
                n += usize::from(b);
                if b != 255 {
                    break;
                }
            }
        }
        Ok(n)
    };
    while i < src.len() {
        let token = src[i];
        i += 1;
        let lit = len(&mut i, usize::from(token >> 4))?;
        let end = i.checked_add(lit).filter(|e| *e <= src.len()).ok_or(BAD)?;
        out.extend_from_slice(&src[i..end]);
        i = end;
        if i == src.len() {
            break;
        }
        let off = src.get(i..i + 2).ok_or(BAD)?;
        let off = usize::from(u16::from_le_bytes([off[0], off[1]]));
        i += 2;
        let mlen = len(&mut i, usize::from(token & 15))? + 4;
        if off == 0 || off > out.len() || out.len() + mlen > size {
            return Err(BAD);
        }
        let start = out.len() - off;
        for k in 0..mlen {
            out.push(out[start + k]);
        }
    }
    if out.len() == size {
        Ok(out)
    } else {
        Err(BAD)
    }
}

/// Decode a Firefox `.jsonlz4` blob: magic, LE u32 length, LZ4 block.
pub fn mozlz4_decode(data: &[u8]) -> Result<Vec<u8>, FfError> {
    if data.len() < 12 || &data[..8] != MAGIC {
        return Err(FfError::Bad("magic"));
    }
    let size = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
    if size > MAX_OUT {
        return Err(FfError::Bad("size"));
    }
    lz4_block(&data[12..], size)
}

/// First `MoodleSession*` cookie (name, value) for the Moodle host.
pub fn moodle_session_cookie(session_json: &[u8]) -> Result<(String, String), FfError> {
    let v: serde_json::Value =
        serde_json::from_slice(session_json).map_err(|_| FfError::Bad("json"))?;
    let mut lists: Vec<&serde_json::Value> = vec![&v["cookies"]];
    if let Some(ws) = v["windows"].as_array() {
        lists.extend(ws.iter().map(|w| &w["cookies"]));
    }
    for c in lists.iter().filter_map(|l| l.as_array()).flatten() {
        let host = c["host"].as_str().unwrap_or("");
        let name = c["name"].as_str().unwrap_or("");
        let host_ok = host.trim_start_matches('.').ends_with(MOODLE_HOST);
        if host_ok && name.starts_with("MoodleSession") {
            if let Some(val) = c["value"].as_str().filter(|s| !s.is_empty()) {
                return Ok((name.to_string(), val.to_string()));
            }
        }
    }
    Err(FfError::NoSession)
}

/// Default profile dir from `<root>/installs.ini` (`Default=` of the first
/// install section; relative paths resolve against `root`).
pub fn default_profile(root: &Path) -> Result<PathBuf, FfError> {
    let ini = std::fs::read_to_string(root.join("installs.ini")).map_err(|_| FfError::NoSession)?;
    let rel = ini
        .lines()
        .find_map(|l| l.trim().strip_prefix("Default="))
        .ok_or(FfError::NoSession)?;
    let p = Path::new(rel.trim());
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    })
}

/// Firefox config dir: `MOODLE_FIREFOX_ROOT`, else `$HOME/.mozilla/firefox`.
pub fn root() -> PathBuf {
    std::env::var_os("MOODLE_FIREFOX_ROOT").map_or_else(
        || Path::new(&std::env::var_os("HOME").unwrap_or_default()).join(".mozilla/firefox"),
        PathBuf::from,
    )
}

/// Moodle session cookie of the default profile under `root`.
pub fn session_cookie(root: &Path) -> Result<(String, String), FfError> {
    let file = default_profile(root)?.join("sessionstore-backups/recovery.jsonlz4");
    let blob = std::fs::read(file).map_err(|_| FfError::NoSession)?;
    moodle_session_cookie(&mozlz4_decode(&blob)?)
}
