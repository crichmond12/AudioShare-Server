# AirPlay Receiver Slice 3 — Track Metadata + Album Art Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface now-playing track info (title/artist/album + best-effort client) and album art from each AirPlay receiver to the iOS client, fetched by reference via a new `get_art` task.

**Architecture:** Each `shairport-sync` receiver gains a second FIFO — the **metadata pipe** — alongside its existing audio pipe. A pure parser turns shairport's `<item>` metadata stream into events; a pure accumulator folds those events into commit-points (track update / art update). A continuous per-receiver reader thread (parallel to the existing audio pump, surviving renames) drives the accumulator and pushes results into the engine through two new `SessionSink` methods. The engine stores per-source track fields + a one-latest-image-per-source art cache keyed by an SHA-256 hex `art_id`; the `sources` push gains the new fields and `get_art` returns the cached image bytes. Session lifecycle is unchanged — the **audio FIFO** still brackets the session (open=active, EOF=ended); the metadata pipe is consumed only for track info.

**Tech Stack:** Rust (workspace binary `audioshare_device`), `shairport-sync` (classic AirPlay, `metadata` pipe backend), `base64` 0.22 (already a dep), `sha256` 1.5 (already a dep), `libc` mkfifo, std threads.

## Global Constraints

- **No new crate dependencies.** `base64 = "0.22.1"`, `sha2 = "0.10.8"`, and `sha256 = "1.5.0"` are already in `Cargo.toml`. Use `sha256::digest(bytes) -> String` for the hex `art_id`; use `base64::engine::general_purpose::STANDARD` (with `use base64::Engine as _;`) for image base64 — matching `src/security.rs`.
- **Session lifecycle stays as-built.** The audio FIFO brackets the session (Slice 2 deviation). The metadata pipe must NOT drive session begin/end; parse `pbeg`/`pend` only insofar as you ignore them. `Engine::session_began`/`session_ended` (audio-FIFO driven) are unchanged except `session_ended` additionally clears track+art.
- **`client` is best-effort:** populate from shairport's `ssnc`/`snua` when present; empty string otherwise. Never block on it.
- **Art cache = one latest image per source**, keyed by source id, value carries its content hash. A `get_art` with a stale/unknown `art_id` returns the `unknown_art` error code. Do NOT introduce `unknown_source` here — that arrives with `reroute` in Slice 4.
- **`art_id` = `sha256::digest(image_bytes)`** (lowercase hex). Empty string means "no art".
- **Lock discipline (engine):** mirror existing code — never hold `zones`/`sources` across blocking I/O; `art_cache` is an independent mutex; take it after releasing `sources`.
- **macOS caveat:** the binary reads `/proc/cpuinfo` and exits on macOS at runtime, but `cargo test` runs the device-free unit tests fine. All tests in this plan are device-free (no `shairport-sync`, no audio hardware). The live end-to-end demo is Pi-only and out of scope for CI.
- **Wire envelope unchanged:** all new messages use the existing encrypted + newline-framed channel and the `{ status, task, data?, error? }` envelope.

---

## File Structure

- **Create** `src/audio/airplay_meta.rs` — the metadata-pipe **parser** (`parse_items`, pure), the **event/commit types** (`MetaEvent`, `MetaCommit`), the **accumulator** (`MetaAccumulator`, pure), and the continuous **reader loop** (`run_metadata_reader`). Self-contained; the only file that knows shairport's metadata wire format.
- **Modify** `src/audio/airplay.rs` — add `meta_fifo_path(slot)`; add the `metadata` backend block to `shairport_config`; expose `ensure_fifo` as `pub(crate)`.
- **Modify** `src/audio/mod.rs` — declare `pub mod airplay_meta;`.
- **Modify** `src/audio/engine.rs` — extend `SessionSink` with `track_update`/`art_update`; add track fields + `art_id` to `SourceState`; add the `art_cache` + `ArtImage`; extend `SourceView`; implement `track_update`/`art_update`/`get_art`; clear track+art in `session_ended`.
- **Modify** `src/audio/airplay_factory.rs` — ensure the meta FIFO; spawn the metadata reader thread driving a `MetaAccumulator` into the engine; tear it down on drop.
- **Modify** `src/server/commands.rs` — add `Task::GetArt`, parse/round-trip `"get_art"`, dispatch it (incl. `unknown_art`).
- **Modify** `src/server/connection.rs` — include the new track fields + `art_id` in the `send_sources` payload.
- **Modify** `CLAUDE.md` — protocol section: `get_art` task, new `sources` fields, `unknown_art` error, recognized-tasks list, slice status line.

---

## Task 1: Metadata-pipe parser (pure)

**Files:**
- Create: `src/audio/airplay_meta.rs`
- Modify: `src/audio/mod.rs` (add `pub mod airplay_meta;`)
- Test: inline `#[cfg(test)]` in `src/audio/airplay_meta.rs`

**Interfaces:**
- Produces:
  - `pub enum MetaEvent { Title(String), Artist(String), Album(String), Art(Vec<u8>), Client(String), BundleEnd }`
  - `pub fn parse_items(buf: &[u8]) -> (Vec<MetaEvent>, usize)` — parses every **complete** `<item>…</item>` in `buf`, returns the events plus the number of bytes consumed. A trailing partial item is left unconsumed (caller keeps the remainder for the next read). Base64 `<data>` is decoded internally; whitespace inside the base64 region is stripped. Unknown `(type, code)` pairs produce no event. Zero-length `PICT` produces no event.

shairport's `metadata` pipe emits, per item:

```
<item><type>636f7265</type><code>6d696e6d</code><length>9</length>
<data encoding="base64">
U29tZSBTb25n</data></item>
```

`<type>`/`<code>` are the 4 ASCII bytes of the code in hex (`636f7265` = `core`, `73736e63` = `ssnc`). Zero-length items omit `<data>` and end `</length></item>`. Codes we map: `core`/`minm`→Title, `core`/`asar`→Artist, `core`/`asal`→Album, `ssnc`/`PICT`→Art (raw image bytes), `ssnc`/`snua`→Client, `ssnc`/`mden`→BundleEnd.

- [ ] **Step 1: Write the failing tests**

Add to `src/audio/airplay_meta.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib audio::airplay_meta`
Expected: FAIL to compile — `parse_items`/`MetaEvent` not found.

- [ ] **Step 3: Write the parser**

Create `src/audio/airplay_meta.rs` (module head + parser; reader loop comes in Task 3):

```rust
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
```

Add to `src/audio/mod.rs` (next to the other `pub mod` lines, e.g. after `pub mod airplay_manager;`):

```rust
pub mod airplay_meta;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib audio::airplay_meta`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/audio/airplay_meta.rs src/audio/mod.rs
git commit -m "AirPlay slice 3: metadata-pipe parser (items -> events)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Metadata accumulator (pure)

**Files:**
- Modify: `src/audio/airplay_meta.rs`
- Test: inline `#[cfg(test)]` in `src/audio/airplay_meta.rs`

**Interfaces:**
- Consumes: `MetaEvent` (Task 1).
- Produces:
  - `pub enum MetaCommit { Track { title: String, artist: String, album: String, client: String }, Art(Vec<u8>) }`
  - `pub struct MetaAccumulator { … }` with `pub fn new() -> Self` and `pub fn apply(&mut self, ev: MetaEvent) -> Option<MetaCommit>`.

Behavior: `apply` updates internal title/artist/album/client from text events (no commit); on `BundleEnd` it returns `MetaCommit::Track` with the current snapshot; on `Art(bytes)` it returns `MetaCommit::Art(bytes)` immediately. `client` persists across bundles (only `Client` changes it); title/artist/album persist too (shairport re-sends the full bundle per track). This bounds engine pushes to one per bundle + one per art change.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/audio/airplay_meta.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib audio::airplay_meta`
Expected: FAIL to compile — `MetaAccumulator`/`MetaCommit` not found.

- [ ] **Step 3: Implement the accumulator**

Add to `src/audio/airplay_meta.rs` (above the `tests` module):

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib audio::airplay_meta`
Expected: PASS (8 tests total).

- [ ] **Step 5: Commit**

```bash
git add src/audio/airplay_meta.rs
git commit -m "AirPlay slice 3: metadata accumulator (events -> commits)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Metadata FIFO config + reader loop

**Files:**
- Modify: `src/audio/airplay.rs` (`shairport_config`, add `meta_fifo_path`, `pub(crate) ensure_fifo`)
- Modify: `src/audio/airplay_meta.rs` (add `run_metadata_reader`)
- Test: inline `#[cfg(test)]` in both files

**Interfaces:**
- Consumes: `parse_items` (Task 1).
- Produces:
  - `airplay::meta_fifo_path(index: usize) -> std::path::PathBuf` → `/tmp/audioshare-airplay-{index}.meta`.
  - `airplay::ensure_fifo` becomes `pub(crate)` so the factory can mkfifo the meta FIFO.
  - `airplay_meta::run_metadata_reader(meta_fifo: &Path, stop: &Arc<AtomicBool>, on_event: impl FnMut(MetaEvent)) -> Result<(), String>` — blocking-opens the FIFO, reads to EOF parsing items (carrying a remainder across reads), re-opens on EOF, loops until `stop`. Mirrors `airplay::run_receiver`'s reopen loop.
- The generated shairport config now contains a `metadata` block pointing at the meta FIFO.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/audio/airplay.rs`:

```rust
    #[test]
    fn meta_fifo_path_is_indexed() {
        assert_eq!(meta_fifo_path(0), PathBuf::from("/tmp/audioshare-airplay-0.meta"));
        assert_eq!(meta_fifo_path(3), PathBuf::from("/tmp/audioshare-airplay-3.meta"));
    }

    #[test]
    fn config_enables_metadata_pipe() {
        let cfg = shairport_config("Kitchen", 5002, "AA5500000002", Path::new("/tmp/x.pcm"));
        assert!(cfg.contains("metadata ="), "{cfg}");
        assert!(cfg.contains("enabled = \"yes\""), "{cfg}");
        assert!(cfg.contains("include_cover_art = \"yes\""), "{cfg}");
        assert!(cfg.contains("/tmp/audioshare-airplay"), "{cfg}"); // meta pipe path derived from audio path
    }
```

Add to the `tests` module in `src/audio/airplay_meta.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib audio::airplay`
Expected: FAIL to compile — `meta_fifo_path` / `run_metadata_reader` not found, `metadata =` not in config.

- [ ] **Step 3: Implement config + paths + reader**

In `src/audio/airplay.rs`, change `ensure_fifo`'s visibility:

```rust
/// Create the FIFO at `path` if it does not already exist (mode 0o600).
pub(crate) fn ensure_fifo(path: &Path) -> Result<(), String> {
```

Add `meta_fifo_path` next to `fifo_path`:

```rust
/// Path of the metadata FIFO backing receiver `index` (parallel to [`fifo_path`]).
pub fn meta_fifo_path(index: usize) -> PathBuf {
    PathBuf::from(format!("{FIFO_PATH_BASE}-{index}.meta"))
}
```

Replace `shairport_config` to add the metadata block. It derives the meta path from the audio path by swapping the `.pcm` suffix for `.meta` so a single `fifo_path` argument still fully determines both FIFOs:

```rust
/// Build a minimal libconfig `shairport-sync` config: a named classic-AirPlay
/// receiver on `port` whose `pipe` backend writes raw PCM to `fifo_path`, plus a
/// `metadata` backend writing the DAAP/PICT metadata stream to a sibling `.meta`
/// FIFO (same stem as the audio FIFO).
fn shairport_config(name: &str, port: u16, device_id: &str, fifo_path: &Path) -> String {
    let meta_path = fifo_path.with_extension("meta");
    format!(
        "general =\n{{\n  name = \"{name}\";\n  port = {port};\n  airplay_device_id = \"{device_id}\";\n}};\n\n\
         pipe =\n{{\n  name = \"{}\";\n}};\n\n\
         metadata =\n{{\n  enabled = \"yes\";\n  include_cover_art = \"yes\";\n  pipe_name = \"{}\";\n}};\n",
        fifo_path.display(),
        meta_path.display(),
    )
}
```

> Note: `meta_fifo_path(slot)` and `fifo_path(slot).with_extension("meta")` produce the same path — the factory uses `meta_fifo_path(slot)` for the reader, the config derives it from the audio path. Both resolve to `/tmp/audioshare-airplay-{slot}.meta`.

In `src/audio/airplay_meta.rs`, add the reader loop (with the imports it needs at the top of the file):

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib audio::airplay`
Expected: PASS (existing airplay tests + `meta_fifo_path_is_indexed`, `config_enables_metadata_pipe`, `reader_parses_events_until_eof`).

- [ ] **Step 5: Commit**

```bash
git add src/audio/airplay.rs src/audio/airplay_meta.rs
git commit -m "AirPlay slice 3: metadata FIFO config + continuous reader loop

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Engine track + art state and `get_art`

**Files:**
- Modify: `src/audio/engine.rs`
- Test: inline `#[cfg(test)]` in `src/audio/engine.rs`

**Interfaces:**
- Consumes: nothing new (called by Task 5's reader thread and Task 6's dispatch).
- Produces (on `Engine`, all `pub`):
  - `fn track_update(&self, source: &str, title: &str, artist: &str, album: &str, client: &str)` — overwrites the source's track fields (leaves `client` unchanged when the arg is empty), fires `SOURCES_CHANGED`.
  - `fn art_update(&self, source: &str, image: &[u8])` — computes `art_id = sha256::digest(image)`, sniffs mime, stores one `ArtImage` per source in `art_cache`, sets the source's `art_id`, fires `SOURCES_CHANGED`.
  - `fn get_art(&self, art_id: &str) -> Option<(String, Vec<u8>)>` — returns `(mime, bytes)` of the cached image whose hash matches, else `None`.
  - `SessionSink` trait gains `track_update` + `art_update` (same signatures, forwarded by the `&'static Engine` impl).
  - `SourceView` gains `pub title/artist/album/client/art_id: String` (filled from `SourceState`).

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/audio/engine.rs` (it already has `add_idle_source`/`has_source` helpers and dummy-sink patterns):

```rust
    #[test]
    fn track_and_art_update_populate_source_view() {
        let engine = Engine::new();
        engine.add_idle_source("default", "Hub");
        engine.session_began("default"); // active so list_sources reports it

        engine.track_update("default", "Song", "Artist", "Album", "Chris's iPhone");
        engine.art_update("default", &[0xFF, 0xD8, 0xFF, 1, 2, 3]); // JPEG magic

        let views = engine.list_sources();
        let v = views.iter().find(|v| v.source == "default").expect("source present");
        assert_eq!(v.title, "Song");
        assert_eq!(v.artist, "Artist");
        assert_eq!(v.album, "Album");
        assert_eq!(v.client, "Chris's iPhone");
        assert!(!v.art_id.is_empty());

        // get_art returns the cached image + sniffed mime.
        let (mime, bytes) = engine.get_art(&v.art_id).expect("art cached");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(bytes, vec![0xFF, 0xD8, 0xFF, 1, 2, 3]);
    }

    #[test]
    fn get_art_unknown_id_returns_none() {
        let engine = Engine::new();
        assert!(engine.get_art("deadbeef").is_none());
    }

    #[test]
    fn session_ended_clears_track_and_art() {
        let engine = Engine::new();
        engine.add_idle_source("default", "Hub");
        engine.session_began("default");
        engine.track_update("default", "Song", "Artist", "Album", "Phone");
        engine.art_update("default", &[0x89, b'P', b'N', b'G', 9]);
        let art_id = engine.list_sources()[0].art_id.clone();

        engine.session_ended("default");

        // Source is now inactive (not in list_sources), and its art is evicted.
        assert!(engine.list_sources().is_empty());
        assert!(engine.get_art(&art_id).is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib audio::engine`
Expected: FAIL to compile — `track_update`/`art_update`/`get_art` not found, `SourceView` lacks `title`.

- [ ] **Step 3: Implement engine state + methods**

In `src/audio/engine.rs`:

Add the `sha256` use near the top (with the other `use` lines):

```rust
// (no extra import needed for sha256::digest — call it fully-qualified)
```

Extend the `SessionSink` trait and its `&'static Engine` impl:

```rust
pub trait SessionSink: Send + Sync {
    fn session_began(&self, source: &str);
    fn sink_for_source(&self, source: &str) -> Option<Arc<dyn AudioSink>>;
    fn session_ended(&self, source: &str);
    /// Slice 3: replace the source's track fields (empty `client` leaves it as-is).
    fn track_update(&self, source: &str, title: &str, artist: &str, album: &str, client: &str);
    /// Slice 3: a new album-art image arrived for the source.
    fn art_update(&self, source: &str, image: &[u8]);
}

impl SessionSink for &'static Engine {
    fn session_began(&self, source: &str) { Engine::session_began(self, source); }
    fn sink_for_source(&self, source: &str) -> Option<Arc<dyn AudioSink>> {
        Engine::sink_for_source(self, source)
    }
    fn session_ended(&self, source: &str) { Engine::session_ended(self, source); }
    fn track_update(&self, source: &str, title: &str, artist: &str, album: &str, client: &str) {
        Engine::track_update(self, source, title, artist, album, client);
    }
    fn art_update(&self, source: &str, image: &[u8]) { Engine::art_update(self, source, image); }
}
```

Add fields to `SourceState` (defaults wherever it is constructed — see below):

```rust
struct SourceState {
    name: String,
    dest_zone: ZoneId,
    active: bool,
    routed: bool,
    sink: Option<Arc<dyn AudioSink>>,
    // Slice 3: latest now-playing info; cleared on session end.
    title: String,
    artist: String,
    album: String,
    client: String,
    art_id: String, // sha256 hex of current art; "" when none
}
```

Add the cached-image type and extend `SourceView`:

```rust
/// One cached album-art image (the latest for a source).
struct ArtImage {
    art_id: String,
    mime: String,
    bytes: Vec<u8>,
}

pub struct SourceView {
    pub source: ZoneId,
    pub name: String,
    pub dest_zone: ZoneId,
    pub routed: bool,
    // Slice 3:
    pub title: String,
    pub artist: String,
    pub album: String,
    pub client: String,
    pub art_id: String,
}
```

Add the `art_cache` field to `Engine` and initialize it in `Engine::new` (`art_cache: Mutex::new(HashMap::new())`):

```rust
    /// One latest album-art image per source id (Slice 3). Independent mutex;
    /// taken only after releasing `sources`.
    art_cache: Mutex<HashMap<ZoneId, ArtImage>>,
```

Every place that constructs a `SourceState` must set the new fields to `String::new()`. There are three: the `reconcile_airplay` `or_insert_with` (around line 626) and the two `#[cfg(test)]` helpers `add_idle_source` (around line 656) and any other test constructor. Add to each literal:

```rust
                        title: String::new(),
                        artist: String::new(),
                        album: String::new(),
                        client: String::new(),
                        art_id: String::new(),
```

Add the methods to `impl Engine` (near `list_sources`):

```rust
    /// Replace the source's track fields. An empty `client` leaves the existing
    /// value (shairport sends it sparsely). Fires SOURCES_CHANGED.
    pub fn track_update(&self, source: &str, title: &str, artist: &str, album: &str, client: &str) {
        {
            let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
            let Some(s) = sources.get_mut(source) else { return };
            s.title = title.to_string();
            s.artist = artist.to_string();
            s.album = album.to_string();
            if !client.is_empty() {
                s.client = client.to_string();
            }
        }
        self.notify_sources_changed();
    }

    /// A new album-art image arrived for `source`: hash it (sha256 hex), sniff its
    /// mime, cache one image per source, and record its id on the source. Fires
    /// SOURCES_CHANGED.
    pub fn art_update(&self, source: &str, image: &[u8]) {
        let art_id = sha256::digest(image);
        let mime = sniff_mime(image).to_string();
        {
            let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
            let Some(s) = sources.get_mut(source) else { return };
            s.art_id = art_id.clone();
        }
        self.art_cache
            .lock()
            .expect("engine art cache mutex poisoned")
            .insert(source.to_string(), ArtImage { art_id, mime, bytes: image.to_vec() });
        self.notify_sources_changed();
    }

    /// Look up a cached image by its content hash. `None` => `unknown_art`.
    pub fn get_art(&self, art_id: &str) -> Option<(String, Vec<u8>)> {
        let cache = self.art_cache.lock().expect("engine art cache mutex poisoned");
        cache
            .values()
            .find(|a| a.art_id == art_id)
            .map(|a| (a.mime.clone(), a.bytes.clone()))
    }
```

Add the mime sniff as a free function near the bottom of the file (module scope):

```rust
/// Best-effort image mime from magic bytes; AirPlay art is JPEG or PNG.
fn sniff_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else {
        "application/octet-stream"
    }
}
```

Update `list_sources` to fill the new view fields:

```rust
    pub fn list_sources(&self) -> Vec<SourceView> {
        let sources = self.sources.lock().expect("engine sources mutex poisoned");
        sources
            .iter()
            .filter(|(_, s)| s.active)
            .map(|(id, s)| SourceView {
                source: id.clone(),
                name: s.name.clone(),
                dest_zone: s.dest_zone.clone(),
                routed: s.routed,
                title: s.title.clone(),
                artist: s.artist.clone(),
                album: s.album.clone(),
                client: s.client.clone(),
                art_id: s.art_id.clone(),
            })
            .collect()
    }
```

Update `session_ended` to clear track + art. In its `sources` lock block, after `s.sink = None;` add the clears; after the block (locks released) evict the cache entry:

```rust
    pub fn session_ended(&self, source: &str) {
        let dest = {
            let mut sources = self.sources.lock().expect("engine sources mutex poisoned");
            let Some(s) = sources.get_mut(source) else { return };
            s.active = false;
            s.routed = false;
            s.sink = None;
            // Slice 3: now-playing is session-scoped.
            s.title.clear();
            s.artist.clear();
            s.album.clear();
            s.client.clear();
            s.art_id.clear();
            s.dest_zone.clone()
        };

        let mut zones = self.zones.lock().expect("engine zones mutex poisoned");
        if let Some(z) = zones.get_mut(&dest) {
            if matches!(&z.current, Some(ZoneDriver::Airplay(s)) if s == source) {
                z.current = None;
            }
        }
        drop(zones);
        self.snapcast.release_zone(&dest);
        self.art_cache
            .lock()
            .expect("engine art cache mutex poisoned")
            .remove(source);

        self.notify_sources_changed();
        self.notify_outputs_changed();
    }
```

> If a `#[cfg(test)]` helper besides `add_idle_source` also constructs `SourceState`, give it the same five empty-string fields. Grep `SourceState {` to find every literal.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib audio::engine`
Expected: PASS (existing engine tests + the three new ones).

- [ ] **Step 5: Commit**

```bash
git add src/audio/engine.rs
git commit -m "AirPlay slice 3: engine track/art state, art cache, get_art

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Wire the metadata reader thread into the factory

**Files:**
- Modify: `src/audio/airplay_factory.rs`
- Test: inline `#[cfg(test)]` in `src/audio/airplay_factory.rs` (update `NoSessions` to the extended trait)

**Interfaces:**
- Consumes: `airplay::meta_fifo_path`, `airplay::ensure_fifo` (Task 3); `airplay_meta::{run_metadata_reader, MetaAccumulator, MetaCommit}` (Tasks 2–3); `SessionSink::{track_update, art_update}` (Task 4).
- Produces: a `ShairportReceiver` that, in addition to the audio pump, runs a `airplay-meta-{slot}` thread translating metadata commits into engine calls; its `Drop` stops + joins that thread too.

- [ ] **Step 1: Update the test stub to the extended trait, and add a guard test**

In `src/audio/airplay_factory.rs`, update `NoSessions` to satisfy the new trait methods and assert construction still doesn't spawn:

```rust
    struct NoSessions;
    impl crate::audio::engine::SessionSink for NoSessions {
        fn session_began(&self, _s: &str) {}
        fn sink_for_source(&self, _s: &str) -> Option<Arc<dyn AudioSink>> { None }
        fn session_ended(&self, _s: &str) {}
        fn track_update(&self, _s: &str, _t: &str, _a: &str, _al: &str, _c: &str) {}
        fn art_update(&self, _s: &str, _img: &[u8]) {}
    }
```

(The existing `factory_constructs_without_spawning` test already exercises construction; no new test code is required here — the metadata thread spawning is demo-gated like the audio pump. This step's deliverable is that the crate compiles against the extended trait.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib audio::airplay_factory`
Expected: FAIL to compile — `NoSessions` doesn't implement the new trait methods (and the production `ShairportReceiver` doesn't yet spawn/stop the meta thread, but compilation fails first on the trait).

- [ ] **Step 3: Spawn + tear down the metadata reader**

In `src/audio/airplay_factory.rs`:

Extend the `use` for the meta module:

```rust
use crate::audio::airplay::{self, ShairportSupervisor};
use crate::audio::airplay_manager::{ReceiverFactory, ZoneReceiver};
use crate::audio::airplay_meta::{self, MetaAccumulator, MetaCommit};
use crate::audio::engine::SessionSink;
```

In `ReceiverFactory::create`, after the audio `pump` is spawned and before constructing `ShairportReceiver`, ensure the meta FIFO and spawn the reader thread:

```rust
        // Metadata reader: ensure the meta FIFO exists, then pump shairport's
        // metadata stream into the engine via the accumulator. Runs continuously
        // (independent of the audio session), survives renames (FIFO persists).
        let meta_fifo = airplay::meta_fifo_path(slot);
        airplay::ensure_fifo(&meta_fifo)?;
        let meta = {
            let sessions = Arc::clone(&self.sessions);
            let source = zone.to_string();
            let stop = Arc::clone(&stop);
            let meta_fifo = meta_fifo.clone();
            thread::Builder::new()
                .name(format!("airplay-meta-{slot}"))
                .spawn(move || {
                    let mut acc = MetaAccumulator::new();
                    let result = airplay_meta::run_metadata_reader(&meta_fifo, &stop, |ev| {
                        if let Some(commit) = acc.apply(ev) {
                            match commit {
                                MetaCommit::Track { title, artist, album, client } => {
                                    sessions.track_update(&source, &title, &artist, &album, &client);
                                }
                                MetaCommit::Art(bytes) => sessions.art_update(&source, &bytes),
                            }
                        }
                    });
                    if let Err(e) = result {
                        eprintln!("airplay metadata reader for {source} ended: {e}");
                    }
                })
                .map_err(|e| format!("failed to spawn airplay metadata thread: {e}"))?
        };

        Ok(Box::new(ShairportReceiver {
            slot,
            supervisor,
            stop,
            pump: Mutex::new(Some(pump)),
            meta: Mutex::new(Some(meta)),
            fifo,
            meta_fifo,
        }))
```

Add the two fields to the `ShairportReceiver` struct:

```rust
struct ShairportReceiver {
    slot: usize,
    supervisor: Mutex<Option<ShairportSupervisor>>,
    stop: Arc<AtomicBool>,
    pump: Mutex<Option<JoinHandle<()>>>,
    meta: Mutex<Option<JoinHandle<()>>>,
    fifo: PathBuf,
    meta_fifo: PathBuf,
}
```

Extend `Drop` to stop + join the meta thread, nudging its FIFO open the same way the audio FIFO is nudged. Replace the existing `Drop` body's tail (after the audio-pump join) so both threads are torn down:

```rust
impl Drop for ShairportReceiver {
    fn drop(&mut self) {
        // Stop both readers first so neither blocks on a FIFO no writer will reopen.
        self.stop.store(true, Ordering::Relaxed);
        // Drop the supervisor (kills shairport), then nudge each blocking FIFO open
        // by briefly opening the write end so parked opens return and observe `stop`.
        *self.supervisor.lock().expect("supervisor mutex poisoned") = None;
        for fifo in [&self.fifo, &self.meta_fifo] {
            if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(fifo) {
                use std::io::Write;
                let _ = f.write_all(&[]);
            }
        }
        if let Some(handle) = self.pump.lock().expect("pump mutex poisoned").take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.meta.lock().expect("meta mutex poisoned").take() {
            let _ = handle.join();
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib audio::airplay_factory`
Expected: PASS (`factory_constructs_without_spawning`). Then build the whole crate: `cargo build` → success.

- [ ] **Step 5: Commit**

```bash
git add src/audio/airplay_factory.rs
git commit -m "AirPlay slice 3: factory runs metadata reader -> engine

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: `get_art` task + sources push fields

**Files:**
- Modify: `src/server/commands.rs` (add `Task::GetArt`, parse/name, dispatch)
- Modify: `src/server/connection.rs` (`send_sources` payload)
- Test: inline `#[cfg(test)]` in `src/server/commands.rs`

**Interfaces:**
- Consumes: `ENGINE.get_art` (Task 4).
- Produces: the `get_art` task → `{ status:"ok", task:"get_art", data:{ art_id, mime, image(base64) } }` or `{ status:"error", task:"get_art", error:"unknown_art" }`; the `sources` push gains `title/artist/album/client/art_id`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/server/commands.rs`:

```rust
    #[test]
    fn parses_get_art_task() {
        assert_eq!(Task::parse("get_art"), Task::GetArt);
        assert_eq!(Task::GetArt.name(), "get_art");
    }

    #[test]
    fn get_art_unknown_id_errors() {
        // No art cached -> any id is unknown_art. Device-free.
        let data = serde_json::json!({ "art_id": "deadbeef" });
        let json = dispatch(Task::GetArt, &data).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unknown_art\""));
        assert!(json.contains("\"task\":\"get_art\""));
    }

    #[test]
    fn get_art_without_id_errors() {
        let json = dispatch(Task::GetArt, &Value::Null).to_json();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"error\":\"unknown_art\""));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib server::commands`
Expected: FAIL to compile — `Task::GetArt` not found.

- [ ] **Step 3: Implement the task + dispatch + push fields**

In `src/server/commands.rs`, add the enum variant (next to `ListSources`):

```rust
    GetArt,
```

In `Task::parse`, add the arm (near `"list_sources" => Task::ListSources,`):

```rust
            "get_art" => Task::GetArt,
```

In `Task::name`, add the arm (near `Task::ListSources => "list_sources",`):

```rust
            Task::GetArt => "get_art",
```

In `dispatch`, add a match arm (before the `Task::Unknown` arm). It base64-encodes the image bytes — import the engine the same way the file already does (`use base64::Engine as _;` is local to the arm to avoid touching the file header if it's not already imported):

```rust
        Task::GetArt => {
            use base64::engine::general_purpose::STANDARD;
            use base64::Engine as _;
            match data["art_id"].as_str() {
                Some(art_id) if !art_id.is_empty() => match ENGINE.get_art(art_id) {
                    Some((mime, bytes)) => TaskResponse::accepted(
                        "get_art",
                        Some(json!({
                            "art_id": art_id,
                            "mime": mime,
                            "image": STANDARD.encode(bytes),
                        })),
                    ),
                    None => TaskResponse::error("get_art", "unknown_art"),
                },
                _ => TaskResponse::error("get_art", "unknown_art"),
            }
        }
```

> `list_sources` is special-cased in `connection::handle_task` (it pushes); `get_art` returns inline data, so routing it through `dispatch()` is correct and needs no `handle_task` change.

In `src/server/connection.rs`, extend the `send_sources` JSON map to include the new fields:

```rust
            .map(|s| json!({
                "source": s.source, "name": s.name, "dest_zone": s.dest_zone,
                "active": true, "routed": s.routed,
                "title": s.title, "artist": s.artist, "album": s.album,
                "client": s.client, "art_id": s.art_id
            }))
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib server::commands`
Expected: PASS (`parses_get_art_task`, `get_art_unknown_id_errors`, `get_art_without_id_errors` + existing). Then `cargo build` → success.

- [ ] **Step 5: Commit**

```bash
git add src/server/commands.rs src/server/connection.rs
git commit -m "AirPlay slice 3: get_art task + track/art fields in sources push

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: Documentation — freeze the wire additions in CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

**Interfaces:** none (docs). Mirrors the protocol additions already specified in `docs/superpowers/specs/2026-06-23-airplay-receiver-design.md`.

- [ ] **Step 1: Update the recognized-tasks list**

In `CLAUDE.md`, find the "Recognized tasks:" sentence in the wire-protocol section and add `get_art`:

```
Recognized tasks: `play`, `pause`, `stop`, `seek`, `volume`, `list_outputs`, `list_zones`, `create_zone`, `delete_zone`, `rename_zone`, `set_zone_outputs`, `list_sources`, `get_art`.
```

- [ ] **Step 2: Update the `sources` push doc**

In the "Live AirPlay source list (`sources`)" subsection, replace the note "Track metadata fields (`client`, `title`, `artist`, `album`, `art_id`) are added in Slice 3." with a statement that they are now present, and update the example JSON to include them:

```json
{ "status": "ok", "task": "sources",
  "data": { "sources": [
    { "source": "<home-zone-id>", "name": "Kitchen",
      "dest_zone": "<zone-id>", "active": true, "routed": true,
      "title": "Song", "artist": "Artist", "album": "Album",
      "client": "Chris's iPhone", "art_id": "<sha256-hex-or-empty>" } ] } }
```

Add a sentence: "`art_id` is the SHA-256 hex of the current album art (empty when none); fetch the bytes by reference via the `get_art` task (art is never inlined in the push). Track fields are cleared when the session ends."

- [ ] **Step 3: Document the `get_art` task**

Add a new paragraph after the `sources` push doc:

```
**Album art fetch (`get_art`) — iOS → server.** The push carries only `art_id` (a content hash); the client fetches the image once per change:
`{ "task": "get_art", "data": { "art_id": "<hash>" }, "session_token": "<UUID>" }`
→ `{ "status": "ok", "task": "get_art", "data": { "art_id": "<hash>", "mime": "image/jpeg", "image": "<base64>" } }`.
The hub caches one latest image per source keyed by its hash; a stale/unknown `art_id` returns `error` `unknown_art`. Routed through `commands::dispatch()`.
```

- [ ] **Step 4: Add the `unknown_art` error code**

In the error-codes paragraph (the one listing `unsupported_task`, `missing_url`, …), add: "`unknown_art` (`get_art` with an `art_id` not in the cache — art changed underneath, or never present)."

- [ ] **Step 5: Update the AirPlay slice status line**

Find the "**AirPlay Slice 2 is in:**" sentence in the "Current state" paragraph and append a Slice 3 clause:

```
**AirPlay Slice 3 is in:** per-receiver metadata pipe (`/tmp/audioshare-airplay-{slot}.meta`) parsed by `audio/airplay_meta.rs` (pure `parse_items` + `MetaAccumulator`) on a continuous `airplay-meta-{slot}` reader thread; the engine stores per-source track fields (title/artist/album, best-effort `client`) + a one-latest-image-per-source album-art cache keyed by an SHA-256 hex `art_id`, cleared on session end; the `sources` push carries the new fields and the new `get_art` task returns the cached image bytes (`unknown_art` on a stale id). Session lifecycle is unchanged (audio FIFO brackets the session). Reroute (Slice 4) is not yet built.
```

- [ ] **Step 6: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: freeze AirPlay slice 3 wire additions (get_art, sources fields)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Final verification

- [ ] **Run the full device-free test suite**

Run: `cargo test --lib`
Expected: PASS — all new tests (`audio::airplay_meta` ×9, `audio::airplay` config/path, `audio::engine` ×3, `server::commands` ×3) plus the pre-existing suite, no regressions.

- [ ] **Build the whole workspace**

Run: `cargo build`
Expected: success, no warnings introduced by this slice (`cargo build 2>&1 | grep -i warning` empty for the touched files).

- [ ] **Demo-gated (Pi only, not CI) — manual end-to-end**

On the Pi (classic `shairport-sync` installed, distro `shairport-sync.service` disabled): run the hub, AirPlay from an iPhone to a zone, play a track with album art. Confirm: the iOS client's `sources` push shows title/artist/album/client and a non-empty `art_id`; a `get_art` with that id returns the image. (No automated assertion — observation only.)

---

## Self-Review Notes

- **Spec coverage:** Slice 3 spec items → tasks: metadata-pipe parse → Task 1; accumulator/commit batching → Task 2; second FIFO + config + reader → Task 3; art cache keyed by hash + `art_id` + track state + clear-on-end → Task 4; reader-into-engine wiring → Task 5; `get_art` task + `art_id`/track fields in push → Task 6; `CLAUDE.md` freeze (`get_art`, fields, `unknown_art`) → Task 7. The `client`=best-effort, one-latest-art-per-source, SHA-256, and unchanged-lifecycle decisions from the spec's "as-planned" note are honored in Global Constraints and Tasks 1/2/4. `unknown_source` is explicitly deferred to Slice 4 (reroute), per the spec.
- **Placeholders:** none — every code step shows complete code; every command has expected output.
- **Type consistency:** `MetaEvent`/`MetaCommit`/`MetaAccumulator` (Tasks 1–2) are consumed verbatim in Tasks 3 and 5; `SessionSink::{track_update,art_update}` signatures (Task 4) match the factory call sites (Task 5) and the `NoSessions` stub; `Engine::get_art -> Option<(String, Vec<u8>)>` (Task 4) matches the `dispatch` destructure (Task 6); `SourceView` fields (Task 4) match `send_sources` (Task 6).
