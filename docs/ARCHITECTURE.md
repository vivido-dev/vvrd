# vvrd — Architecture

`vvrd` (Vivido Reader) is a terminal PDF, EPUB, Markdown, and Mermaid reader for the **Vivido**
terminal. MuPDF renders PDF/EPUB; the native markup backend produces fixed portrait Letter pages.
All formats display through the **Vivid Protocol 1.5** side channel, using the reusable
[`vivid_sdk`](../../vivid_sdk) producer client on top of
[`vivid_protocol`](../../vivid_protocol).

It is the Vivid-native counterpart of [`kitpdf`](../../kitpdf), which targets the Kitty graphics
protocol. **The goal is full feature parity with `kitpdf`** (see the parity matrix below), delivered
through Vivid mechanisms instead of Kitty escape sequences.

This document describes the overall architecture. The step-by-step build order lives in
[`IMPLEMENTATION_PLAN.md`](./IMPLEMENTATION_PLAN.md).

---

## 1. Context: where vvrd sits

```
        ┌──────────── Vivido terminal (presenter) ─────────────┐
        │  GPU compositor · scene graph · media decode         │
        │                                                      │
  PTY   │   text + zero-width anchor marker (allowed on PTY)    │
  ◀─────┼──────────────────────────────────────────────────────┤
        │                                                      │
  Vivid │   control connection  (VIVID_ENDPOINT_CONTROL)        │
  side  │   track channels      (VIVID_ENDPOINT_BULK)           │  ◀── page images ride here,
  chan. │   proof-authenticated from VIVID_ROOT_SECRET          │      NEVER on the PTY
        └──────────────────────────────────────────────────────┘
                         ▲
                         │  vvrd is a Vivid *producer*
                 ┌───────┴────────┐
                 │      vvrd       │   document render · TUI · vivid_sdk
                 └────────────────┘
```

Vivido launches its child shell with the `VIVID_ENDPOINT_*` lane endpoints and `VIVID_ROOT_SECRET`
set (`vivido/src/window_context.rs`). The user then runs `vvrd file.pdf` like any command. `vvrd`:

- reads the endpoints and root secret from the environment (never logs or forwards them, and the
  root secret is never sent on the wire — only a transcript-bound proof of it),
- opens an authenticated control session and one or more authenticated track channels,
- keeps **all image bytes off the PTY** — the PTY carries only ordinary terminal text (status bar,
  overlays) and at most a bounded, authenticated anchor marker,
- places its page image as a **scene node** the presenter composites into the window.

The same binary works unchanged through `vvmux` and over `vvssh` (remote forwarding). `vvmux` is a
**virtual presenter**: it terminates the Vivid session for the pane, then bridges the pane's surfaces
and scene nodes up to the outer presenter (Vivido), translating **pane-local** coordinates into the
composed grid and clipping them to the pane. vvrd needs **no vvmux-specific code** — it always speaks
the ordinary producer contract to whatever presenter set the endpoints. Because vvrd must run in
`vvmux` (including a floating pane, where `vivi` already works), this is treated as a first-class
target — see [§9](#9-running-inside-vvmux-nested-presenter).

---

## 2. kitpdf → vvrd: the fundamental shift

| Concern | kitpdf (Kitty) | vvrd (Vivid) |
|---|---|---|
| Image transport | Escape sequences / SHM **in-band on the PTY** | Vivid **media connection** (side channel), bytes off the PTY |
| Protocol handshake | Query/response over stdin/stdout | `HELLO`/`WELCOME` + profile negotiation and an authentication proof via `vivid_sdk` |
| "Show an image" | `TransmitAndDisplay` / `Display` Kitty action | `CREATE_SURFACE` → `CREATE_TRACK` → `CHANNEL_OPEN` → `RASTER_FRAME` → `ACTIVATE_TRACK` + `CREATE_NODE` |
| Placement | Cursor `MoveTo` + Kitty display location (source-rect crop) | Scene node in **grid-cell 32.32 coordinates**, presenter `contain`-fits |
| Image identity | Stable Kitty image id per page; re-display; delete under pressure | Persistent viewport **surface**; frames ride a replaceable raster **track** |
| Reading terminal replies | stdin is shared by keypresses **and** Kitty responses (must disambiguate) | stdin is **pure user input**; all presenter I/O is on Vivid sockets |
| Flow control | None (terminal buffers) | **Cumulative channel maxima**, track-scoped, enforced by the SDK |
| Async model | tokio event loop + blocking render thread | **Synchronous, thread-based** (the SDK is blocking) |
| Teardown | Delete Kitty images | `DELETE_NODE` + `DESTROY_TRACK` + `DESTROY_SURFACE` (a leaked node is a ghost image) |

Everything MuPDF-related (page rasterization, search, TOC, metadata, links, invert/tint/rotate,
auto-crop, EPUB reflow, export) ports over almost verbatim. Everything Kitty-specific
(`kitty.rs`, the SHM path, the stdin-response parser, tmux passthrough placeholders) is **replaced**
by a thin Vivid presenter layer built on `vivid_sdk`.

---

## 3. Core design decisions

### D1 — Build on `vivid_sdk`, not raw `vivid_protocol`
`vivid_sdk::Session` owns the authentication transcript, the split full-duplex control dispatcher,
heartbeat/`PING`/`PONG`, reply correlation, cumulative channel flow, scene transactions, text
anchors, and authenticated track channels. vvrd consumes it exactly as `vivi` does. vvrd depends on
`vivid_protocol` only for shared types (`registry`, `TrackConfiguration`, `RasterDeltaOperation`,
`cbor::Value`).

The 1.5 object model is the load-bearing part of the design:

| Object | vvrd instance | Lifetime |
|---|---|---|
| Surface | one document surface (`generic-content-v1`, desktop logical pixels) | the whole process |
| Scene node | one terminal grid-cell node placing that surface | created once, updated, deleted at exit |
| Track | one live raster track in slot 3 (`raster`), immutable dimensions | replaced on settled resize or track loss |
| Track channel | one authenticated generation per track attachment | advanced on transport failure |

A track is immutable, so *any* geometry change means a replacement track — but the surface, its
descriptor, its policy, and its scene placement never change identity. That is the property the
whole recovery design rests on.

### D2 — Synchronous, multi-threaded runtime (no tokio)
The SDK is blocking (std threads + condvars + blocking socket I/O). vvrd drops kitpdf's tokio
runtime and uses plain `std::thread` supervisors and bounded mailboxes, with
`crossterm::event::poll(timeout)` driving the UI timers (loading-indicator delay, resize debounce).
This matches the SDK's nature and removes an async/blocking impedance mismatch.

### D3 — Display primitive: the **viewport framebuffer** raster track
vvrd's presenter layer exposes one job behind a `Presenter` seam: *"make the terminal show this
composited view."* The implementation is a **stable surface with one active raster track sized to
the drawable pixel area** (`cols × (rows-1)` cells). Whenever the visible view changes (page turn,
scroll, zoom, pan, rotate, invert, tint, crop, search highlight) vvrd composites the exact visible
region into a viewport-sized RGBA buffer and sends it as the next `RASTER_FRAME`. The scene node is
created once, full-viewport, and only touched on resize or overlay show/hide.

Why this model is the primary path:

- **Reuses kitpdf's pixel math wholesale.** `renderer.rs` (MuPDF → pixmap, invert/tint/rotate,
  search quads) and `image_pipeline.rs` (auto-crop, highlight compositing, scaling) carry over; the
  final step changes from "emit a Kitty image" to "blit into the viewport buffer."
- **Deletes an entire bug class.** kitpdf's `compute_page_surface` juggles Kitty source-rects,
  centering, and placement. With a framebuffer, scroll/zoom/pan is a plain crop-and-scale into a
  buffer the presenter draws 1:1 (`contain`-fit of a same-aspect buffer never letterboxes).
- **Viewport-sized transport with bounded rendering.** A raster track is admissible only if `72 + w·h·4` fits the presenter
  ceiling (~16.7 M pixels raw). The viewport must fit that transport ceiling. Page rendering also obeys the 16.7 MP budget;
  extreme zoom reduces raster scale uniformly before cropping the viewport region.
- **Single bounded presenter resource** with retained delta composition and full-frame recovery.
- **Zstd is per-track.** Raster zstd is a field of the immutable raster track configuration, not a
  session feature. vvrd probes it together with raster deltas, prefers zstd when the presenter can
  grant only one enhancement, and uses the SDK's adaptive sends so compressed bytes are selected
  only when they are smaller than the raw frame or delta.

Raster deltas are requested in the track configuration and granted by `TRACK_READY`, which returns
the **effective** operation limit; vvrd plans against that granted value, never the requested one.
The Vivid thread retains the last submitted viewport. Scroll and pan become an overlap-safe copy plus
overwrite rectangles for newly exposed strips. Other changes become a tight overwrite bounding the
changed pixels. Vivid 1.5 has no `send_raster_delta_or_full`, so vvrd owns the choice: it bounds the
delta body and sends a full frame whenever the delta would not be cheaper. Vvrd forces a full frame
for a new track, after a channel-generation advance, after `NEED_FULL_FRAME`, and whenever
accumulated damage since the last full frame would exceed one half of the viewport.

The representative 800×460 viewport with a 60-pixel vertical scroll step is 1,472,096 bytes as a
raw full frame and 192,112 bytes as a raw copy-plus-strip delta, an 86.95% reduction. Status text is
outside the raster source, the terminal cursor is hidden, and full-screen overlays hide the node;
those changes correctly generate zero raster bytes rather than artificial overwrite rectangles.

### D4 — Full-screen alt-screen + grid-cell node; text overlays hide the node
vvrd is a full-screen TUI: it enters the alternate screen, hides the cursor, and reserves the last
row for a status bar. Because it owns its whole surface, it uses **grid-cell coordinates**
(`COORDINATE_GRID_CELL`, no text anchor) to place the page node at **pane-local** grid `(0,0)–(cols,
rows-1)`, where `cols`/`rows` come from vvrd's own PTY size. Under a bare Vivido presenter the pane
*is* the window, so local `(0,0)` == window origin. Under `vvmux`, the virtual presenter offsets
those pane-local coordinates by the pane's position in the composed grid and clips them to the pane
(verified in `vvmux/src/session.rs::project_logical_node`) — so the identical code works in a tiled
or floating pane with no vvrd changes (see [§9](#9-running-inside-vvmux-nested-presenter)). This also
sidesteps the entire anchor lifecycle (`ANCHOR_READY` waits, cursor coupling, tmux restrictions) that
`vivi`'s inline-image path uses.

Critical compositing fact: Vivid media lives at `TEXT_LAYER_BETWEEN_BACKGROUND_AND_GLYPH` — the
**only** legal layer — so **terminal glyphs draw on top of media** (confirmed in the spec §7.7 and
`vivido/src/display`). Consequences:

- The **status bar** (last row) and any overlay text render *over* the image automatically — fine
  for the status line, since the node covers only the page rows.
- **Full-screen text overlays** (TOC, metadata, links, help, go-to-page, search input) must first
  **hide the page node** (`UPDATE_NODE` visible=false, or delete it) so no stale image shows behind
  the text; re-show on exit. This is the direct analog of kitpdf's `clear_visible_image` before
  drawing overlays.
- vvrd must keep glyphs out of the page area while an image shows (mirrors kitpdf clearing the page
  area). "Loading…" text is drawn only when no node is visible.

### D5 — Supervised rendering processes and shared CPU pixmaps
A supervised subprocess owns the MuPDF `Document` or native Markdown/Mermaid plan. It serves
opening, rendering, search, metadata, links, transactional reload, and export over private pipes.
No native document operation runs on the UI thread. The subprocess caches at most 24 pages and
256 MiB of immutable `Arc<PageImage>` pixels; hits share allocations. Neighbour prefetch runs only
after the request completion fence and when no command is pending.

EPUB counting and outlines use a second disposable subprocess after the first page is returned.
A newer layout request or shutdown kills a superseded paginator. Open/render deadlines are 30 s;
Office opening, reload, search, export, conversion and pagination have 120 s deadlines. Preflight
has the same bounded helper policy. A failed worker keeps the last displayed page and restarts
only after a new request; it cannot enter an automatic crash loop.

Worker IPC has a 12-byte little-endian header (u32 JSON metadata length, u64 raw RGB length), an
8 MiB metadata ceiling and a 50.1 MB pixel ceiling. Documents are limited to 100,000 pages and
outline depth to 256; terminal indentation is additionally clipped to the visible cell width.
Geometry, exact tight stride, payload length,
request generation and document revision are validated before publication. Raw pixels use only
private worker pipes and the Vivid media lane, never the terminal PTY. Process groups on Unix and
process-tree termination on Windows include conversion descendants in cancellation.

PDF and markup rasterization both enforce 16,384 pixels per axis and 16.7 MP before allocation.
PDF scale is reduced uniformly with rounding headroom. Search geometry uses the full rendering
matrix and pixmap origin. The Vivid compositor borrows page pixels, retains one prepared
crop/highlight/scale result, reuses viewport and delta buffers, and compares translated rows
without allocating a reference framebuffer. Identical frames send no media except when a full
recovery frame is required.

### D5a — vvrd owns MuPDF's system-font lookups
MuPDF asks for a substitute font whenever it meets a character no loaded face covers, and it only
remembers the answer when one is found. A script with no installed font therefore repeats the same
failing lookup **once per character**. The `mupdf` crate's `system-fonts` feature answers each repeat
by building a fresh `font-kit` `SystemSource`, so every one of them enumerates the whole system font
collection over a synchronous CoreText/fontconfig round trip.

That made laying out a single chapter of a Chinese EPUB take seconds and counting the whole book take
**ten minutes**, of which ~99.9% was font enumeration — and the Chinese text never rendered, because
no face was ever found. `src/mupdf_fonts.rs` replaces that path with one `fontdb` database plus a
memo of every outcome, misses included, keyed by name / CJK ordering / (script, language). The
`system-fonts` feature is off so this loader is the only one in the chain: a remembered miss has to
be able to end the lookup, and a `None` from a user loader otherwise falls through to the built-in.

The same 2 MB book now counts in ~0.3 s instead of 592 s, forty sequential page loads take 43 ms
instead of stalling 2–7 s at every chapter boundary, and CJK glyphs render. Keep the memo negative:
caching only hits would leave the pathological case exactly as slow as before.

### D10 — Fixed Letter markup backend and transactional reload

Extension dispatch is case-insensitive: `.md`, `.markdown`, and `.mkd` select Markdown; `.mmd` and
`.mermaid` select standalone Mermaid; `.pptx`, `.docx`, `.odp`, and `.odt` select the office
conversion step, which runs a bounded headless LibreOffice (`office.rs`) to produce a PDF cached
by source content hash and then opens that PDF through the unchanged MuPDF path; everything else
follows the unchanged MuPDF path directly. Markup
layout is independent of terminal dimensions: every logical page is 2040×2640 (portrait Letter at
240 DPI, swapped to 2640×2040 by `-l`/`--landscape`) with 180-pixel margins. Normal presentation
contain-fits that page into the viewport; zoom mode crops a bounded higher-resolution copy. The
same flag widens the MuPDF reflow layout for HTML/EPUB documents.

Markdown is parsed by Comrak with the GFM table, strikethrough, task-list, and autolink extensions.
The arena AST is copied into an owned block IR. Pagination keeps blocks together when possible,
keeps a heading with its successor, splits oversized prose/code/lists/tables at line/item/row
boundaries, repeats table headers, and contain-fits images and Mermaid rather than slicing them.
Local raster, SVG, and base64 data-URI assets are supported; remote URLs become placeholders and
are never fetched. Standalone Mermaid must pass preflight; an invalid fenced diagram becomes an
in-page error block.

Pagination also creates heading slugs/outline entries, page-scoped text and links, Mermaid label
semantics, and search geometry. Source is capped at 16 MiB, encoded assets at 32 MiB, visuals at
16,384 pixels per axis and 16.7 MP, blocks at 100,000, and pages at 10,000.

Interactive `.md`, `.markdown`, `.mkd`, `.docx`, and `.pptx` views watch the opened source's parent
directory non-recursively. Source notifications are bounded and debounced for 300 ms so editor
write bursts and atomic replacements produce one reload; symlink paths and their canonical targets
are both covered. A temporarily removed source is ignored until a later create event. `R`/F5
remains the manual reload path for every backend and for linked Markdown assets.

Automatic and manual reloads construct a complete replacement document before swapping it into
the document worker. Success clears page rasters, clamps the current page, rereads local assets, and
advances the document content revision. Failure reports an error while the previous plan and
visible frame remain active. The revision is included in `PresentCmd::UpdateContent`, so a
same-page reload updates the surface descriptor without changing the surface generation, node,
track, or channel. Watcher failures are nonfatal and leave the manual reload path available.

### D6 — Input is pure PTY stdin
Running under Vivido, vvrd is an ordinary terminal app from the PTY's view. Keyboard/mouse come
through `crossterm` in raw mode exactly as kitpdf — but simpler, because stdin carries **no** graphics
responses to disambiguate. All presenter traffic is on Vivid sockets, serviced by SDK background
threads.

### D7 — Profile negotiation and graceful fallback
Vivid 1.5 negotiates **coherent named profiles**, not feature IDs, so the 1.1 per-feature fallback
matrix collapses to two checks (§6): the presenter must select `terminal-surface-v1`, and
`observability-v1` is used only when accepted. Per-track capabilities (raster delta, zstd) are probed
with `PROBE_TRACK_CONFIG` and fall back to a plainer track configuration on rejection. If the
terminal is not Vivido (no `VIVID_ENDPOINT_CONTROL`), vvrd exits with a clear message, or runs in
`--dry-run`/`--trace` for development, just like `vivi`.

### D8 — Deterministic, bounded teardown
Because a placed node persists in the presenter independent of PTY text, exit/panic paths **must**
`DELETE_NODE`, `DESTROY_TRACK`, and `DESTROY_SURFACE`, in that order. A shared shutdown/`Drop` path attempts this with a two-second grace period on normal exit, `q`/`Ctrl-C`, error, and panic — otherwise a ghost image
lingers in Vivido.

The track transport must **outlive** its ordered `DESTROY_TRACK`. A relay that observes the media
connection reach EOF first removes the track and then rejects the destroy, which is the exact failure
the 1.1 resize regression caught. Both the replacement and teardown paths therefore answer the destroy
before dropping the channel, and `resize_and_teardown_destroy_tracks_before_closing_their_transports`
proves it against a live fake presenter.

### D9 — Three distinct recoveries
Vivid 1.1 had one hammer: replace the source. 1.5 separates failures by what actually broke, and vvrd
implements each separately.

| Trigger | Response | Preserved |
|---|---|---|
| `NEED_FULL_FRAME` / `NEED_KEYFRAME` | next submission is a full frame | channel, track, surface |
| Track transport failure (send fails, channel reports an error) | `ADVANCE_CHANNEL`, reopen, full frame | track, surface, node, media-ID space |
| `TRACK_LOST` | create + prime a replacement track, `ACTIVATE_TRACK` slot 3, destroy the lost track | surface, node, descriptor, policy |

`TRACK_LOST` is matched by the **complete** `(context, surface, track)` tuple. Another owner may
legitimately reuse the same numeric track ID, and
`two_owners_reusing_object_numbers_stay_isolated_through_track_loss` proves one owner's loss and
recovery leaves the other's surface, node, generation, and next frame untouched.

Media IDs belong to the *track*, not the channel, so a channel advance keeps the frame counter
climbing; only a replacement track restarts it.

---

## 4. Runtime structure (threads & data flow)

The UI, render supervisor, and Vivid worker communicate through bounded mailboxes:

```text
UI ── RenderCmd ── supervisor ── private pipes ── document worker
 ▲                     │                              │
 └── RenderEvent ───────┘                              └── EPUB paginator process
 │
 └── PresentCmd ── Vivid worker / Composer ── SDK control and media channels
        ▲                │
        └─ PresentEvent ─┘
```

Mailboxes contain at most 32 entries and one replaceable request/result of each coalescible kind.
Render requests coalesce only after the last query barrier; a request that cannot yet be queued
is retained as the UI's latest desired render and retried without blocking input. Export and
other ordered commands return a visible busy result if full. Obsolete display frames and page
results may be discarded, but their replacements are appended after intervening controls.
A cancelled mailbox closes independently of capacity. IPC readers and writers have bounded
queues too, so blocked pipe I/O cannot prevent the supervisor from enforcing deadlines.

The Vivid worker owns presenter state and composition. It retains a session cancellation handle
and access to the currently blocking track transport outside its work queue. Normal teardown
gets two seconds for ordered `DELETE_NODE`, `DESTROY_TRACK`, and `DESTROY_SURFACE`; cancellation
then shuts down control and media I/O. Renderer shutdown similarly kills and reaps stuck native
work. `Drop` and explicit shutdown share idempotent implementations.

The SDK continues to own authentication, checked framing, reply correlation, flow control,
heartbeat, and session-scoped cleanup. vvrd does not implement a second protocol stack.

---

## 5. Module map (`vvrd/src`)

Modeled on kitpdf, with the Kitty layer swapped for a Vivid presenter layer.

| Module | Role | Origin |
|---|---|---|
| `main.rs` | CLI parse, env/config, terminal guard, thread wiring, event loop | port of kitpdf `main.rs` (de-tokio-fied) |
| `app.rs` | App state: page, scroll/zoom/pan, input mode, search, transforms, pixmap residency | port of kitpdf `app.rs` (near-verbatim; drops Kitty `ImageId`) |
| `renderer.rs` | Native worker backend: cache, search, TOC, metadata, links, reload, EPUB reflow, export | MuPDF path from kitpdf plus native backend |
| `source_watch.rs` | Bounded source notifications, path matching, and reload debounce for Markdown/DOCX/PPTX | New |
| `office.rs` | Office conversion: LibreOffice discovery, content-hash PDF cache, bounded headless `soffice` subprocess with isolated profile | New for PPTX/DOCX/ODP/ODT viewing |
| `markup/` | Owned Markdown IR, Letter pagination, text/image/SVG raster helpers, bundled Mona Sans/Monaspace fonts | adapted from Kitmd `45cb75f` |
| `mermaid_engine/` | Complete Rust Mermaid parser, validation, layout, and SVG renderer | copied from Kitmd `45cb75f` |
| `compositor.rs` | Page pixmap + view transform → viewport RGBA buffer (crop/scale/highlight/crop-margins) | derived from kitpdf `image_pipeline.rs` + `compute_page_surface` |
| `presenter.rs` | `Presenter` trait + `VividPresenter`: session, document surface, raster track + channel, scene node, frame send, resize, three recoveries, teardown | wraps `vivid_sdk` |
| `render_process.rs` | Subprocess supervision, deadlines, validated private-pipe IPC and pagination | — |
| `mailbox.rs` | Bounded submission, ordering barriers and latest-view coalescing | — |
| `child_process.rs` | Helper environment scrubbing and process-tree cancellation | — |
| `vivid_thread.rs` | Owns `Session` + presenter; `PresentCmd`/`PresentEvent` loop; signal servicing; frame coalescing | — |
| `fake_presenter.rs` | Test-only Vivid 1.5 presenter: real auth transcript, framing, and channel handshake | test support |
| `terminal.rs` | Raw mode / alt screen / cursor / mouse guard; status bar; TOC/metadata/links/help/loading text draw | port of kitpdf `terminal.rs` |
| `geometry.rs` | Terminal grid ↔ pixel geometry; drawable area; reconcile local size vs the presenter's terminal target descriptor | merge of kitpdf `terminal.rs` sizing + `vivi` `terminal_geometry.rs` |
| `state.rs` | Per-file persisted state (page, rotation, invert, crop, tint, EPUB em) in XDG cache | port of kitpdf `state.rs` (verbatim) |
| `export.rs` | Page → PNG export paths & writing | port of kitpdf `export.rs` (verbatim) |
| `error.rs`, `perf.rs` | Error types, perf logging | port (verbatim) |

Deleted vs kitpdf: `kitty.rs` (and its SHM/tmux/response-parsing machinery). Tokio,
`futures-util`, `memmap2`, `psx-shm`, `rustix` shm dependencies are dropped.

---

## 6. Vivid profile set

Mirrors `vivi`'s producer config (`vivi/src/client.rs::producer_config`), pruned to what a reader
needs. Profile lists must be sorted, unique, and prerequisite-closed; `ProducerConfig::validate`
rejects anything else, including a 1.1 feature name repackaged as a profile.

**Required** (fail fast if the presenter lacks them):

- `vivid-core-control-v1` — the session, scene, and surface/track control plane
- `terminal-surface-v1` — the selected presentation target: grid, cell metrics, text layers, anchors
- `live-media-v1` — live-mode tracks, which need no `PLAY` and recover with a full frame

**Optional** (used when present):

- `observability-v1` — `QUERY_TRACK` around recovery, for generation-local milestone diagnostics

Everything the 1.1 config negotiated per feature is now either implied by a profile or a per-track
configuration field:

| 1.1 feature | 1.5 home |
|---|---|
| `RASTER_RGBA8`, `RASTER_DELTA_V1`, `RASTER_ZSTD_V1` | raster track configuration, probed |
| `SCENE_TRANSACTIONS`, `GRID_CELL_NODES`, `NODE_CLIP_RECT_V1` | `vivid-core-control-v1` + `terminal-surface-v1` |
| `CREDIT_FLOW_CONTROL` | cumulative `MAX_CHANNEL_DATA` on each track channel |
| `SOURCE_DESCRIPTOR_V1`, `SOURCE_CAPTURE_POLICY_V1` | surface descriptor and surface policy |
| `OBSERVABILITY_CORE_V1` | `observability-v1` |
| `VISIBILITY_EVENTS_V1` | no equivalent; see §7 |
| `ENCODED_IMAGE_V1` | encoded-image track kind (unused by vvrd) |

Terminal anchors (marker v3) are **not required**: the full-screen reader uses grid-cell
coordinates. vvrd does check that the target descriptor reports marker version 3, since a v2 marker
must never cross-authenticate. (An anchor may be adopted later only if an inline/split-view mode is
added.)

---

## 7. Geometry & coordinate model

- **Grid** — the `terminal-surface-v1` **target descriptor** (authoritative from
  `WELCOME`/`TARGET_CHANGED`) gives target pixel size, grid columns/rows, cell width/height, a
  settled flag, and the anchor marker version. Vivid 1.5 core has no grid at all; this is target
  profile state, not surface or track state. Under `vvmux` these are the **pane's** dimensions, and
  they match vvrd's PTY size from `crossterm::size()` / `TIOCGWINSZ`. vvrd reconciles on
  `TARGET_CHANGED` (which fires on pane resize too), and falls back to the local terminal size if
  the descriptor is unusable.
- **Visibility is producer-owned.** 1.5 has no per-source visibility event: visibility is a surface
  placement property that vvrd itself controls with `UPDATE_NODE`, plus track presentation milestones
  it can query. vvrd therefore hides its node for text overlays exactly as before, but no longer
  gates frame submission on a presenter push. Backpressure comes from cumulative channel flow: a
  send blocks until `MAX_CHANNEL_DATA` raises the maximum.
- **Coordinates are pane-local.** vvrd always places at local `(0,0)`; the presenter (Vivido
  directly, or `vvmux` on its behalf) applies any pane offset and clips to the pane. vvrd never
  computes an absolute outer position.
- **Page area** — grid rows `0 .. rows-1`; the last row is the status bar. Node covers
  `cols × (rows-1)` cells.
- **Scene node** — `SceneNodeConfig { context_id: root, anchor_id: None, x:0, y:0,
  width: cols<<32, height: (rows-1)<<32, fit: contain, sampling: linear, text_layer: 1, z:0 }`.
  Coordinates are signed **32.32 fixed-point cells** (`i64::from(cells) << 32`).
- **Framebuffer** — rendered at exactly `cols·cell_w × (rows-1)·cell_h` px, so `contain`-fit of a
  same-aspect buffer fills the node with no letterbox.
- **Zoom** — the render thread rasterizes the page at `viewport × zoom_factor` (sharp text); the
  compositor crops the viewport-sized region at the current scroll/pan offset (kitpdf's crop path,
  writing into the buffer instead of a Kitty source-rect).
- **Frame identity** — `frame_id` strictly increasing & nonzero **for the lifetime of a track**,
  across channel generations; `epoch` monotonic. Width/height must exactly match the track's
  immutable raster configuration, so a **resize replaces the track** (new dims) and activates it into
  the surface's raster slot. The node keeps referencing the surface and never learns a track ID.

---

## 8. Lifecycle sequences

**Startup**
1. Parse CLI; read `VIVID_ENDPOINT_CONTROL`/`VIVID_ROOT_SECRET` (or dry-run/trace). Preflight-probe
   the document in a subprocess (kitpdf pattern) to fail cleanly on corrupt files. EPUB preflight
   verifies that MuPDF can open a reflowable document without also paginating the full book. The
   renderer publishes the requested EPUB page first, then a low-priority helper process completes the
   full-book page count and outline pass in a separate MuPDF address space. Arrow-key rendering
   continues in the document worker while that helper is busy. Neither helper child inherits any endpoint or
   secret variables. `mupdf_fonts::install()` runs before any document is opened — MuPDF caches
   resolved fonts per context, so a loader installed later is never consulted for faces it already
   looked up (D5a).
2. `Session::connect` (HELLO with a transcript-bound root proof, WELCOME with a server confirmation);
   verify the selected target profile and read the authoritative grid from the target descriptor.
3. Enter alt screen / raw mode / hide cursor (terminal guard). Install panic hook that restores the
   terminal *and* tears down Vivid.
4. Spawn the document subprocess supervisor and Vivid thread (owns the session). Send initial `Render`
   (viewport × zoom) to the renderer; `CREATE_SURFACE`, then create + prime the raster track, then
   `ACTIVATE_TRACK` slot 3, then a hidden `CREATE_NODE` for the full-viewport placement. Show it after the first content frame.
5. Load persisted per-file state; jump to saved/`-p` page.

The track is primed — channel opened, first full frame sent, `MILESTONE_OUTPUT_READY` awaited —
*before* activation, so a slot never points at a track with no decoded output.

**Page turn / navigation** — UI updates `App.page`; sends `RenderNotif::JumpToPage` (renderer
prioritizes that page's window) and `PresentCmd::ShowView{page, transform}`. Vivid thread composites
the newest ready pixmap for that page into the viewport buffer and sends a frame. Until the pixmap is
ready, UI shows the delayed "Loading…" text (node hidden).

**Scroll / zoom / pan / rotate / invert / tint / crop** — UI mutates transform state and posts
`ShowView`. Rotate/invert/tint change pixel content → also notify the renderer to re-rasterize (as
kitpdf does). Zoom changes render resolution → new `Area`. Vivid thread coalesces to the latest view.

**Resize** — debounce (kitpdf's 120 ms). While dragging, only the node geometry moves
(`UPDATE_NODE`); nothing else changes. On settle, `PresentCmd::Resize` performs the full track
replacement:

1. compose the retained page at the new dimensions, prime the replacement track with content,
   and wait for output readiness (startup keeps the node hidden until content exists);
2. `UPDATE_SURFACE` the logical size (this advances the surface **generation**, since coordinate
   truth changed, but not surface identity);
3. `ACTIVATE_TRACK` slot 3 — one atomic compositor-boundary swap;
4. adopt the activated track/channel locally, then `UPDATE_NODE` if cell geometry changed;
5. `DESTROY_TRACK` the retired track, then drop its transport.

Because the replacement is already output-ready when the slot swaps, the resize shows **no blank
frame** — the visible regression of the 1.1 source-replacement path. A failure before activation destroys the candidate and restores surface geometry and the
previous activation through current SDK revisions. Failed rollback cancels the session. After
activation, local state always owns the replacement, even if node update fails; retirement still
runs. Target-follow retries obey an unconditional two-second deadline.

The local PTY resize is only a fallback for presenters that do not announce `TARGET_CHANGED`. Each
fallback command names the last target generation observed by the UI; if an authoritative target
change advances while a nested replacement is still priming, the queued older fallback is dropped
instead of replacing the correct track again with the previous presenter's cell-pixel geometry.

**Overlay (TOC/metadata/links/help/search/goto)** — UI sends `PresentCmd::HideNode`, draws text over
the full screen; on exit `PresentCmd::ShowView` re-shows and re-sends the page.

**Source change** — the UI drains bounded watcher events, extends a 300 ms debounce on each
matching mutation, and sends the existing transactional `Reload` command once the source exists
and the debounce settles. The refreshed current page follows the normal render/present path.

**Search / links / export** — reuse kitpdf's renderer-side flows verbatim (`Search`, `GetLinks`,
`ExportPage`); results flow back as `RenderInfo`. External links open via the system opener.

**Quit / panic** — Vivid thread teardown guard: `DELETE_NODE` → `DESTROY_TRACK` → drop the track
transport → `DESTROY_SURFACE`; UI restores the primary screen; state is persisted.

**Track loss / channel loss** — see D9. Neither disturbs the surface, the node, or the document
descriptor; both re-arm a full frame so the recovered generation starts with a complete image.

---

## 9. Running inside vvmux (nested presenter)

vvrd **must** run inside `vvmux`, including a **floating pane** (where `vivi` already works). This is
a first-class target, not an afterthought. The good news from auditing `vvmux/src` is that the
framebuffer + single-node model (D3/D4) is exactly the shape `vvmux` handles best.

Both current Vivido and vvmux use Vivid 1.5. Direct and nested smoke tests exercise the same
producer binary; no 1.1 compatibility path is provided.

**The nested path.** `vvmux` runs a per-pane *virtual presenter*: it sets the pane shell's Vivid
endpoint and secret to point at itself, terminates vvrd's Vivid session, scopes objects and flow to
the pane, and **bridges** a revisioned projection snapshot up to the outer presenter (Vivido) in one
transaction (`vvmux/src/bridge.rs`, `media.rs`, `session.rs`). vvrd is unaware of any of this — it
speaks the ordinary producer contract.

**Pane-local coordinate translation (verified).** `media.rs::projection_snapshot` keeps each node in
**pane-local** grid coordinates (anchor-cell nodes are resolved into the same space by adding the
anchor's text cell). `session.rs::project_logical_node` then:

1. adds the pane origin: `x += pane.x << 32`, `y += pane.y << 32`;
2. intersects with the pane content rect **and** the tab area (a node bigger than the pane is clipped
   to it);
3. applies the producer's optional downstream clip;
4. subtracts every higher pane's opaque rectangle → visible fragments.

So vvrd's pane-local full-viewport node lands correctly wherever the pane is. Consequences vvrd
should design around:

- **Moving a floating pane needs zero vvrd work.** A move changes `pane.x/pane.y`; `vvmux`
  re-projects. vvrd only reacts to a **content-size change** (its PTY resize → replace the raster
  track at the new pane dims, the same resize path as everywhere else).
- **Occlusion is handled upstream, but favors a single node.** Higher floats are subtracted from
  vvrd's node into fragments; a logical node is **dropped entirely above `MAX_NODE_FRAGMENTS` (8)**.
  One full-viewport node fragments minimally (1 rect, or a few under partial occlusion), so it stays
  well under the limit. The Phase-7 multi-node model (many page nodes) is *more* exposed to this and
  to per-pane budgets — a second reason the framebuffer model is the primary path.
- **Per-pane resource contracts.** 1.5 contracts bound rates and decoder/GPU cost, not just object
  counts, and a child contract reserves capacity from its parent. The framebuffer model uses **1
  surface + 1 node + 1 active track + a few MB**, and declares modest sustained rate/bitrate claims —
  trivially safe. Any Phase-7 per-page cache MUST bound residency against the same contract.
- **Raster coalescing is native.** `vvmux` "coalesces raster updates to the latest body," exactly
  matching vvrd's "re-send the viewport on each view change" model — intermediate scroll frames are
  dropped for free, on top of vvrd's own producer-side coalescing.
- **Clipping is added upstream.** `vvmux` requires `node-clip-rect-v1` from the *outer* presenter so
  fitted quads can't escape pane content; vvrd does **not** need to send clips for the framebuffer
  model. (Only the Phase-7 oversized-node scroll model would send producer-side clips, via the
  optional `NODE_CLIP_RECT_V1` feature.) Occluded regions are clipped, not re-fit, so the page image
  shows correctly cropped under a float rather than squished.
- **Visibility is per-pane, and no longer pushed.** When vvrd's pane is on a background tab, hidden
  by a zoom, or fully occluded, the effect reaches vvrd as flow-control backpressure and, if it asks,
  a `NOT_VISIBLE` wait outcome — not as a source visibility event. vvrd keeps submitting and lets
  cumulative channel flow throttle it.
- **Alt screen & status row.** `vvmux-terminal` owns a primary/alternate screen per pane, so vvrd's
  alt-screen + last-row status bar live entirely within the pane (panes are clamped to ≥2 rows /
  ≥4 cols, so always leave room for the status row and degrade gracefully in a tiny pane).
- **Anchors vs grid-cell.** `vivi` uses text anchors, which `vvmux` resolves server-side; vvrd's
  grid-cell path is equally first-class (both become pane-local before projection). No anchor needed.

**Net:** vvrd requires no `vvmux`-specific branch. The only discipline is (a) always use pane-local
coordinates from the live PTY size, (b) treat pane resize as the single trigger for source recreate,
(c) keep to one node + one bounded source, and (d) honor visibility. All of these are already in the
core design; §Verification calls out explicit tiled/floating-pane test cases.

## 10. Feature parity matrix (kitpdf → vvrd)

| kitpdf feature | vvrd mechanism |
|---|---|
| PDF & EPUB via MuPDF | MuPDF document subprocess |
| Markdown and Mermaid | Native fixed 2040×2640 Letter `PagePlan`; Kitmd-derived renderer/engine |
| Sharp zoom (re-render at resolution) | Render page at `viewport×zoom`; crop viewport region into framebuffer |
| Vertical scroll + auto page-turn at bounds | Same `App` scroll logic; compositor crops at scroll offset |
| Horizontal pan (zoom mode) | Compositor crops at pan offset |
| Auto-crop whitespace | `compositor.rs` (ported `crop_whitespace`) before blit |
| Colour tint (sepia) / invert / custom B&W | Backend render transform; MuPDF behavior unchanged |
| Rotate 90° | MuPDF matrix or markup raster rotation |
| Search + highlight + n/N + cross-page counts | MuPDF text quads or markup page semantics; highlight composited into buffer |
| Table of contents (+ mouse) | MuPDF outlines or Markdown heading slugs; full-screen overlay |
| Metadata / links / follow links | MuPDF extraction or markup semantics; scrubbed external opener |
| Go-to-page, PgUp/PgDn, arrows, hjkl, space | Same `App` key handling |
| EPUB reflow + font size `<`/`>` | Renderer EPUB layout path (unchanged) |
| State persistence (XDG) | `state.rs` (unchanged) |
| Export page to PNG (`-e`, `e`) | `export.rs` (unchanged) |
| Light/dark paper theme | `--theme light|dark`, markup only |
| Source refresh | Automatic debounced Markdown/DOCX/PPTX watch plus manual `R`/F5; atomic backend reload keeps the old document on failure |
| Loading indicator w/ delay | Same UI timer; text drawn while node hidden |
| Panic isolation + slow-render watchdog | Supervised subprocess with operation deadlines |
| Clean exit / Ctrl-C / panic cleanup | Terminal restore **+ Vivid node/track/surface teardown** |
| Kitty capability detection / SHM probe | Replaced by Vivid profile negotiation (HELLO/WELCOME) and `PROBE_TRACK_CONFIG` |
| tmux passthrough placeholders | N/A (Vivid media is off-PTY); grid-cell node needs no passthrough |

---

## 11. Invariants & security (from repo `AGENTS.md`)

- **Media off the PTY.** Only status/overlay text and (if ever adopted) the bounded authenticated
  anchor marker touch the PTY. Page pixels go over Vivid media connections only.
- **Never log/serialize/forward** `VIVID_ROOT_SECRET`, session keys, channel tags, or derived
  material; secret-bearing config has no `Debug`. The root secret never reaches the wire — only a
  transcript-bound proof. Child processes (preflight probe, external link opener, export) inherit
  neither the secret nor the endpoints.
- **Validate before allocate.** Clamp render dimensions to the raster admissibility budget; use
  checked arithmetic for geometry and buffer sizes; stay within the track's declared maximum record
  body and sustained rate claims.
- **Bounded resources.** Pixmap residency budget (MB, like kitpdf); one surface, one node, one active
  track; a command queue sized from the declared in-flight claim; coalesce superseded views.
- **Complete owner identity.** Every predicate that matches, retains, or tears down an object uses
  the full `(context, surface, track)` tuple — never a bare numeric ID. `TRACK_LOST` filtering is the
  specific place this would go wrong, and it carries a two-owner regression.
- **Scoped recovery.** A malformed page produces a caught panic / status error, never corrupted
  terminal text or a lost session. A lost track loses only a track; a lost channel loses only a
  channel generation. Neither touches surface, node, descriptor, or policy.
- **Full-duplex liveness.** The SDK answers `PING`, routes channel flow and events, and correlates
  replies; vvrd must not stall those (hence frame sends live off the UI thread).

---

## 12. Non-goals / deferred

- Audio/video playback (vvrd is a document reader; that is `vivi`/`veston` territory).
- tmux/screen inline-anchor mode (full-screen grid-cell placement needs no anchors).
- The Phase-7 per-page-source bandwidth optimization is optional; the framebuffer model is complete
  and correct on its own.
- Reader niceties beyond kitpdf parity (dual-page spread, continuous scroll across pages,
  annotations) are future work once parity ships.

---

*See [`IMPLEMENTATION_PLAN.md`](./IMPLEMENTATION_PLAN.md) for the phased build order, dependency
list, file-by-file tasks, and verification steps.*

## Audit validation

See [AUDIT-2026-09.md](AUDIT-2026-09.md) for regression coverage, benchmark measurements and
platform validation. Helper launchers centrally remove all `VIVID_*` environment entries.
Document-derived terminal output is sanitized and clipped by grapheme/cell width before `Print`.
