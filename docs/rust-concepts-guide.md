# Rust Concepts in Audio Share — A Guided Tour

A map of the **Rust-specific ideas** this codebase leans on, where each one lives, *why* it's done that way here, and where to read more. It assumes you already know how to program — it focuses on the things Rust does differently from Go/C++/Java/Python, and on the idioms that recur across this repo.

Read it roughly top-to-bottom: the early sections (ownership, traits, error handling) are the vocabulary the later sections (concurrency, async) are written in.

> Resource shorthand:
> - **[TRPL]** = *The Rust Programming Language* ("the book") — https://doc.rust-lang.org/book/
> - **[Rustonomicon]** = the unsafe/deep book — https://doc.rust-lang.org/nomicon/
> - **[Tokio]** = https://tokio.rs/tokio/tutorial
> - **[std docs]** = https://doc.rust-lang.org/std/
> - **[Rust by Example]** = https://doc.rust-lang.org/rust-by-example/

---

## 0. The big architectural shape (so the rest has context)

There are **three binaries** sharing one Cargo workspace (`Cargo.toml`):

- **the hub** (`src/`, package `audio_share`) — runs on the Pi, talks to the iOS app, decodes audio, drives outputs.
- **the dongle agent** (`crates/dongle_agent`) — runs on a remote speaker, registers with a hub, supervises `snapclient`.
- **the shared protocol** (`crates/protocol`, `audioshare_protocol`) — the wire types both of the above speak, so the contract can't drift.

Two concurrency models coexist and the split is deliberate:

- **`tokio` async** for everything network-facing (TCP listeners, per-connection tasks). Many connections, mostly waiting on I/O → cheap async tasks.
- **OS threads (`std::thread`)** for everything CPU/blocking: audio decode, the cpal output callback, process supervisors. These use blocking libraries (`reqwest::blocking`, Symphonia, cpal) that don't belong on an async runtime.

Keep that division in mind — it explains why you'll see both `tokio::sync::Mutex` *and* `std::sync::Mutex`, both `tokio::spawn` *and* `thread::spawn`.

---

## 1. Ownership, borrowing & lifetimes

The core of Rust. Every value has one owner; borrows (`&T` shared, `&mut T` exclusive) are checked at compile time so you never alias-and-mutate.

**Where to see it bite (and get solved):**

- **Explicit lifetime on a struct** — `src/server/connection.rs:15`
  ```rust
  pub struct Connection<'connection> {
      stream: &'connection mut TcpStream,
      ...
  }
  ```
  `Connection` *borrows* the `TcpStream` rather than owning it — the `'connection` lifetime says "this Connection cannot outlive the stream it points at." This is the one place in the codebase you're forced to name a lifetime, because a struct holds a reference. Contrast with everything else, which owns its data or shares via `Arc`.

- **Releasing a borrow early to avoid holding a lock** — `src/audio/decode.rs:69-80`. The code clones the codec params out of `format` inside a block specifically so "the immutable borrow of `format` is released before we start pulling packets." This is the *scoped borrow* pattern: `{ ... }` ends a borrow at a controlled point.

- **The same trick with a `Mutex` guard** — `src/audio/engine.rs:160-200` (`Engine::play`). It snapshots the zone's outputs inside a `{ let zones = self.zones.lock()...; ... }` block, **drops the lock**, does slow work (spawning snapserver / network round-trips), then re-locks to install the result. Holding a lock across slow/blocking work is the classic deadlock/latency bug; the borrow checker doesn't stop you, but scoping the guard does. Read the comment there — it's a great real example.

- **Explicit `drop()`** — `src/audio/engine.rs:210` (`drop(zones);` before calling `self.snapcast.release_zone`). Sometimes you end a borrow *before* the end of the block on purpose.

**Read:** [TRPL] ch. 4 (Ownership), ch. 10.3 (Lifetimes). The lock-scoping idiom: [std docs] `std::sync::MutexGuard`.

---

## 2. Move semantics, `Copy`, and `Clone`

Assignment moves by default. Types opt into copy-on-assign with `#[derive(Copy)]` (only for cheap, no-heap types) or explicit `.clone()`.

- **A `Copy` type, and why** — `src/session.rs:7` `#[derive(Copy, Clone)] struct Session` holds `[u8; 32]` + an `Instant`, both `Copy`, so the whole struct can be. Note the comment at `src/server/connection_server.rs:70`: `let pairing_secret = server.pairing_secret; // [u8;32] is Copy` — copying a fixed array is fine, no `.clone()` needed.
- **Deliberate `.clone()` of shared state** — `src/server/connection.rs:40` clones `self.security` to hand a `Security` into the session map. `Security` is `#[derive(Clone)]` (`src/security.rs:13`).
- **`move` closures** capture by value — everywhere a thread or task is spawned, e.g. `src/audio/engine.rs:191` `.spawn(move || ...)`. The closure *takes ownership* of `thread_url`, `thread_stop`, `sink` so they outlive the spawning function.

**Read:** [TRPL] ch. 4.1, and the `Clone`/`Copy` traits in [std docs].

---

## 3. Traits — the project's main abstraction tool

Traits are Rust's interfaces. This codebase uses them three ways: as a **seam for polymorphism** (`AudioSink`), as a **mockability boundary** for testing (`SnapcastControl`, `DongleRegistrar`), and to **implement std behaviors** (`Display`, `Error`, `FromStr`, `Drop`).

### 3a. The keystone trait: `AudioSink`
`src/audio/sink.rs:17`
```rust
pub trait AudioSink: Send + Sync {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn write(&self, samples: &[f32]);
}
```
This single trait is the whole multi-room architecture in miniature: the decode pipeline writes PCM into "some sink" without knowing if it's the local speaker or a network stream. Implementors:
- `AudioOutput` (local cpal device) — `src/audio/sink.rs:29`
- `SnapcastSink` (writes to a snapserver FIFO) — `src/audio/snapcast.rs:110`
- `FanOut` (writes to several sinks) — `src/audio/engine.rs:327`
- test doubles `NullSink` / `Counter` — `src/audio/registry.rs:131`, `src/audio/engine.rs:508`

The `: Send + Sync` part is a **supertrait bound**: every sink must be safe to send across threads and share between them (because the decode thread and others touch it). More on Send/Sync in §8.

### 3b. Traits for testability (dependency injection without a framework)
- `SnapcastControl` — `src/audio/snapcast_control.rs:74`. The real impl (`CommandConn`) talks JSON-RPC over TCP; the reconcile logic is written against the *trait*, so the test (`src/audio/snapcast_router.rs:270` `MockControl`) records calls in-memory with no snapserver. This is *why* `reconcile()` (`snapcast_router.rs:103`) takes `control: &dyn SnapcastControl` rather than a concrete type.
- `DongleRegistrar` — `src/server/dongle_server.rs:44`. Production `EngineRegistrar` forwards to the global `ENGINE`; the test `MockRegistrar` (`dongle_server.rs:185`) records register/offline calls so the whole TCP handshake can be tested with no audio hardware.

This pattern — *define a narrow trait, write logic against it, inject a mock in tests* — is the idiomatic Rust answer to "I don't have a DI container." Look for it whenever you see `&dyn SomeTrait` in a function signature.

### 3c. Implementing standard-library traits
- `Display` — `src/errors/connection_error.rs:14`, `crates/dongle_agent/src/storage.rs:46` (lets `HubAddress` be `{}`-formatted as `host:port`).
- `std::error::Error` — `src/errors/connection_error.rs:20` (empty body; the blanket impl just needs `Debug + Display`). This is what lets a `ConnectionError` be boxed into `Box<dyn Error>` (see §5).
- `FromStr` — `crates/dongle_agent/src/storage.rs:52`. Implementing it gives you `.parse::<HubAddress>()` for free, used at `storage.rs:145` (`raw.parse().ok()`) and in arg parsing (`main.rs:125`).
- `Default` — e.g. `src/audio/engine.rs:296`, `registry.rs:118`. Many types provide `Default` so they slot into `#[derive(Default)]` structs and `..Default::default()` (see the test at `snapcast_router.rs:339`).
- `Drop` — its own section, §7.

**Read:** [TRPL] ch. 10.2 (Traits), ch. 17.2 (trait objects). [Rust by Example] "Traits".

---

## 4. Static vs dynamic dispatch: `impl Trait`, `dyn Trait`, `Arc<dyn Trait>`

Two ways to be generic over a trait:

- **Static dispatch / generics / `impl Trait`** — monomorphized, zero-cost, one copy of the code per concrete type.
  - Generic function with a `where` bound: `build_stream<T>(...) where T: SizedSample + FromSample<f32>` — `src/audio/output.rs:183`. cpal devices hand back different sample types; this compiles a specialized version for `f32`/`i16`/`u16` (dispatched at `output.rs:153-157`).
  - `impl Into<String>` parameters — `crates/dongle_agent/src/supervisor.rs:48`, `snapcast.rs:99`. "Accept anything convertible to String." Caller can pass `&str` or `String`.
  - `impl Fn() + Send + 'static` — `src/audio/snapcast_control.rs:173` (`EventListener::spawn` takes a callback). Static dispatch over a closure.

- **Dynamic dispatch / `dyn Trait`** — one pointer + vtable, chosen at runtime, lets a collection hold *mixed* concrete types.
  - `Arc<dyn AudioSink>` — pervasively (`registry.rs:37`, `engine.rs:111`, etc.). The registry stores sinks of different concrete types behind one type. You *must* use `dyn` here because a `Vec`/`HashMap` needs a single element type.
  - `&dyn SnapcastControl` / `Arc<dyn DongleRegistrar>` — the test seams from §3b.
  - `Box<dyn std::error::Error + Send + Sync>` — the error type (see §5).

**Rule of thumb this codebase follows:** generics/`impl Trait` for hot or single-type paths; `dyn` when you need heterogeneity (a registry of different sinks) or runtime swapping (mock vs. real). The `as Arc<dyn AudioSink>` casts you'll see (e.g. `snapcast_router.rs:69`) are *upcasts* from a concrete `Arc<SnapcastSink>` to the trait object.

**Read:** [TRPL] ch. 17.2, and the chapter on generics, ch. 10.1. Also "When to use `dyn` vs generics": https://doc.rust-lang.org/book/ch17-02-trait-objects.html

---

## 5. Error handling: `Result`, `?`, boxed errors, and string errors

No exceptions. Fallible functions return `Result<T, E>`; `?` propagates the `Err` early. The interesting part is the *choice of `E`*, and this repo shows three deliberate strategies:

1. **`Box<dyn std::error::Error + Send + Sync>`** — the "I don't care about the exact type, just bubble it up" error, used at API boundaries. Aliased locally as `type BoxError = ...` (`dongle_server.rs:39`, `registration.rs:33`). The `+ Send + Sync` matters because these cross async tasks/threads. The `?` operator auto-converts any concrete error into this box (via the `From` impl on `Box<dyn Error>`). See `connection.rs:30-56`.

2. **`Result<T, String>`** — cheap, human-readable, used internally where a caller only logs/matches on the message. e.g. `Engine::play -> Result<(), String>` (`engine.rs:160`), and the `.map_err(|e| format!(...))?` idiom everywhere in `snapcast.rs`/`output.rs`. The downside (you can't match on a typed variant) is acceptable here; note how `commands.rs:72-76` matches on the *string* to map engine errors to wire error codes — a small smell the comment owns.

3. **`Result<T, &'static str>`** — zero-allocation error for fixed messages, `src/security.rs:80` (`encrypt_data`/`decrypt_data`). When the message is a compile-time constant, you don't even need a `String`.

**Custom error type:** `ConnectionError` (`src/errors/connection_error.rs`) implements `Debug + Display + Error`, which is the minimum to be boxable. It's the "typed error" option, used sparingly.

**Idioms to recognize:**
- `?` after a `Result` — propagate. After an `Option` it propagates `None` (used in `storage.rs:144` `...ok()?`).
- `.map_err(|e| format!("context: {e}"))?` — add context then propagate. The poor-man's `anyhow::Context`.
- `.ok_or_else(|| "msg".to_string())?` — turn `Option` into `Result` then propagate (`engine.rs:170`).
- `.ok()` — discard the error, turn `Result` into `Option` (`storage.rs:144`).
- `let _ = fallible();` — deliberately ignore a result (e.g. `OUTPUTS_CHANGED.send(())` at `engine.rs:243` — "fire and forget").
- `.expect("msg")` / `.unwrap()` — panic on failure. Used for *invariants that should never break* (a poisoned mutex, `engine.rs:166`) and in tests. Note `engine.rs` uses `.expect("engine zones mutex poisoned")` everywhere — a poisoned lock means another thread panicked while holding it, which is unrecoverable, so panicking is correct.

**Read:** [TRPL] ch. 9 (entire chapter). For the boxed-error and `?`-conversion mechanics: [Rust by Example] "Error handling > Boxing errors".

---

## 6. Enums, pattern matching, and the "make illegal states unrepresentable" style

Rust enums are tagged unions (sum types). This codebase uses them as the primary modeling tool.

- **Protocol messages as enums** — `crates/protocol/src/lib.rs:58-99`. `DongleToHub`, `HubToDongle`, etc. Each is a `#[serde(tag = "type")]` enum so new message kinds extend the wire format without breaking old parsers (forward compatibility). Adding a variant is how the protocol grows.
- **Commands** — `src/server/commands.rs:9` `enum Task { Play, Pause, ..., Unknown(String) }`. Note `Unknown(String)` *carries data* — the rejected task name — so the error response can echo it (`commands.rs:93`).
- **`Option<T>`** is just an enum (`Some`/`None`); the whole codebase models "maybe absent" with it (`current: Option<Pipeline>` at `engine.rs:74`, `sink: Option<Arc<...>>` at `registry.rs:37`).

**Pattern-matching idioms worth internalizing:**
- **`let ... else`** — `engine.rs:137`-style early return. e.g. `let Some(first) = present.first() else { continue };` (`snapcast_router.rs:112`) and `let Some(expected_uuid) = self.client_uuid else { return false; };` (`connection.rs:187`). Bind-or-bail; flattens nesting.
- **Irrefutable enum destructure with `let ... else`** — `dongle_server.rs:120`:
  ```rust
  let DongleToHub::Register { dongle_id, name } = from_line(&line)? else {
      return Err("first message ... was not Register".into());
  };
  ```
  "Parse a line; if it isn't a Register, bail." Very common in the protocol handlers.
- **Match guards** — `commands.rs:62` `Some(url) if !url.is_empty() =>`.
- **Nested result matching** — `connection.rs:82` `Ok(Ok(n))` / `Ok(Err(_))` / `Err(_)` from a `timeout(read())` (a `Result<Result<...>, Elapsed>`).
- **`matches!` macro** — `engine.rs:426` (`!matches!(rx.try_recv(), Err(TryRecvError::Empty))`), `snapcast_control.rs:197`. Boolean "does this match this pattern."

**Read:** [TRPL] ch. 6 (Enums & match), ch. 18 (Patterns). The "let-else" feature: https://doc.rust-lang.org/rust-by-example/flow_control/let_else.html

---

## 7. RAII and the `Drop` trait — cleanup tied to scope

Rust has no GC and no `finally`; instead, when a value goes out of scope its `Drop::drop` runs. This codebase uses `Drop` heavily to make resource cleanup *automatic and exception-safe*.

- **`AudioOutput`** — `src/audio/output.rs:121`. Dropping it flips an `AtomicBool` and joins the audio thread. You can't forget to stop the stream; it stops when the handle dies.
- **`SnapserverSupervisor` / `SnapclientSupervisor`** — `snapcast.rs:238`, `supervisor.rs:81`. Drop kills the child process and joins the monitor thread. This is why `Started` (`snapcast_router.rs:131`) holds `_supervisor` and `_events` — *named with leading underscores because they're never read*, they exist purely so their `Drop` fires when the router shuts down. That's a real pattern: **a field held only for its drop side-effect.**
- **`EventListener`** — `snapcast_control.rs:212`. Drop sets `stop`, then `shutdown()`s the socket to unblock the thread's blocking `read_line`, then joins. Note: you often need to *actively interrupt* a blocked thread before joining it — flipping an atomic isn't enough if the thread is parked in a syscall.
- **`AbortOnDrop`** — `crates/dongle_agent/src/registration.rs:37`. A tiny wrapper whose only job is to `abort()` a spawned tokio task on drop, so the heartbeat task can't outlive the session. The `_heartbeat = AbortOnDrop(...)` binding at `registration.rs:94` keeps it alive exactly as long as the function runs.
- **`Advert`** — `crates/dongle_agent/src/assignment.rs:126`. Drop withdraws the mDNS advert. And see `assignment.rs:181` `pre_exec(PR_SET_PDEATHSIG)` — a *backstop* because `Drop` does **not** run on `SIGKILL`/Ctrl-C; the kernel is asked to kill the child when the parent dies by any means. Good lesson: destructors run on normal scope exit, *not* on process signals.
- **Manual `Pipeline::shutdown(self)`** — `engine.rs:65`. Takes `self` by value (consuming it), signals stop, joins. Taking `self` means it can only be called once — the type system enforces "shut down exactly once."

**Read:** [TRPL] ch. 15.3 (Drop), [Rustonomicon] "Destructors". The classic article "RAII" on the Rust wiki.

---

## 8. `Send`, `Sync`, and the `!Send` problem (the most "Rust" part of this app)

This is the deep-dive section. Read it slowly — almost every weird shape in the audio code falls out of these two traits.

### 8.0 The one-sentence definitions (and what they *really* mean)

- **`Send`** = "a value of this type can be **moved to** another thread." Ownership can cross a thread boundary.
- **`Sync`** = "a value of this type can be **shared with** another thread via a shared reference." Formally: `T: Sync` ⇔ `&T: Send`. If it's safe to hand out `&T` to two threads at once, `T` is `Sync`.

The two are different questions and you need both words because some types answer them differently:

| Type | `Send`? | `Sync`? | Why |
|------|---------|---------|-----|
| `i32`, `[u8; 32]`, `String`, `Vec<f32>` | ✅ | ✅ | Plain data, no shared interior mutability. |
| `Rc<T>` | ❌ | ❌ | Refcount is a non-atomic `usize`; two threads bumping it races. |
| `Arc<T>` (where `T: Send + Sync`) | ✅ | ✅ | Refcount is **atomic**, so sharing/moving the handle is safe. |
| `Mutex<T>` (where `T: Send`) | ✅ | ✅ | The lock makes *interior mutation* safe to share — this is the key one below. |
| `Cell<T>` / `RefCell<T>` | ✅ | ❌ | Mutable through `&` with **no** locking → not safe to share, but fine to move. |
| `MutexGuard<'_, T>` | ❌* | ✅ | The OS lock is often tied to the locking thread, so the *guard* can't move threads. (\*platform-ish, but treat it as `!Send`.) |
| `cpal::Stream` (macOS) | ❌ | — | CoreAudio ties the stream to its creating thread. **This is our whole problem.** |

The headline mental model: **`Send` is about moving ownership across threads; `Sync` is about sharing access across threads. `Sync` is literally defined in terms of `Send` of a reference.**

### 8.1 They are *auto traits* — you almost never write them

`Send`/`Sync` are **auto traits**: the compiler implements them for your type **automatically and structurally** — a struct is `Send` iff *every field* is `Send`, `Sync` iff every field is `Sync`. You don't write `impl Send for Foo`. Instead:

- You get them for free when all your fields have them (the normal case).
- You **lose** them the instant one field doesn't have them. One `Rc` or one `*mut T` field "infects" the whole struct as `!Send`/`!Sync`.
- You can *opt out* explicitly with a `PhantomData<*const ()>` field, or (rarely, `unsafe`) opt back in with `unsafe impl Send for ...`.

This is why the design is shaped by *absence*: you can't argue with the compiler about whether `cpal::Stream` is `Send`. It isn't, structurally, and that fact propagates into anything that tries to hold it.

### 8.2 Where the bound is *required*: `thread::spawn` and `tokio::spawn`

The reason any of this matters is that the spawn functions demand it. Roughly:

```rust
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,   // ← the closure (and everything it captures) must be Send
    T: Send + 'static;                   //   and what it returns must be Send
```

So when `output.rs:71-73` does:

```rust
let thread_buffer = Arc::clone(&buffer);     // Arc<Mutex<VecDeque<f32>>> — Send + Sync ✅
let thread_running = Arc::clone(&running);   // Arc<AtomicBool>           — Send + Sync ✅
thread::Builder::new()
    .name("audio-output".to_string())
    .spawn(move || run_audio_thread(thread_buffer, thread_running, init_tx))  // move closure
```

…the `move` closure captures `thread_buffer`, `thread_running`, `init_tx`. For the closure to be `Send`, **every captured value must be `Send`.** They all are. That compiles.

Now notice what is **not** captured: the `cpal::Stream`. It can't be — it's `!Send`, so a closure that captured it would itself be `!Send` and `spawn` would refuse it. That constraint is what forces the next part.

### 8.3 The defining problem: `cpal::Stream` is `!Send`

Read the module comment at `output.rs:8-11`. On macOS the CoreAudio stream **cannot leave the thread it was created on**. That single fact dictates the entire shape of `AudioOutput`. You cannot:

- store the `Stream` in a struct that gets moved to another thread,
- hold it in an async task (tokio may move tasks between worker threads),
- return it from the thread that built it.

So the design **pins the `!Send` thing to one thread and never moves it**:

1. **Create and own the stream entirely inside `run_audio_thread`** (`output.rs:137-163`). It's built there, played there, and dropped there (`drop(stream)` at `:173`). It never crosses a thread boundary, so its `!Send`-ness never matters.
2. **Keep that thread alive deliberately** with a `while running.load(...) { sleep }` park loop (`:170-172`). The thread exists *only* to be the stream's home. If the thread ended, the stream would drop and audio would stop.
3. **Communicate with it through `Send + Sync` channels only.** Two wires cross the thread boundary, and *both* are `Send`:
   - **Audio data in:** `SampleBuffer = Arc<Mutex<VecDeque<f32>>>` (`output.rs:46`). `write()` (`:101`) locks and pushes on the producer thread; the cpal callback (`:205`) locks and drains on the audio thread. The `Arc<Mutex<…>>` *is* `Send + Sync` even though it guards data both threads mutate.
   - **Init status back:** a one-shot `mpsc::channel` (`output.rs:67`). The audio thread sends `Ok((rate, channels))` or `Err(msg)` (`:169`/`:176`) so `AudioOutput::new` can *synchronously* learn whether the device opened (`:77`), even though the device lives on another thread it can't touch.

```
   producer thread                         audio-output thread (owns the !Send Stream)
   ┌──────────────┐   Arc<Mutex<VecDeque>>   ┌─────────────────────────────────────────┐
   │ write(&[f32])│ ───────push──────────▶   │ cpal callback: lock, drain → device     │
   └──────────────┘                          │ Stream lives & dies here, never moves   │
        ▲                                    └─────────────────────────────────────────┘
        │            mpsc::channel  (init result: Ok(rate,ch) / Err)
        └──────────────────◀──────────────────────────┘
```

**This is the canonical Rust move:** *when a resource is `!Send`, pin it to one thread and talk to it through a `Send` channel/shared buffer.* If you understand *why* `AudioOutput` is built this way — that the buffer and the channel are the only things allowed across the boundary because they're the only `Send` things — you understand a huge amount of Rust.

### 8.4 How `Arc<Mutex<T>>` *manufactures* `Send + Sync`

A bare `VecDeque<f32>` is `Send` but **not safely shareable** between two threads that both mutate it — you'd have a data race, which is exactly what Rust forbids. Wrapping it:

- **`Mutex<VecDeque<f32>>`** adds the lock, making concurrent access *sound*. `Mutex<T>: Sync` whenever `T: Send`. So now sharing `&Mutex<…>` across threads is allowed.
- **`Arc<…>`** gives shared *ownership* (atomic refcount) so both threads can hold a handle and the buffer lives until the last handle drops.

Together, `Arc<Mutex<VecDeque<f32>>>` is `Send + Sync`, which is precisely the property `spawn` demanded in §8.2. The wrappers don't just "add locking" — they *change the auto-trait answer* from "unsafe to share" to `Sync`. That's the lever the whole design pulls.

(Contrast: `Arc<RefCell<…>>` would be `!Sync`, because `RefCell` is `!Sync` and `Arc` can't fix that — `Arc` only adds shared ownership, not thread-safe mutation. The compiler would reject sending it into the callback. The `Mutex` is load-bearing.)

### 8.5 The supertrait bound: forcing implementors to be thread-safe

```rust
pub trait AudioSink: Send + Sync { ... }      // sink.rs:17
pub trait SnapcastControl: Send + Sync { ... } // snapcast_control.rs:74
```

`Send + Sync` here are **supertrait bounds** — every implementor of `AudioSink` *must* also be `Send + Sync`. Why force it? Because the engine stores sinks as `Arc<dyn AudioSink>` and hands them to the decode thread (and `FanOut` shares one across several). For `Arc<dyn AudioSink>` to itself be `Send + Sync` (so it can be moved into a `decode-{zone}` thread), the trait object must promise `Send + Sync`. Putting it in the trait definition makes that a compile-time guarantee at *every* implementor: `AudioOutput`, `SnapcastSink`, `FanOut`, and the test doubles all had to satisfy it, or they wouldn't compile as `AudioSink`s. The bound is how the architecture says "every output is thread-safe, no exceptions" once, instead of re-proving it at each call site.

### 8.6 Things that trip people up

- **`Send` without `Sync`:** `Cell`/`RefCell` — fine to *move* to another thread, not fine to *share*. Single-threaded mutation, no lock.
- **`Sync` without `Send`:** `MutexGuard` — safe to reference from elsewhere, but tied to its locking thread so it can't move. This is *why* `std::sync::Mutex` guards must not be held across an `.await` (the task could resume on a different thread) — see §9.
- **`'static` is a separate requirement.** `spawn` wants `Send + 'static`. `'static` means "holds no borrowed references with a shorter life" — that's why the closures `move` owned `Arc`s in rather than borrowing locals. `Send` and `'static` are *both* needed and people conflate them.
- **You rarely write `unsafe impl Send`.** If you reach for it, you're usually promising the compiler something it couldn't verify (e.g., "this raw pointer is only ever touched by one thread"). This repo never needs to — it gets `Send + Sync` structurally, the safe way, by choosing `Arc`/`Mutex`/atomics.

**Read:** [Rustonomicon] "Send and Sync" (the definitive treatment of auto traits). [TRPL] ch. 16.4 (`Sync`/`Send`). [std docs] `std::marker::Send` / `std::marker::Sync`.

---

### 8.7 Check yourself — interactive quiz

Work through these *before* expanding the answer. They're ordered easy → hard and all reference the real code above. (On GitHub the `▸ Answer` toggles expand; in a plain terminal just scroll — the answer follows each question.)

**Q1.** In one sentence each, what do `Send` and `Sync` mean, and what's the formal relationship between them?

<details><summary>▸ Answer</summary>

`Send` = the value can be **moved** to another thread. `Sync` = the value can be **shared** (`&T`) across threads. The relationship: `T: Sync` **if and only if** `&T: Send`. ("Sharing a `T` is the same as sending a reference to it.")
</details>

**Q2.** `Send`/`Sync` are *auto traits*. What does that mean for a struct with three fields, and how would adding an `Rc<String>` field change things?

<details><summary>▸ Answer</summary>

The compiler implements them **structurally**: the struct is `Send` iff all three fields are `Send`, and `Sync` iff all three are `Sync` — no `impl` written by you. Adding an `Rc<String>` field makes the whole struct **`!Send` and `!Sync`**, because `Rc`'s refcount is non-atomic. One bad field infects the whole type.
</details>

**Q3.** Why can't `run_audio_thread`'s `cpal::Stream` simply be stored as a field of `AudioOutput` and used from `write()`?

<details><summary>▸ Answer</summary>

`cpal::Stream` is `!Send` on macOS — it's bound to its creating thread. If `AudioOutput` held it as a field, `AudioOutput` would become `!Send`, and you couldn't move it into tasks/threads or share it. More directly: `write()` is called from the producer/decode thread, but the stream may only be touched from the audio thread that created it. So the stream is *pinned* to `run_audio_thread` and never stored on the struct.
</details>

**Q4.** Exactly two things cross the boundary between the producer thread and the audio thread. Name them, and state the auto-trait property each must have and why.

<details><summary>▸ Answer</summary>

1. `SampleBuffer = Arc<Mutex<VecDeque<f32>>>` (audio data in) — must be **`Send + Sync`** because both threads hold a handle and mutate the contents through it.
2. `mpsc::Sender<Result<(u32,u16), String>>` (init status back) — must be **`Send`** so it can be moved into the spawned closure; the payload `(u32,u16)`/`String` must be `Send` to travel across.

Both are `Send`, which is the only reason the `move` closure passed to `spawn` is itself `Send`.
</details>

**Q5.** `VecDeque<f32>` is already `Send`. So why isn't it enough on its own — why the `Arc<Mutex<…>>` wrapping?

<details><summary>▸ Answer</summary>

`Send` only says it's safe to *move ownership* to one other thread. Here **two** threads need to mutate the *same* deque concurrently (producer pushing, callback draining). A bare `VecDeque` shared that way is a data race. `Mutex` makes concurrent mutation sound and makes the type `Sync` (`Mutex<T>: Sync` when `T: Send`); `Arc` gives both threads shared ownership with an atomic refcount. Together they turn "unsafe to share" into `Send + Sync`.
</details>

**Q6.** Would `Arc<RefCell<VecDeque<f32>>>` work instead of `Arc<Mutex<…>>` for the shared buffer? Why or why not?

<details><summary>▸ Answer</summary>

No. `RefCell` is **`!Sync`**, and `Arc` cannot add `Sync` — it only adds shared *ownership*, not thread-safe *mutation*. So `Arc<RefCell<…>>` is `!Sync`, the closure capturing it would be `!Send`, and `spawn` (or `build_output_stream`'s `Send` callback bound) would reject it at compile time. The `Mutex` is load-bearing precisely because it's the thing that provides `Sync`.
</details>

**Q7.** Why is `Send + Sync` written into the *trait definition* `pub trait AudioSink: Send + Sync` rather than as a bound at each use site?

<details><summary>▸ Answer</summary>

Because the engine stores sinks as `Arc<dyn AudioSink>` and moves them into per-zone decode threads (and `FanOut` shares one across threads). For `Arc<dyn AudioSink>` to be `Send + Sync`, the underlying trait object must *guarantee* `Send + Sync` — and a `dyn Trait` only carries the auto-trait bounds named in the trait itself. Writing it as a supertrait makes every implementor (`AudioOutput`, `SnapcastSink`, `FanOut`, test doubles) prove thread-safety **once, at the definition**, instead of re-stating `where S: AudioSink + Send + Sync` at every call site. It encodes "every output is thread-safe, no exceptions" into the type.
</details>

**Q8.** `thread::spawn` requires `F: Send + 'static`. The closures in `output.rs` `move` owned `Arc`s in rather than borrowing the locals. Which bound forces that — `Send` or `'static` — and what would borrowing instead violate?

<details><summary>▸ Answer</summary>

`'static` forces it. Borrowing a local (`&buffer`) would make the closure hold a reference whose lifetime is shorter than `'static`, and the spawned thread could outlive that local — a use-after-free. Moving owned `Arc` clones in means the closure owns its captures (no borrowed references), satisfying `'static`. (`Send` is *also* required, and the `Arc<Mutex<…>>` satisfies that — but `Send` and `'static` are two separate requirements people often conflate.)
</details>

**Q9.** Give one type that is `Send` but not `Sync`, and one that is `Sync` but not `Send`. For the second, name a concrete rule in *this* codebase that it explains.

<details><summary>▸ Answer</summary>

- `Send` but not `Sync`: `Cell<T>` / `RefCell<T>` — fine to move to one thread, but unsynchronized interior mutation makes sharing unsound.
- `Sync` but not `Send`: `MutexGuard<'_, T>` — safe to reference, but tied to the locking thread so it can't move. This is exactly *why a `std::sync::Mutex` guard must not be held across an `.await`* (§9): a tokio task can resume on a different worker thread, which would require the guard to move — and it can't.
</details>

**Q10. (synthesis)** A teammate proposes "simplifying" `AudioOutput` by storing the `cpal::Stream` directly in the struct and deleting the dedicated audio thread, calling `stream` methods from `write()`. In auto-trait terms, walk through exactly what breaks.

<details><summary>▸ Answer</summary>

1. A field of type `cpal::Stream` makes `AudioOutput` structurally **`!Send`** (one non-`Send` field infects the struct).
2. That alone breaks any code that moves an `AudioOutput` into a thread/task or stores it as `Arc<dyn AudioSink>` (the trait requires `Send + Sync`) — it won't compile.
3. Even ignoring the type error: `write()` is called from the decode thread, but the macOS stream may only be touched on its creating thread, so calling stream methods from `write()` is the exact unsoundness `!Send` exists to prevent.

The dedicated thread + `Arc<Mutex<buffer>>` + `mpsc` init channel isn't incidental complexity; it's the *only* shape that keeps the `!Send` resource pinned while still letting other threads feed it. That's the whole lesson of §8.
</details>

---

## 9. Shared mutable state: `Arc`, `Mutex`, `RwLock`, atomics, interior mutability

Rust's rule "either many readers or one writer" extends to threads via these wrappers. **`Arc`** = atomically reference-counted shared ownership. **`Mutex`/atomics** = the *interior mutability* that lets you mutate through a shared `&` reference.

- **`Arc<Mutex<HashMap<...>>>`** — `src/server/server.rs:11` (the session store). Shared across all connection tasks; locked per access (`server.rs:49`). Classic shared map.
- **`Arc::clone` before `spawn`** — everywhere (`server.rs:27`, `connection_server.rs:62`, `dongle_server.rs:95`). Each task gets its own owning handle to the same data; the data lives until the last `Arc` drops. Note `Arc::clone(&x)` is preferred over `x.clone()` stylistically — it signals "cheap refcount bump, not a deep copy."
- **`std::sync::Mutex` vs `tokio::sync::Mutex`** — a real, important distinction:
  - `tokio::sync::Mutex` (`server.rs:4`) — async, its guard can be held across `.await`. Used where the lock guards data touched by async code.
  - `std::sync::Mutex` (`engine.rs:22`, `registry.rs:21`, `snapcast*.rs`) — synchronous, *must not* be held across `.await`, but is faster and used in the thread-based audio code. The engine is reached from sync threads, so it uses the std mutex.
  This is a frequent beginner trap; this codebase picks correctly per context.
- **Atomics for flags** — `AtomicBool` as a cooperative stop signal: `engine.rs:58` (`stop: Arc<AtomicBool>`), `output.rs:65`, `snapcast.rs:194`, every supervisor. The pattern: producer thread polls `stop.load(Ordering::Relaxed)`; owner sets `stop.store(true, ...)` from another thread. No lock needed for a single bool.
- **`AtomicU64` for a counter** — `snapcast_control.rs:88` (`next_id`), incremented with `fetch_add` (`:111`) to generate JSON-RPC request ids without a lock.
- **`Mutex<Option<Child>>`** — `snapcast.rs:197`, `supervisor.rs:32`. The `Option` lets `Drop` *take* the child out (`.take()`) so it can be killed exactly once even while a monitor thread also races for it.
- **`Ordering::Relaxed`** — used for all the flags/counters here because none of them guard *other* memory that needs ordering; they're standalone signals. (If you were publishing data via the flag you'd need `Acquire`/`Release`.)

**A subtle one worth studying:** `SnapcastSink::write` (`snapcast.rs:119`) locks `Mutex<Inner>` and does **disjoint field borrows** — it mutates `inner.scratch` and `inner.writer` separately (`:133-147`). The comment at `:136` calls it out. The borrow checker allows borrowing different fields of the same struct mutably at once, which is why this compiles.

**Read:** [TRPL] ch. 16 (Fearless Concurrency) — esp. 16.3 (`Arc`/`Mutex`). [std docs] `std::sync::atomic` (the module docs explain `Ordering` well). Tokio's "Shared state" tutorial page for the two-Mutex distinction.

---

## 10. Threads, channels, and the supervisor pattern

The blocking/CPU side uses raw OS threads.

- **Named threads** — `thread::Builder::new().name("audio-output").spawn(...)` (`output.rs:71`, and `decode-{zone}` at `engine.rs:189`). Naming threads makes panics and debuggers legible. Note `spawn` here returns a `Result` (unlike `thread::spawn`), handled with `?`/`.map_err`.
- **`JoinHandle` and `.join()`** — every spawned worker is stored and joined on shutdown (e.g. `engine.rs:68`, `output.rs:124`). Joining waits for the thread to finish and propagates its panic.
- **`std::sync::mpsc::channel`** — `output.rs:67`. A one-shot handshake: the audio thread sends back `Ok((rate, channels))` or `Err(msg)` so `AudioOutput::new` can *synchronously* learn whether the device opened, even though the device lives on another thread. Channels are how `!Send`/thread-isolated code reports results.
- **The supervisor pattern** (recurs 3×: `SnapserverSupervisor`, `SnapclientSupervisor`, and the cpal thread): spawn a child (thread or process) → a monitor loop `wait()`s on it → relaunch on exit unless a `stop` flag is set → `Drop` tears it down. Study `monitor_loop` in `snapcast.rs:260` and `supervisor.rs:101` side by side — same skeleton.

**Read:** [TRPL] ch. 16.1–16.2 (threads & channels). [std docs] `std::thread`, `std::sync::mpsc`.

---

## 11. Async / `tokio` — the network side

`async fn` returns a *future* that does nothing until polled; `tokio` is the runtime that polls them. `.await` yields control while waiting on I/O.

- **`#[tokio::main]`** — `src/main.rs:24`, `crates/dongle_agent/src/main.rs:42`. Macro that sets up the runtime and runs an async `main`.
- **`tokio::spawn`** — spawns a concurrent task (green thread, not OS thread). The accept loops spawn one task per connection: `connection_server.rs:64`, `dongle_server.rs:96`. Each task gets a cloned `Arc<Self>`.
- **`tokio::try_join!`** — `server.rs:45`. Run the four servers concurrently, fail if any fails.
- **The accept loop** — `listener.accept().await` in a `loop` (`connection_server.rs:59`, `dongle_server.rs:92`, `assignment.rs:60`). The fundamental server shape.
- **`tokio::select!`** — `connection.rs:69`. Race two async operations: "either a client message arrives, or the outputs-changed broadcast fires." Whichever is ready first wins; the other is cancelled. This is how one task multiplexes input and a push channel. **Caveat the code respects:** the `read_line` heartbeat loops *don't* use `select!` to cancel reads, because `read_line` isn't cancellation-safe — see the comment at `registration.rs:93`. Instead they wrap reads in `timeout(...)`. Knowing which futures are cancel-safe is real async expertise; this codebase documents it.
- **`tokio::time::timeout`** — `connection.rs:71`, `registration.rs:67`, `dongle_server.rs:139`. Bound a read so a dead/half-open peer is detected. Returns `Result<inner, Elapsed>`, hence the nested matching.
- **`tokio::time::interval`** — `registration.rs:122`, the heartbeat ticker. Note `.tick().await` and the comment that the *first* tick fires immediately.
- **Split a socket** — `stream.into_split()` → `(OwnedReadHalf, OwnedWriteHalf)` (`registration.rs:52`, `dongle_server.rs:112`, `assignment.rs:83`). Lets the read loop and the heartbeat-writer task own their halves independently. `BufReader` wraps the read half for line-buffered `read_line` (`registration.rs:53`).
- **Async traits via `tokio::io`** — `AsyncReadExt`, `AsyncWriteExt`, `AsyncBufReadExt` are imported to bring `.read()`, `.write_all()`, `.read_line()` into scope on the async streams (`connection.rs:10`, `registration.rs:21`).

**Mental model to carry:** an `async fn` is a state machine the runtime drives; `.await` is a yield point; `select!`/`timeout` compose futures. The split between this section and §10 is the single most important design decision in the repo.

**Read:** [Tokio] tutorial (do the whole thing — Hello Tokio → Select → Channels). [TRPL] ch. 17 (async, in newer editions). On cancellation safety: the `tokio::select!` macro docs.

---

## 12. Serde — (de)serialization by derive

`serde` + `serde_json` turn Rust types ↔ JSON with `#[derive(Serialize, Deserialize)]`.

- **Derives + attributes** — `crates/protocol/src/lib.rs:56` (`#[derive(... Serialize, Deserialize)] #[serde(tag = "type")]`). The `tag = "type"` makes each enum variant serialize as `{"type":"Register", ...}` — an *internally tagged* representation. The test at `lib.rs:172` pins the exact wire bytes, which is how you keep a cross-language contract (the iOS app hand-mirrors these).
- **`#[serde(skip_serializing_if = "Option::is_none")]`** — `src/json_structs/task_response.rs:13`. Omit `data`/`error` from the JSON when absent, so the wire stays clean.
- **Typed vs untyped JSON** — two styles coexist:
  - *Typed*: parse straight into a struct/enum (`from_line` → `DongleToHub`, `lib.rs:109`).
  - *Untyped*: `serde_json::Value` + index access, used on the iOS-facing path where the message shape is looser: `connection.rs:105` parses to `Value`, then `request["task"].as_str()` (`connection.rs:140`), `data["url"].as_str()` (`commands.rs:62`). The `json!{...}` macro builds `Value`s (`connection.rs:165`).
- **Custom `to_json` trait** — `src/json_structs/json_trait.rs` + `task_response.rs:42`. A thin app-specific trait wrapping `serde_json::to_string`. (Slightly redundant with serde, but shows trait-based serialization.)

**Read:** https://serde.rs/ (esp. "Enum representations" for `tag`). [Rust by Example] doesn't cover serde; serde.rs is the source of truth.

---

## 13. Closures and `Fn`/`FnMut`/`FnOnce`

Closures capture environment; their trait says how.

- **`FnMut` closure holding per-call state** — `output.rs:199-217`. The cpal callback is `FnMut` (called repeatedly, mutates captured `primed`). The comment at `:198` is gold: "the callback is FnMut, so this per-stream state lives in the closure (no atomics needed)." A closure is a struct with captured fields; mutating a captured `bool` across calls is free.
- **`impl Fn() + Send + 'static` callback** — `snapcast_control.rs:173`. `EventListener` takes a closure to call on each snapserver event; `'static` because it outlives the spawning call (runs on a thread).
- **`move` closures into threads/tasks** — see §2.
- **Closures in iterator adapters** — `engine.rs:165` (`.map(|(zone, name, online)| json!{...})`), `registry.rs:112` (`.map(|o| (o.id.clone(), ...))`).
- **An immediately-invoked closure for error scoping** — `output.rs:137` `let opened = (|| -> Result<...> { ... })();`. Runs a fallible block with `?` inside and captures the `Result`, so the surrounding thread body can `match` on success/failure cleanly. A neat trick for "use `?` where the function itself can't return `Result`."

**Read:** [TRPL] ch. 13.1 (closures), ch. 13.2 (iterators).

---

## 14. Iterators — lazy, composable, zero-cost

Idiomatic Rust replaces most loops with iterator chains.

- `engine.rs:131-133` — `.iter().any(...)`, `.iter().filter(...).cloned().collect()`.
- `engine.rs:220-236` — `list_targets` builds the target list with `filter`/`collect`/`sort_by`/`extend`/`with_capacity`.
- `snapcast_router.rs:103-118` — the whole `reconcile` is iterator-driven (`filter`, `cloned`, `collect`, `first`).
- `decode.rs:251-265` — `mix_planar` uses `zip`, `iter_mut`, and a `(0..out_channels).map(...).collect()` to build channels.
- `snapcast.rs:356` — `out.chunks_exact(2).map(...).collect()` to decode bytes back to `i16` in a test.

Recognize the rhythm: `source.iter()` → adapters (`map`/`filter`/`zip`/`take`) → consumer (`collect`/`any`/`find`/`sum`/`for_each`). It compiles to the same code as a hand-written loop.

**Read:** [TRPL] ch. 13.2; [std docs] `std::iter::Iterator` (skim the method list once — it pays off forever).

---

## 15. Generics on data + the `lazy_static` globals

- **`lazy_static!`** — `server.rs:90` (`MAIN_SERVER`), `engine.rs:41` (`ENGINE`, `OUTPUTS_CHANGED`). Rust has no runtime-initialized `static` by default; this macro defers init to first access. Used here for *process-wide singletons* so handlers reach them without threading a handle through every call. The comment at `engine.rs:14-18` explains the trade-off (and that constructing `ENGINE` deliberately touches no hardware so it's safe as a global). Note the modern alternative is `std::sync::OnceLock` / `once_cell`, but `lazy_static` is what's here.
- **Type aliases** — `type ZoneId = String` (`engine.rs:55`), `type OutputId = String` (`registry.rs:27`), `type SampleBuffer = Arc<Mutex<VecDeque<f32>>>` (`output.rs:46`), `type BoxError = ...`. Aliases give intent to primitive/compound types without new-typing them.

**Read:** the `lazy_static` crate docs; [std docs] `std::sync::OnceLock` for the modern equivalent.

---

## 16. The module system, workspace, and visibility

- **Workspace** — root `Cargo.toml` declares `[workspace] members = ["crates/protocol", "crates/dongle_agent"]` and depends on `audioshare_protocol` by `path`. This is how three binaries share one type crate. Read the comment block in `Cargo.toml`.
- **Module tree** — `mod` declarations in `main.rs:4-13` and nested (`src/server/mod.rs`, `src/audio/mod.rs`). A `mod foo;` pulls in `foo.rs` or `foo/mod.rs`.
- **`pub`, `pub(crate)`, path `use`** — `use crate::audio::engine::ENGINE` etc. Items are private by default; `pub` exposes them. Note `pub use`/re-exports aren't heavily used here.
- **`#[allow(dead_code)]`** — `registry.rs:18`, `output.rs:18`, `engine.rs:308`. Suppresses warnings for code wired in by a *later* build step. The codebase is honest about staged work.
- **`#[cfg_attr(not(test), allow(dead_code))]`** — `storage.rs:102`. Conditional attribute: the `Storage::at` constructor is "dead" except in tests.

**Read:** [TRPL] ch. 7 (modules & packages), ch. 14.3 (workspaces).

---

## 17. Conditional compilation (`#[cfg(...)]`) — one codebase, many targets

This project runs on macOS (dev) and Linux/Pi (prod), and `cfg` carves per-OS paths at compile time.

- **Per-OS function bodies** — `connection_server.rs:132` vs `:137` (`get_serial_number` reads `/proc/cpuinfo` only on Linux; returns a stub on macOS — this is the "macOS caveat" from CLAUDE.md, solved with `cfg`). Same shape in `pairing.rs:14`/`:24` and `:66`/`:100` (QR to Preview on mac, Unicode to terminal on Pi).
- **Per-OS *implementations of the same abstraction*** — `assignment.rs` is the masterclass: the `Advert` enum has a `Mdns` variant on non-Linux and an `Avahi(Child)` variant on Linux (`:116-123`), with `#[cfg]` on imports, the `Drop` impl, and three `advertise_*` functions. The comment (`:104-115`) explains *why* (avahi owns mDNS on the Pi). Read this file to see how Rust keeps a portable seam while the guts diverge per platform.
- **`#[cfg(test)]`** — every `mod tests` (e.g. `engine.rs:341`). Test code compiles only under `cargo test`, so it adds zero weight to the shipped binary.

**Read:** https://doc.rust-lang.org/reference/conditional-compilation.html ; [Rust by Example] "Attributes > cfg".

---

## 18. Testing idioms (this repo tests *well* — learn from it)

- **Inline `#[cfg(test)] mod tests`** — co-located with the code; tests can reach private items via `use super::*` (`engine.rs:343`).
- **`#[test]` vs `#[tokio::test]`** — sync tests vs async tests (`dongle_server.rs:206` uses `#[tokio::test]` to get a runtime).
- **`#[ignore]` for hardware/network tests** — `output.rs:268`, `decode.rs:328`, `engine.rs:483`, `snapcast.rs:540`. These need real audio/network so they're opt-in (`cargo test -- --ignored`). The doc comment on each says exactly how to run it. Good model: *the smoke test exists and is documented, but never blocks CI.*
- **Mocks via traits** — `MockControl` (`snapcast_router.rs:265`), `MockRegistrar` (`dongle_server.rs:185`), `NullSink`/`Counter`. See §3b.
- **Loopback TCP tests** — `snapcast_control.rs:228`, `dongle_server.rs:207`, `assignment.rs:238`. Bind `127.0.0.1:0` (OS picks a free port), spawn a server task, connect a client, assert the wire exchange. This is how the protocol is tested with zero hardware.
- **Pure functions extracted for testability** — `fill_output` (`output.rs:229`), `next_backoff` (`main.rs:111`), `mix_planar`/`interleave` (`decode.rs`), `to_i16le` (`snapcast.rs:170`), `parse_server_status`. The pattern: pull the logic out of the I/O so it can be unit-tested without the I/O. The comments explicitly call this out.
- **A genuinely advanced concurrency test** — `snapcast.rs:431` `write_applies_backpressure_once_reader_present`. It `mkfifo`s a pipe, coordinates a reader and writer thread with two `Barrier`s, and asserts no bytes are dropped. Read it to see FIFO open-ordering, `libc` fcntl, and barrier synchronization in one place.

**Read:** [TRPL] ch. 11 (testing). For async tests: tokio's `#[tokio::test]` docs.

---

## 19. A little `unsafe` and FFI (`libc`) — used surgically

Rust's safety is the default; `unsafe` is a small, audited escape hatch. This repo touches it in exactly two well-commented spots:

- **`libc::mkfifo` / `fcntl`** in the backpressure test — `snapcast.rs:443`, `:483`. Raw syscalls have no safe wrapper here, so they're called in `unsafe { }`.
- **`Command::pre_exec` with `prctl(PR_SET_PDEATHSIG)`** — `assignment.rs:181`. The `SAFETY:` comment (`:179`) justifies it: the closure runs in the forked child before `exec` and only calls async-signal-safe functions. **Note the convention: every `unsafe` block gets a `// SAFETY:` comment explaining why it's sound.** That's the cultural norm; follow it.

**Read:** [Rustonomicon] (the whole thing is about this), and [TRPL] ch. 20.1 (unsafe Rust).

---

## 20. Smaller idioms you'll see repeatedly

- **`format!` / `{var}` inline captures** — `format!("decode-{zone}")` (`engine.rs:190`), `format!("snapserver error: {err}")`. Modern Rust interpolates locals directly in format strings.
- **`.to_string()` / `.to_vec()` / `.cloned()` / `.into()`** — explicit conversions; Rust never implicitly copies heap data.
- **`Vec::with_capacity`** — `engine.rs:229`, `decode.rs:276`, `snapcast.rs:171` — preallocate when the size is known, to avoid reallocs.
- **Builder-ish constructors** — `TaskResponse::accepted` / `::error` (`task_response.rs:21`,`:32`) instead of exposing fields. Encapsulation via associated functions.
- **`self: Arc<Self>` receiver** — `server.rs:21`, `connection_server.rs:53`, `dongle_server.rs:82`. A method that requires being called on an `Arc` (so it can clone itself into spawned tasks). Unusual receiver type worth recognizing.
- **Slices & `split_at`** — `security.rs:113` (`decoded_data.split_at(12)` to separate nonce from ciphertext), `&buf[..n]` everywhere reading sockets.
- **`?` on `try_into()` for fixed arrays** — `security.rs:41`, `pairing.rs:33` (`Vec<u8>` → `[u8; 32]`, fails if wrong length). How you safely go from a runtime-length buffer to a compile-time-sized key.

---

## Suggested reading order for *this* codebase

1. `src/audio/sink.rs` — the one trait that explains the architecture (§3a, §8).
2. `src/audio/output.rs` — ownership, `!Send`, threads, channels, `Drop`, generics, closures all in one file (§1, §7, §8, §10, §13).
3. `src/server/connection.rs` + `connection_server.rs` — async, lifetimes, `select!`, error boxing (§1, §5, §11).
4. `crates/protocol/src/lib.rs` — enums + serde as a wire contract (§6, §12).
5. `src/audio/engine.rs` — `Arc`/`Mutex` discipline, lock scoping, the global, `dyn` sinks (§1, §4, §9, §15).
6. `crates/dongle_agent/src/registration.rs` + `assignment.rs` — async sessions, `Drop`-based task cleanup, `cfg` per-OS, a touch of `unsafe` (§7, §11, §17, §19).
7. `src/audio/snapcast*.rs` — the supervisor pattern, trait-based mocking, the advanced FIFO/backpressure test (§3b, §10, §18).

By the time you've read those seven with this guide open, you'll have hit every major Rust concept the project uses.

---

## Top external resources (ranked)

1. **The Book ([TRPL])** — read ch. 4, 6, 9, 10, 13, 15, 16, 17. That's ~80% of what's here.
2. **[Tokio tutorial]** — the async half of the app maps directly onto it.
3. **[Rust by Example]** — when you want a runnable snippet for a single concept.
4. **[std docs]** — `Arc`, `Mutex`, `atomic`, `mpsc`, `thread`, `Option`, `Result`, `Iterator`. Searchable, authoritative.
5. **[Rustonomicon]** — once you want to understand `Send`/`Sync`/`unsafe` deeply.
6. **serde.rs** — the serialization model.
7. *Rust for Rustaceans* (Jon Gjengset, book) and his YouTube channel — excellent once the basics click; "Crust of Rust" videos cover lifetimes, `dyn`, async, atomics at exactly this codebase's level.
```
