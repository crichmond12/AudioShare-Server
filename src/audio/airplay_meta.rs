//! AirPlay metadata pipe (Phase 4, Slice 3).
//!
//! `shairport-sync`'s `metadata` backend writes a stream of `<item>` elements to
//! a second FIFO carrying DAAP track tags (title/artist/album), the client's
//! user-agent, album art (`PICT`), and bundle markers. [`parse_items`] turns that
//! byte stream into [`MetaEvent`]s; [`MetaAccumulator`] (Task 2) folds them into
//! commit points; [`run_metadata_reader`] (Task 3) drives it from the live FIFO.
//!
//! Session lifecycle is NOT driven here — the audio FIFO brackets the session
//! (Slice 2 deviation). `pbeg`/`pend` are parsed-and-ignored.

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Continuously read the metadata FIFO, parsing items into [`MetaEvent`]s passed
/// to `on_event`, until `stop`. Each blocking open returns when shairport opens
/// the write end; EOF (shairport closed/restarted) reopens. A partial item at a
/// read boundary is carried in `remainder` to the next read. Mirrors
/// `airplay::run_receiver`'s reopen loop; lifecycle is the audio FIFO's job, not
/// this reader's.
pub fn run_metadata_reader(
    meta_fifo: &Path,
    stop: &Arc<AtomicBool>,
    mut on_event: impl FnMut(MetaEvent),
) -> Result<(), String> {
    while !stop.load(Ordering::Relaxed) {
        let mut file = match File::open(meta_fifo) {
            Ok(f) => f,
            Err(e) => return Err(format!("open airplay meta fifo {} failed: {e}", meta_fifo.display())),
        };
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut remainder: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let n = match file.read(&mut buf) {
                Ok(0) => break, // EOF: shairport closed; reopen
                Ok(n) => n,
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("airplay meta fifo read error: {e}")),
            };
            remainder.extend_from_slice(&buf[..n]);
            let (events, consumed) = parse_items(&remainder);
            for ev in events {
                on_event(ev);
            }
            remainder.drain(..consumed);
        }
    }
    Ok(())
}

/// A decoded event from the metadata pipe. Payloads are already base64-decoded:
/// text is UTF-8 (lossy), `Art` is the raw image bytes.
#[derive(Debug, Clone, PartialEq)]
pub enum MetaEvent {
    Title(String),
    Artist(String),
    Album(String),
    Art(Vec<u8>),
    Client(String),
    /// A metadata bundle finished (`ssnc`/`mden`) — the commit point for text.
    BundleEnd,
}

/// Parse every complete `<item>…</item>` in `buf`. Returns the decoded events and
/// the number of leading bytes consumed; a trailing partial item is left for the
/// caller to re-feed with more bytes. Unknown `(type, code)` pairs and zero-length
/// `PICT` produce no event.
pub fn parse_items(buf: &[u8]) -> (Vec<MetaEvent>, usize) {
    let text = match std::str::from_utf8(buf) {
        Ok(t) => t,
        // Art bytes live inside base64, so the framing itself is ASCII. If invalid
        // UTF-8 appears mid-buffer, parse the valid prefix and re-feed the rest.
        Err(e) => std::str::from_utf8(&buf[..e.valid_up_to()]).unwrap(),
    };

    let mut events = Vec::new();
    let mut consumed = 0usize;

    loop {
        let rest = &text[consumed..];
        let Some(start) = rest.find("<item>") else { break };
        let item_abs_start = consumed + start;
        let Some(end_rel) = text[item_abs_start..].find("</item>") else {
            // Incomplete item: stop, keep from its start onward.
            consumed = item_abs_start;
            return (events, consumed);
        };
        let item_end = item_abs_start + end_rel + "</item>".len();
        let item = &text[item_abs_start..item_end];

        if let Some(ev) = parse_one(item) {
            events.push(ev);
        }
        consumed = item_end;
    }

    (events, consumed)
}

/// Parse a single complete `<item>…</item>` element into an event, if it maps to
/// one we care about.
fn parse_one(item: &str) -> Option<MetaEvent> {
    let type4 = hex_tag(item, "type")?;
    let code4 = hex_tag(item, "code")?;
    let data = decode_data(item); // None when length 0 / absent

    match (type4.as_str(), code4.as_str()) {
        ("core", "minm") => Some(MetaEvent::Title(string_of(data?))),
        ("core", "asar") => Some(MetaEvent::Artist(string_of(data?))),
        ("core", "asal") => Some(MetaEvent::Album(string_of(data?))),
        ("ssnc", "snua") => Some(MetaEvent::Client(string_of(data?))),
        ("ssnc", "PICT") => {
            let bytes = data?;
            if bytes.is_empty() { None } else { Some(MetaEvent::Art(bytes)) }
        }
        ("ssnc", "mden") => Some(MetaEvent::BundleEnd),
        _ => None,
    }
}

/// Extract `<tag>HEX</tag>` and decode the 8-hex-char value into its 4 ASCII chars.
fn hex_tag(item: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = item.find(&open)? + open.len();
    let e = item[s..].find(&close)? + s;
    let hex = &item[s..e];
    // Guard: odd length would panic on `hex[i..i+2]`; non-ASCII would give a
    // wrong byte boundary. Either means malformed FIFO input — return None
    // rather than panicking the metadata reader thread.
    if hex.len() % 2 != 0 || !hex.is_ascii() {
        return None;
    }
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

/// Decode the base64 `<data>` body of an item, if present. Whitespace inside the
/// base64 region is stripped before decoding.
fn decode_data(item: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    let marker = "<data encoding=\"base64\">";
    let s = item.find(marker)? + marker.len();
    let e = item[s..].find("</data>")? + s;
    let b64: String = item[s..e].chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD.decode(b64.as_bytes()).ok()
}

fn string_of(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A commit point produced by [`MetaAccumulator`]: either a full track snapshot
/// (on bundle end) or a new album-art image (immediately).
#[derive(Debug, Clone, PartialEq)]
pub enum MetaCommit {
    Track { title: String, artist: String, album: String, client: String },
    Art(Vec<u8>),
}

/// Folds a stream of [`MetaEvent`]s into [`MetaCommit`]s. Text fields accumulate
/// and commit together on `BundleEnd`; art commits on arrival. Fields persist
/// across bundles (shairport re-sends the full bundle per track).
#[derive(Default)]
pub struct MetaAccumulator {
    title: String,
    artist: String,
    album: String,
    client: String,
}

impl MetaAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, ev: MetaEvent) -> Option<MetaCommit> {
        match ev {
            MetaEvent::Title(t) => { self.title = t; None }
            MetaEvent::Artist(a) => { self.artist = a; None }
            MetaEvent::Album(l) => { self.album = l; None }
            MetaEvent::Client(c) => { self.client = c; None }
            MetaEvent::Art(bytes) => Some(MetaCommit::Art(bytes)),
            MetaEvent::BundleEnd => Some(MetaCommit::Track {
                title: self.title.clone(),
                artist: self.artist.clone(),
                album: self.album.clone(),
                client: self.client.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;

    /// Build one metadata item the way shairport-sync writes it.
    fn item(type4: &str, code4: &str, data: Option<&[u8]>) -> Vec<u8> {
        let type_hex: String = type4.bytes().map(|b| format!("{:02x}", b)).collect();
        let code_hex: String = code4.bytes().map(|b| format!("{:02x}", b)).collect();
        match data {
            None => format!(
                "<item><type>{type_hex}</type><code>{code_hex}</code><length>0</length></item>"
            )
            .into_bytes(),
            Some(d) => {
                let b64 = STANDARD.encode(d);
                format!(
                    "<item><type>{type_hex}</type><code>{code_hex}</code><length>{}</length>\n\
                     <data encoding=\"base64\">\n{b64}</data></item>",
                    d.len()
                )
                .into_bytes()
            }
        }
    }

    #[test]
    fn parses_title_artist_album() {
        let mut buf = item("core", "minm", Some(b"Song"));
        buf.extend(item("core", "asar", Some(b"Artist")));
        buf.extend(item("core", "asal", Some(b"Album")));
        let (events, consumed) = parse_items(&buf);
        assert_eq!(consumed, buf.len());
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], MetaEvent::Title(t) if t == "Song"));
        assert!(matches!(&events[1], MetaEvent::Artist(a) if a == "Artist"));
        assert!(matches!(&events[2], MetaEvent::Album(a) if a == "Album"));
    }

    #[test]
    fn parses_picture_as_raw_bytes() {
        let png = [0x89u8, b'P', b'N', b'G', 1, 2, 3];
        let buf = item("ssnc", "PICT", Some(&png));
        let (events, _) = parse_items(&buf);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], MetaEvent::Art(b) if b.as_slice() == png));
    }

    #[test]
    fn maps_client_and_bundle_end_and_skips_unknown() {
        let mut buf = item("ssnc", "snua", Some(b"Chris's iPhone"));
        buf.extend(item("ssnc", "pbeg", None)); // unknown-to-us: no event
        buf.extend(item("ssnc", "mden", None)); // bundle end
        let (events, consumed) = parse_items(&buf);
        assert_eq!(consumed, buf.len());
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], MetaEvent::Client(c) if c == "Chris's iPhone"));
        assert!(matches!(events[1], MetaEvent::BundleEnd));
    }

    #[test]
    fn leaves_trailing_partial_item_unconsumed() {
        let whole = item("core", "minm", Some(b"Done"));
        let mut buf = whole.clone();
        buf.extend_from_slice(b"<item><type>636f7265</type><code>6173"); // truncated
        let (events, consumed) = parse_items(&buf);
        assert_eq!(events.len(), 1);
        assert_eq!(consumed, whole.len()); // only the complete item was consumed
    }

    #[test]
    fn zero_length_picture_emits_nothing() {
        let buf = item("ssnc", "PICT", None);
        let (events, consumed) = parse_items(&buf);
        assert!(events.is_empty());
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn accumulator_commits_track_on_bundle_end() {
        let mut acc = MetaAccumulator::new();
        assert_eq!(acc.apply(MetaEvent::Title("S".into())), None);
        assert_eq!(acc.apply(MetaEvent::Artist("A".into())), None);
        assert_eq!(acc.apply(MetaEvent::Album("L".into())), None);
        let commit = acc.apply(MetaEvent::BundleEnd).expect("commit on mden");
        assert_eq!(
            commit,
            MetaCommit::Track { title: "S".into(), artist: "A".into(), album: "L".into(), client: String::new() }
        );
    }

    #[test]
    fn accumulator_emits_art_immediately() {
        let mut acc = MetaAccumulator::new();
        let commit = acc.apply(MetaEvent::Art(vec![1, 2, 3])).expect("art commits now");
        assert_eq!(commit, MetaCommit::Art(vec![1, 2, 3]));
    }

    #[test]
    fn accumulator_persists_client_across_bundles() {
        let mut acc = MetaAccumulator::new();
        acc.apply(MetaEvent::Client("Chris's iPhone".into()));
        acc.apply(MetaEvent::Title("S".into()));
        let commit = acc.apply(MetaEvent::BundleEnd).unwrap();
        match commit {
            MetaCommit::Track { client, title, .. } => {
                assert_eq!(client, "Chris's iPhone");
                assert_eq!(title, "S");
            }
            _ => panic!("expected Track"),
        }
    }

    #[test]
    fn reader_parses_events_until_eof() {
        use std::ffi::CString;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let path = std::env::temp_dir().join(format!("as-airplay-meta-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let c_path = CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo failed");

        // Writer: one title item, then close (EOF).
        let type_hex: String = "core".bytes().map(|b| format!("{:02x}", b)).collect();
        let code_hex: String = "minm".bytes().map(|b| format!("{:02x}", b)).collect();
        let b64 = {
            use base64::engine::general_purpose::STANDARD;
            use base64::Engine as _;
            STANDARD.encode(b"Hello")
        };
        let payload = format!(
            "<item><type>{type_hex}</type><code>{code_hex}</code><length>5</length>\n\
             <data encoding=\"base64\">\n{b64}</data></item>"
        );
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().write(true).open(&writer_path).unwrap();
            f.write_all(payload.as_bytes()).unwrap();
            // drop -> EOF
        });

        let stop = Arc::new(AtomicBool::new(false));
        let got = Arc::new(Mutex::new(Vec::new()));
        let reader_stop = Arc::clone(&stop);
        let reader_got = Arc::clone(&got);
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            let one_shot = Arc::clone(&reader_stop);
            let _ = run_metadata_reader(&reader_path, &reader_stop, |ev| {
                reader_got.lock().unwrap().push(ev);
                // First event observed: stop so the reopen loop terminates.
                one_shot.store(true, Ordering::Relaxed);
            });
        });

        writer.join().unwrap();
        reader.join().unwrap();
        let _ = std::fs::remove_file(&path);

        let events = got.lock().unwrap();
        assert!(matches!(events.first(), Some(MetaEvent::Title(t)) if t == "Hello"), "{events:?}");
    }

    // hex_tag must return None for malformed hex rather than panicking the
    // metadata reader thread.
    #[test]
    fn hex_tag_rejects_odd_length_hex() {
        // Odd-length hex body — would panic on hex[i..i+2] without the guard.
        let odd = "<item><type>abc</type></item>";
        assert!(hex_tag(odd, "type").is_none(), "odd-length hex must return None");
    }

    #[test]
    fn hex_tag_rejects_non_ascii_hex() {
        // Non-ASCII byte inside the hex body — would give a wrong byte boundary.
        let non_ascii = "<item><type>636f72\u{00e9}</type></item>"; // é is multi-byte
        assert!(hex_tag(non_ascii, "type").is_none(), "non-ASCII hex must return None");
    }

    #[test]
    fn hex_tag_happy_path_decodes_correctly() {
        // "core" in hex is 636f7265 — a valid 8-char ASCII hex string.
        let item = "<item><type>636f7265</type></item>";
        let result = hex_tag(item, "type").expect("valid hex must decode");
        assert_eq!(result, "core");
    }
}
