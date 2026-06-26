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
}
