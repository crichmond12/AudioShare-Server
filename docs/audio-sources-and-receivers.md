# Audio Share — How Audio Reaches the Speaker (Source & Receiver Strategy)

> Living design doc for the **input side** of Audio Share: every avenue by which
> audio can get *into* the hub, with pros/cons and a recommended stack. This is
> the companion to `docs/multi-room-plan.md` (which covers the **transport** side
> — hub → dongles → speakers). Read both together. Cross-check against
> `CLAUDE.md` (source of truth for protocol/state) and update all three when
> reality changes.

---

## The one-paragraph answer

Lead with **AirPlay 2 receive (shairport-sync)** as the universal "play anything
from your iPhone" path — it delivers Spotify, Apple Music, YouTube, podcasts, and
every other iOS app at once, with the lowest licensing exposure (the *phone*
decrypts; we are literally a speaker). Keep the **server-side DRM-free fetch**
sources (internet radio — done — plus podcasts and self-hosted libraries) as the
legal, fully-owned backbone. Add **Spotify Connect (librespot)** only as an
opt-in, user-installed plugin for people who specifically want Connect semantics
(phone can leave, device pulls directly). Treat **Chromecast, official on-device
Spotify, DLNA, and Roon** as dead ends for an indie self-hosted product, and
**Bluetooth A2DP** as a nice universal fallback (especially for Android). The key
enabler: **Snapcast natively runs shairport-sync and librespot as stream
sources**, so receivers plug into the multi-room sync you're already building.

---

## Framing — two fundamental models (from the pivot)

Every avenue is one of two things. This distinction is the whole legal strategy.

1. **Server-side fetch** — the hub holds the encoded bytes and plays them itself.
   Legal **only** for DRM-free / open / user-self-hosted sources. (Internet
   radio, podcasts, Subsonic/Jellyfin/Plex, phone-relayed local files.)
2. **Receiver** — the phone's *own* app streams to us; decryption/licensing
   happens on the phone or via the service's own protocol. We are "a speaker."
   (AirPlay 2, Spotify Connect, Bluetooth, Cast.)

What we **never** do: capture raw audio from a DRM service server-side
(Widevine/EME only decrypts in the player), or use a sanctioned device SDK that
forbids combining with other services (Spotify eSDK). Those killed the original
"streaming aggregator" vision; see `CLAUDE.md`.

---

## The architectural insight that drives the recommendation

The engine already splits **"produce PCM"** (`audio/decode.rs`) from
**"consume PCM"** (`audio/sink.rs` `AudioSink`). A *receiver protocol is just
another PCM producer* — parallel to "HTTP URL → Symphonia decode," it's
"shairport-sync → PCM → engine." Nothing about zones, the registry, or Snapcast
needs to know which producer it is.

And **Snapcast already implements this for us.** `snapserver` supports source
types beyond `pipe`/`process`, including:

```
[stream]
source = airplay:///shairport-sync?name=AirPlay&devicename=Audio%20Share
source = librespot:///librespot?name=Spotify&devicename=Audio%20Share&...
source = pipe:///tmp/audioshare?name=Internal&codec=pcm   # our own decode output
```

`snapserver` supervises the helper binary, ingests its audio, and distributes it
**time-synced to every snapclient** — i.e. through the exact multi-room path the
roadmap already adopts in Phase 3. So once Snapcast is in:

- Our own decode output is one Snapcast source (a pipe).
- AirPlay is another (snapserver runs shairport-sync).
- Spotify Connect is another (snapserver runs librespot).

Adding a receiver becomes **"declare a stream + supervise/supply a binary + map
it to our zone model in the app,"** not "build a new audio subsystem." This is
why receivers are cheap *after* Phase 3 and why they should not be hand-rolled
before it.

### Discipline: the hub is ONE receiver, not many

AirPlay 2 and Spotify Connect each have their *own* multi-speaker grouping. If we
exposed every dongle as its own AirPlay/Connect target, the user could group via
Apple/Spotify and bypass our hub — two grouping systems fighting, exactly the
failure mode `multi-room-plan.md` warns about with Snapcast. So:

> **Expose the *hub* as a single AirPlay/Connect receiver. The stream it receives
> is an input source; OUR zone model + Snapcast decide where it plays.** Apple/
> Spotify see one speaker. Our app picks the zone(s). The hub's zone registry
> stays the single source of truth.

---

## Comparison table (the pros/cons at a glance)

| Avenue | Mechanism | Catalog reach | Licensing posture | iOS / Android | Multi-room fit | Effort | Verdict |
|---|---|---|---|---|---|---|---|
| **AirPlay 2 receive** | `shairport-sync` (Snapcast `airplay` source) | **Everything on iOS** (app-agnostic) | Low — phone decrypts; FOSS-standard, audio-only widely tolerated | iOS/Mac only | Excellent (1 hub target → Snapcast) | Med | ✅ **Lead receiver** |
| **Server-side DRM-free fetch** | our `decode.rs` (radio done; podcasts, Subsonic/Jellyfin/Plex next) | Open + self-hosted only | **Clean — fully legal core** | Both (our app) | Native (it's our engine) | Med (per source) | ✅ **Backbone** |
| **Phone-relay of local files** | our app → encrypted TCP → decode | User's own files only | Clean | Both (our app) | Native | Low–Med | ✅ Ship (modest reach) |
| **Spotify Connect** | `librespot` (Snapcast `librespot` source) | Spotify only (Premium) | **Gray/red — violates Spotify dev ToS** | Both (Spotify app) | Good (1 hub target → Snapcast) | Med | ⚠️ **Opt-in plugin only, never bundled** |
| **Bluetooth A2DP sink** | BlueZ + bluez-alsa → Snapcast source | Everything (system audio) | Clean (it's a BT speaker) | **Both** | Weak (point-to-point; can feed Snapcast) | Med (BlueZ is fiddly) | ◑ Optional fallback (esp. Android) |
| **Chromecast receive** | — | Cast apps / Android | Closed — needs Google cert | Android-centric | n/a | Very high / infeasible | ❌ Dead end (no open receiver) |
| **Official on-device Spotify** | eSDK / Web Playback SDK | Spotify | eSDK approved-orgs-only; Web SDK is browser-only | — | — | n/a | ❌ Dead end (the pivot's whole point) |
| **DLNA / UPnP renderer** | gmediarender / upmpdcli | Local libraries, some Android | Clean (open standard) | **iOS doesn't speak it** | OK | Med | ❌ Skip (misses our iOS-first user) |
| **Roon endpoint (RAAT)** | — | Roon users (audiophile niche) | Proprietary, partner-only | via Roon | — | High | ❌ Skip (tiny audience, closed) |
| **Loopback / line-in capture** | host audio capture | Whatever's playing on a PC | ToS-gray, single-zone | n/a | Poor | Low | ◔ Last resort only |

Legend: ✅ build · ⚠️ build but quarantined · ◑/◔ optional/low · ❌ don't.

---

## Per-avenue detail

### ✅ AirPlay 2 receive — `shairport-sync` (lead receiver)

**What it is.** `shairport-sync` is the mature, de-facto-standard open-source
AirPlay (1 & 2) audio receiver. It advertises via mDNS/Bonjour; any iPhone/iPad/
Mac then sees "Audio Share" in the AirPlay menu (Control Center, any app's
AirPlay button, system audio). The phone's app does the streaming and the
DRM decryption; shairport-sync receives audio and emits PCM. (AirPlay 2 receive
needs the companion `nqptp` timing daemon.)

**Why it's the single best receiver to lead with:**
- **One integration → every iOS app.** Apple Music, Spotify, YouTube, podcasts,
  browser audio — all AirPlay because it's at the OS layer. We get "play my
  Spotify/Apple Music to the speaker" **without touching either service's
  licensing**, because the phone's own app streams and decrypts. This is the
  highest-leverage feature in the entire product.
- **Lowest legal exposure of any receiver.** We are a speaker; the phone holds
  the license. Audio-only AirPlay receive via shairport-sync is shipped by
  Volumio, moOde, and Home Assistant — the accepted self-hosted posture.
- **Best demo.** "Open Apple Music, tap AirPlay, pick the speaker" is instantly
  legible — ideal for the portfolio video.
- **Snapcast integrates it** as an `airplay` stream source → rides Phase 3 sync.
- Mature, actively maintained, robust buffering/sync in-protocol.

**Cons / watch-outs:**
- **External C process** to supervise — same pattern as `snapserver`, not new,
  but it's another non-Rust dependency + `nqptp`.
- **iOS/Mac only.** No Android. Fine for an iOS-first product; note the gap.
- **Phone is the continuous source** — it streams the whole time, so it must stay
  in range and not be killed (background audio is OK). Contrast Spotify Connect,
  where the phone hands off and can leave. This is the main functional reason to
  *also* offer Connect.
- **MFi gray for commercial.** Real AirPlay 2 licensing (MFi) is for manufactured
  products; shairport-sync is reverse-engineered. As **user-installed FOSS** this
  is the standard tolerated path, but it argues for shipping it as a clearly
  separable component, not welding it into a sold binary.
- **Don't expose per-dongle AirPlay targets** (see discipline above) — one hub
  target, distribute internally.

**Posture:** core feature, but kept as a separable/supervised component. Default-
installable (unlike librespot) because audio-only AirPlay receive is broadly
accepted.

### ✅ Server-side DRM-free fetch (the legal backbone)

**What it is.** The hub fetches and plays sources it's legally allowed to hold:
internet radio (**done, Phase 1**), podcasts (RSS), and self-hosted libraries the
user owns (Subsonic/Navidrome, Jellyfin, Plex). Flows through `decode.rs` →
`AudioSink` → zones → Snapcast, identical to radio today.

**Pros:** clean legal core; **fully ours** (great engineering showcase: real
HTTP/decode/resample pipeline); phone is just a remote so it can leave; native
multi-room; an enormous amount of real content exists (podcasts especially).

**Cons:** no mainstream streaming *catalog* (no Spotify/Apple Music library);
self-hosted-library users are a minority; each source is its own integration
(auth, API, browse UI in the app).

**Posture:** ships first and keeps shipping (roadmap Phase 5). This is what makes
us legally a media player in our own right, independent of any receiver.

### ✅ Phone-relay of local files (ours, low licensing)

**What it is.** Our iOS app reads a file the user legitimately has (Files app,
downloaded podcast, voice memo, AirDrop) and streams its bytes over our existing
encrypted TCP/session channel; the hub decodes and plays it like any stream.

**Important limit:** iOS sandboxing means our app **cannot capture another app's
audio** (Spotify/Apple Music output is off-limits). Relay only works for files
the app itself can read. So this never solves "play my Spotify playlist" — that's
what AirPlay is for.

**Pros:** uses our own handshake/protocol end-to-end (cleanest architecture,
nice portfolio point); zero third-party licensing; native multi-room.

**Cons:** limited content (few users keep large local libraries); phone must stay
connected; modest demo value next to AirPlay.

**Posture:** worth building (it's the `relay-from-phone` play-mode already in the
vision), but it's a complement, not the headline.

### ⚠️ Spotify Connect — `librespot` (opt-in plugin ONLY)

**What it is.** `librespot` is an open-source reimplementation of the Spotify
Connect (speaker) protocol. It advertises via Spotify zeroconf; a **Premium** user
picks "Audio Share" in their Spotify app and plays to it. Crucially, librespot
then **pulls the stream from Spotify directly and decrypts it on the device** —
the phone is only a remote. (It's also written in Rust and usable as a crate,
which is tempting — resist embedding it; see below.)

**What it adds over AirPlay** (the only reasons to bother):
- Phone can leave / screen off — device keeps playing (Connect hands off).
- Gapless and avoids AirPlay's re-encode; controllable from Spotify on *any*
  device, including desktop.

**Cons / why it's quarantined:**
- **Violates Spotify's developer ToS** — reverse-engineered, not the sanctioned
  eSDK. Tolerated for personal use, not sanctioned. Periodically breaks when
  Spotify changes the protocol; login now requires OAuth/zeroconf tokens.
- **Spotify only**, Premium-gated.
- Because decode happens **on our device** (not on the phone), embedding librespot
  into our shipped binary would pull gray-area code into *our* product. **Do not.**

**Posture — strict:** ship as an **optional, user-installed plugin, never
bundled** (the Volumio/moOde model). The user installs it, so responsibility sits
with the user. Integrate via Snapcast's `librespot` source so it rides the same
multi-room path — but the binary is supplied by the user, not us. This is a
harder line than AirPlay precisely because the device pulls/decrypts and the ToS
violation is more pointed.

### ◑ Bluetooth A2DP sink (universal fallback, esp. Android)

**What it is.** The Pi acts as a Bluetooth speaker (BlueZ + bluez-alsa/PipeWire).
Any phone pairs and streams system audio.

**Pros:** truly universal — **iOS and Android**, any app, no companion app needed;
fully legitimate (it's a Bluetooth speaker); simplest possible mental model;
works without WiFi. Can be fed into Snapcast as a source to reach multi-room.

**Cons:** Bluetooth range/quality (SBC/AAC); fundamentally **point-to-point and
single-source** (undercuts the networked multi-room story unless bridged into
Snapcast); pairing friction (one device at a time); **BlueZ A2DP on the Pi is
notoriously fiddly** and onboard BT shares the antenna with WiFi (interference).

**Posture:** good optional "it just works" path and the realistic **Android**
answer until/unless there's an Android companion app. Low priority; bridge into
Snapcast if built.

### ❌ Chromecast / Google Cast receive

No production-grade open-source Cast *receiver* exists; Google gates the receiver
framework behind certification and the audio path is closed. Would capture Android
+ Cast apps, but it's effectively infeasible for self-hosting. **Dead end** — note
and move on.

### ❌ Official on-device Spotify (eSDK / Web Playback SDK)

The sanctioned paths don't fit an embedded indie device: **eSDK** is
approved-organizations-only and contractually forbids combining with other
services; the **Web Playback SDK** uses EME/Widevine and only runs in a desktop
browser, not headless on a Pi. This is the exact wall the pivot was a response to.
librespot is the only practical Spotify path, and it's gray → plugin. **Dead end.**

### ❌ DLNA / UPnP renderer

Open standard, no licensing issue — but **iOS has never spoken DLNA/UPnP**, and our
companion app + primary user is iOS. It mostly serves local-library push, which we
already cover server-side. **Skip** (revisit only if Android-first ever matters).

### ❌ Roon endpoint (RAAT)

Proprietary, requires a Roon partnership and a paying Roon Core; audiophile niche.
(Roon *can* target AirPlay/Chromecast/Squeezebox endpoints — so AirPlay support
incidentally reaches Roon users anyway.) **Skip** as a first-class target.

### ◔ Loopback / line-in capture

Capture whatever is playing on a desktop's audio device. ToS-gray, single-zone,
poor multi-room fit, and not phone-centric. **Last resort only**, for API-less
platforms — already flagged as such in `CLAUDE.md`.

---

## Recommended stack & sequencing

Anchored to the existing roadmap in `CLAUDE.md` / `multi-room-plan.md`:

1. **Backbone first (Phases 1–2, in progress).** Server-side DRM-free fetch:
   internet radio (done) → podcasts → Subsonic/Jellyfin/Plex; plus phone-relay of
   local files. Independent multi-room (zones/registry). This is the legal,
   fully-owned core and needs no receiver.
2. **Snapcast (Phase 3).** Synchronized groups. **Land this before receivers** —
   it's the substrate that makes receivers cheap (they become snapserver sources).
3. **AirPlay 2 receive (Phase 4, lead receiver).** `shairport-sync` as a
   snapserver `airplay` source; hub is one AirPlay target; our app maps it to
   zones. This is the universal "play anything from my iPhone" unlock and the best
   demo. Highest priority receiver.
4. **Spotify Connect (Phase 4, opt-in plugin).** `librespot` as a snapserver
   `librespot` source, **user-installed, never bundled.** Adds Connect semantics
   (phone can leave) for users who want it.
5. **Bluetooth A2DP (optional, anytime).** Universal fallback / the Android
   answer; bridge into Snapcast if built.

Everything else (Cast, official Spotify, DLNA, Roon, loopback) is explicitly **not
on the roadmap** and recorded here so the decision isn't relitigated.

### Why AirPlay over librespot as the lead

| | AirPlay (shairport-sync) | Spotify Connect (librespot) |
|---|---|---|
| Catalog | **Every iOS app at once** | Spotify only |
| Licensing | Low (phone decrypts; we're a speaker) | **Gray/red (device pulls + decrypts; ToS)** |
| Bundling | Default-installable | **Must be user-installed plugin** |
| Demo legibility | Highest | Spotify-users only |
| Phone can leave | No (continuous source) | **Yes (hands off)** |

AirPlay delivers the most value at the least legal cost and is app-agnostic.
librespot's *only* genuine edge is "phone can leave / desktop control," which is
worth offering — but as a quarantined plugin, not the headline.

---

## Disciplines to not violate (carry-overs + new)

1. **Hub is one receiver, not many.** Single AirPlay/Connect target; our zone
   model + Snapcast own grouping. Never expose per-dongle AirPlay/Connect targets.
   (Same spirit as the Snapcast "single source of truth" rule in
   `multi-room-plan.md`.)
2. **Receivers are PCM *sources* behind the existing seam.** They feed the engine/
   Snapcast like any other producer; decode/zone/registry code stays receiver-
   agnostic. Don't special-case them deeper than the source boundary.
3. **Gray-area code is user-installed, never bundled.** AirPlay (audio-only, FOSS
   standard) may be default-installable; **librespot is opt-in plugin only** —
   the device pulls/decrypts there, so responsibility must sit with the user.
4. **Supervise helpers, don't reimplement.** shairport-sync/librespot/nqptp are
   supervised external processes (the `snapserver` pattern) — we never reimplement
   AirPlay/Spotify/clock-sync ourselves.

---

## Open decisions (not yet locked)

- **UX: how an AirPlay/Connect stream maps to zones.** Options: (a) the active
  receiver stream shows up in our app as a selectable source the user routes to a
  zone; (b) it auto-plays to a default/last zone on activation. Likely (a).
- **Android strategy.** iOS-first means AirPlay covers the universal case; Android
  universal-push realistically means Bluetooth (now) or an Android companion-app
  relay (later). Decide if/when Android is a goal.
- **Packaging of shairport-sync.** Bundled-but-separable in the flashable image vs.
  first-run optional install. (librespot is settled: opt-in only.)
- **Snapcast source ↔ zone model wiring.** Mechanics of mapping snapserver streams/
  groups to our zone registry for receiver sources — detail for Phase 3/4, related
  to the JSON-RPC orchestration in `multi-room-plan.md` Change 5.

---

## Cross-references

- `docs/multi-room-plan.md` — transport side (hub → dongles), Snapcast, the
  `AudioSink` seam these sources feed into.
- `CLAUDE.md` — product vision, the two play-modes, wire protocol, current state.
- Roadmap: receivers are **Phase 4**; they depend on Snapcast (**Phase 3**).
