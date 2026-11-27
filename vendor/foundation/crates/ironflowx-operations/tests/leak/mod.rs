//! The search for a key's secret in text and in files (TEN-3). `secrets.rs` tests it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The 256 bits a key's secret part encodes.
fn bits(secret: &str) -> Vec<u8> {
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let (mut held, mut held_bits, mut out) = (0u32, 0, Vec::new());
    for c in secret.chars() {
        let six = alphabet.find(c).expect("base64url");
        held = held << 6 | u32::try_from(six).unwrap();
        held_bits += 6;
        if held_bits >= 8 {
            held_bits -= 8;
            out.push((held >> held_bits & 0xff) as u8);
        }
    }
    out
}

/// Bytes as a string `str::contains` searches: each byte the character of that number.
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| char::from(*b)).collect()
}

/// `bytes` in base64, but for what does not fill a group of 3.
fn base64(bytes: &[u8]) -> String {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let groups = bytes.as_chunks::<3>().0.iter();
    let held = groups.map(|g| u32::from(g[0]) << 16 | u32::from(g[1]) << 8 | u32::from(g[2]));
    held.flat_map(|g| [18, 12, 6, 0].map(|at| char::from(alphabet[(g >> at & 63) as usize])))
        .collect()
}

/// The halves of a key's secret: of its text, and of the 256 bits that encodes. Half of it is
/// enough to matter.
fn halves(key: &str) -> [Vec<u8>; 4] {
    let (text, bits) = (&key.as_bytes()[41..], bits(&key[41..]));
    let (a, b) = text.split_at(21);
    let (c, d) = bits.split_at(16);
    [a, b, c, d].map(<[u8]>::to_vec)
}

/// What gives a secret away in `text`: half of one of `keys`, or the start of any key's text. A
/// half as it is, in either base64 alphabet, as a list of numbers, decimal or hex, as a run of hex
/// digits, whatever the case, the spaces and the `0x` between them, or as `escape_ascii` writes
/// it: what one usual call to std's formatting gives; or within base64 of any text that holds it,
/// such as a header, where the groups of 3 it fills are enough: from 12 bytes, fewer than a half,
/// which is stricter and harmless. In no other form: not with a `char` for each byte, unless in a
/// path, nor as hex with anything else between. It can miss base64 that is wrapped into shorter
/// lines (of 16 characters, say), and base64 of base64.
pub fn leak<'a>(text: &str, keys: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    let list: String = text.split_whitespace().collect();
    let list = list.to_lowercase().replace("0x", "");
    let run = list.replace(',', "");
    let standard = text.replace('-', "+").replace('_', "/");
    for key in keys {
        for half in halves(key) {
            let as_is = latin1(&half);
            let each = |f: fn(&u8) -> String| half.iter().map(f).collect::<Vec<_>>();
            let in_base64 = as_is.replace('-', "+").replace('_', "/");
            if text.contains(&as_is) || text.contains(&in_base64) {
                return Some("a secret's text");
            }
            // Wherever its groups of 3 begin.
            if (0..3).any(|at| standard.contains(&base64(&half[at..]))) {
                return Some("a secret in base64");
            }
            if list.contains(&each(|b| format!("{b}")).join(","))
                || list.contains(&each(|b| format!("{b:x}")).join(","))
            {
                return Some("a secret as a list");
            }
            if run.contains(&each(|b| format!("{b:02x}")).concat()) {
                return Some("a secret in hex");
            }
            if text.contains(&half.escape_ascii().to_string()) {
                return Some("a secret, escaped");
            }
        }
    }
    // The key format a hint names goes on with `<`.
    let mut starts = text.match_indices("ixk_");
    let start = starts.find(|(at, _)| text[at + 4..].starts_with(|c: char| c.is_ascii_hexdigit()));
    start.map(|_| "a key's text")
}

/// As [`leak`], in a file's or directory's path or in a file's bytes.
pub fn leak_in<'a>(
    (path, bytes): &(PathBuf, String),
    keys: impl IntoIterator<Item = &'a str>,
) -> Option<&'static str> {
    let raw = latin1(path.as_os_str().as_encoded_bytes());
    leak(&format!("{}\n{raw}\n{bytes}", path.display()), keys)
}

/// Every file and directory under `root`, by its path there, a directory empty. Copied by another
/// process: closing a file this one opened would drop the locks SQLite holds on it here.
pub fn files(root: &Path) -> Vec<(PathBuf, String)> {
    let copy = tempfile::tempdir().unwrap();
    let copied = copy.path().join("root");
    let cp = Command::new("cp").arg("-R").arg(root).arg(&copied).status();
    assert!(cp.unwrap().success());
    let (mut out, mut stack) = (Vec::new(), vec![copied.clone()]);
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let mut bytes = String::new();
            if path.is_dir() {
                stack.push(path.clone());
            } else {
                bytes = latin1(&std::fs::read(&path).unwrap());
            }
            out.push((path.strip_prefix(&copied).unwrap().to_owned(), bytes));
        }
    }
    out
}
