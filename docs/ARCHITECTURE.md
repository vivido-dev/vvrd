# vvrd — Architecture

`vvrd` (Vivido Reader) is a terminal PDF and EPUB reader for the **Vivido** terminal. It renders
documents with MuPDF and displays them through the **Vivid Protocol 1.1** side channel, using the
reusable [`vivid_sdk`](../../vivid_sdk) producer client on top of
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
  Vivid │   control connection  (VIVID_ENDPOINT)               │
  side  │   media connections   (VIVID_ENDPOINT[_BULK])        │  ◀── page images ride here,
  chan. │   authenticated by VIVID_TOKEN                        │      NEVER on the PTY
        └──────────────────────────────────────────────────────┘
                         ▲
                         │  vvrd is a Vivid *producer*
                 ┌───────┴────────┐
                 │      vvrd       │   MuPDF render · TUI · vivid_sdk
                 └────────────────┘
```

Vivido launches its child shell with `VIVID_ENDPOINT` and `VIVID_TOKEN` set
(`vivido/src/window_context.rs`). The user then runs `vvrd file.pdf` like any command. `vvrd`:

- reads the endpoint/token from the environment (never logs or forwards them),
- opens an authenticated control session and one or more media connections,
- keeps **all image bytes off the PTY** — the PTY carries only ordinary terminal text (status bar,
  overlays) and at most a bounded, authenticated anchor marker,
- places its page image as a **scene node** the presenter composites into the window.

The same binary works unchanged through `vvmux` and over `vvssh` (remote forwarding). `vvmux` is a
**virtual presenter**: it terminates the Vivid session for the pane, then bridges the pane's sources
and scene nodes up to the outer presenter (Vivido), translating **pane-local** coordinates into the
composed grid and clipping them to the pane. vvrd needs **no vvmux-specific code** — it always speaks
the ordinary producer contract to whatever presenter set `VIVID_ENDPOINT`. Because vvrd must run in
`vvmux` (including a floating pane, where `vivi` already works), this is treated as a first-class
target — see [§9](#9-running-inside-vvmux-nested-presenter).

---

## 2. kitpdf → vvrd: the fundamental shift

| Concern | kitpdf (Kitty) | vvrd (Vivid) |
|---|---|---|
| Image transport | Escape sequences / SHM **in-band on the PTY** | Vivid **media connection** (side channel), bytes off the PTY |
| Protocol handshake | Query/response over stdin/stdout | `HELLO`/`WELCOME` + feature negotiation via `vivid_sdk` |
| "Show an image" | `TransmitAndDisplay` / `Display` Kitty action | `CREATE_RASTER` → `RASTER_FRAME` + `CREATE_NODE` scene transaction |
| Placement | Cursor `MoveTo` + Kitty display location (source-rect crop) | Scene node in **grid-cell 32.32 coordinates**, presenter `contain`-fits |
| Image identity | Stable Kitty image id per page; re-display; delete under pressure | Persistent viewport **raster source** re-sent as frames (primary model) |
| Reading terminal replies | stdin is shared by keypresses **and** Kitty responses (must disambiguate) | stdin is **pure user input**; all presenter I/O is on Vivid sockets |
| Flow control | None (terminal buffers) | **Credit-based**, source-scoped, enforced by the SDK |
| Async model | tokio event loop + blocking render thread | **Synchronous, thread-based** (the SDK is blocking) |
| Teardown | Delete Kitty images | Delete node + `DESTROY_SOURCE` + `GOODBYE` (a leaked node is a ghost image) |

Everything MuPDF-related (page rasterization, search, TOC, metadata, links, invert/tint/rotate,
auto-crop, EPUB reflow, export) ports over almost verbatim. Everything Kitty-specific
(`kitty.rs`, the SHM path, the stdin-response parser, tmux passthrough placeholders) is **replaced**
by a thin Vivid presenter layer built on `vivid_sdk`.

---

## 3. Core design decisions

### D1 — Build on `vivid_sdk`, not raw `vivid_protocol`
`vivid_sdk::ProducerSession` already owns authentication, the split full-duplex control dispatcher,
heartbeat/`PING`/`PONG`, reply correlation, credit accounting, scene transactions, text anchors, and
credit-aware media senders. vvrd consumes it exactly as `vivi` and `veston` do. vvrd depends on
`vivid_protocol` only for shared constants/types (`messages::FEATURE_*`, `ConnectionKind`,
`SceneNodeConfig`).

### D2 — Synchronous, multi-threaded runtime (no tokio)
The SDK is blocking (std threads + condvars + blocking socket I/O). vvrd drops kitpdf's tokio
runtime and uses plain `std::thread` workers coordinated by `flume` channels, with
`crossterm::event::poll(timeout)` driving the UI timers (loading-indicator delay, resize debounce).
This matches the SDK's nature and removes an async/blocking impedance mismatch.

### D3 — Display primitive: the **viewport framebuffer** raster source
vvrd's presenter layer exposes one job behind a `Presenter` seam: *"make the terminal show this
composited view."* The recommended implementation is a **single persistent raster source sized to
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
- **No size ceilings, ever.** A raster source is admissible only if `72 + w·h·4` fits the presenter
  ceiling (~16.7 M pixels raw). A viewport-sized frame is always far under that, so — unlike kitpdf,
  which clamps to 10 000 px for Kitty — zoom is unbounded. Sharpness is preserved by rendering the
  page at zoom resolution in MuPDF and cropping the viewport region, exactly as kitpdf does.
- **Single bounded presenter resource** with retained delta composition and full-frame recovery.
- **Zstd for free.** When `RASTER_ZSTD_V1` is negotiated, the SDK compresses frames internally;
  vvrd adds no zstd dependency.

When `RASTER_DELTA_V1` is accepted, the Vivid thread retains the last submitted viewport. Scroll
and pan become an overlap-safe copy plus overwrite rectangles for newly exposed strips. Other
changes become a tight overwrite bounding the changed pixels. The SDK compares the actual raw or
zstd delta representation with the equivalent full frame and sends the delta only when smaller.
Vvrd forces a full frame for a new source or epoch, after `NEED_FULL_FRAME`, and whenever
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

### D5 — MuPDF render thread + CPU pixmap prerender cache
A dedicated blocking thread owns the MuPDF `Document` (which is `!Send`) and renders a window of
pages around the current page into RGB pixmaps, plus search/TOC/metadata/links/export — a near-direct
port of kitpdf's `renderer.rs`. Prerendering neighbors keeps page turns instant. The cache is
**CPU-side pixmaps** (bounded by MB budget like kitpdf), feeding the compositor; it is not a
presenter-side image cache in the primary model.

### D6 — Input is pure PTY stdin
Running under Vivido, vvrd is an ordinary terminal app from the PTY's view. Keyboard/mouse come
through `crossterm` in raw mode exactly as kitpdf — but simpler, because stdin carries **no** graphics
responses to disambiguate. All presenter traffic is on Vivid sockets, serviced by SDK background
threads.

### D7 — Feature negotiation and graceful fallback
vvrd negotiates the producer feature set (§6). Raster RGBA8 + scene transactions + grid-cell nodes +
credit flow control are **required** (no presenter without them can show anything). Zstd,
raster deltas, observability, visibility events, and encoded-image are **optional** enhancements.
If the terminal is not Vivido
(no `VIVID_ENDPOINT`), vvrd exits with a clear message, or runs in `--dry-run`/`--trace` for
development, just like `vivi`.

### D8 — Deterministic, bounded teardown
Because a placed node persists in the presenter independent of PTY text, exit/panic paths **must**
delete the node, `DESTROY_SOURCE`, and `GOODBYE`. A guard (RAII / scopeguard on the Vivid thread)
guarantees this on normal exit, `q`/`Ctrl-C`, error, and panic — otherwise a ghost image lingers in
Vivido.

### D9 — Office documents are converted to PDF before any thread starts
MuPDF cannot read DOCX, PPTX, ODT, or ODP. Rather than teach the renderer a second document model,
`main.rs` resolves the CLI path **once**, up front, into an `office::DocumentInput` holding two
paths: the `origin` the user named, and the `render_path` MuPDF opens. For PDF and EPUB they are the
same path and nothing is read or written. For an office format the document is converted into a
`TempDir` owned by `DocumentInput` and `render_path` points at the resulting PDF.

Consequences, all deliberate:

- `renderer.rs`, `compositor.rs`, `vivid_thread.rs`, the page cache, the prerender window, and the
  raster-delta planner stay entirely format-agnostic. A converted document is `DocumentKind::Fixed`,
  so rerendered zoom (D3) works and `epub_font_size` is inert.
- Conversion happens once per process, never per page or per resize.
- `origin` remains the identity for saved state, the source descriptor title, the capture policy,
  and export filenames. A temporary path must never leak into any of them.
- `DocumentInput` is held in `main` for the whole run; dropping it deletes the converted PDF, on
  every exit route including panic unwind.

Two backends, chosen at `resolve` time and overridable with `VVRD_OFFICE_BACKEND`: a real
`soffice --headless --convert-to pdf` (preferred; run with a private `-env:UserInstallation` profile
so it cannot collide with the user's own LibreOffice session, with `VIVID_TOKEN` stripped from its
environment and a bounded timeout), and the pure-Rust `lo_writer`/`lo_impress` importers. The
pure path is lossy — it drops embedded images and ignores page and slide geometry — so it reports
itself in the status row instead of presenting an approximation as the document.

---

## 4. Runtime structure (threads & data flow)

Three threads, coordinated by `flume` channels (kitpdf already uses `flume`):

```
              key / mouse / resize (crossterm, raw mode, alt screen)
                                   │
          ┌────────────────────────▼─────────────────────────┐
          │  UI thread  (main)                                │
          │  · owns App state (page, scroll, zoom, mode…)     │
          │  · crossterm poll/read loop + UI timers           │
          │  · draws status bar & text overlays to the PTY    │
          │  · decides the desired *view* + document commands  │
          └───────┬───────────────────────────────┬──────────┘
     RenderNotif  │                                │ PresentCmd
   (JumpToPage,   │                                │ (ShowView{page,transform},
    Area, Search, ▼                                ▼  Resize, HideNode, Teardown)
    Invert…) ┌─────────────────┐        ┌────────────────────────────────┐
             │ Render thread    │ Page  │ Vivid thread                    │
             │ · MuPDF Document │ pixmap│ · owns ProducerSession          │
             │   (!Send)        ├──────▶│ · owns viewport raster source   │
             │ · page → RGB     │(flume)│   + MediaChannel + scene node   │
             │ · search/TOC/    │       │ · composites viewport RGBA      │
             │   meta/links/exp │       │   (crop/scale/highlight)        │
             └─────────────────┘        │ · send_raster_frame (credits)   │
                     ▲                   │ · scene txns / resize / goodbye │
                     └───────RenderInfo──┴────────────┬───────────────────┘
                        (NumPages, Page, Toc,         │ PresentEvent
                         Metadata, Links, Error)      ▼ (FrameShown, SourceLost,
                                                        NodeReady, Visibility)
                                              back to UI thread
```

- **UI thread** never blocks on the network. It emits `RenderNotif` to the render thread (identical
  enum to kitpdf) and `PresentCmd` to the Vivid thread, and consumes `RenderInfo`/`PresentEvent`.
- **Render thread** is a port of kitpdf `renderer.rs`: a window of pages rendered around the current
  page, MuPDF `!Send` isolation, panic-caught per page, a slow-render watchdog, EPUB reflow/layout.
- **Vivid thread** owns *all* presenter I/O. It composites the current page pixmap + view transform
  into the viewport buffer and sends it as a raster frame (blocking on credit off the UI thread),
  runs scene transactions, handles resize (recreate source at new dims), applies source events
  (visibility/loss), and performs teardown. It **coalesces** superseded view requests — only the
  latest desired view is composited/sent — giving smooth scroll with no backlog (spec §11.4 allows
  the presenter to drop intermediate frames; vvrd drops them producer-side).

The SDK additionally runs its own `vivid-sdk-control` and `vivid-sdk-heartbeat` threads internally
(reply routing, `PING`/`PONG`, liveness). vvrd does not manage those.

---

## 5. Module map (`vvrd/src`)

Modeled on kitpdf, with the Kitty layer swapped for a Vivid presenter layer.

| Module | Role | Origin |
|---|---|---|
| `main.rs` | CLI parse, env/config, terminal guard, thread wiring, event loop | port of kitpdf `main.rs` (de-tokio-fied) |
| `app.rs` | App state: page, scroll/zoom/pan, input mode, search, transforms, pixmap residency | port of kitpdf `app.rs` (near-verbatim; drops Kitty `ImageId`) |
| `renderer.rs` | MuPDF render thread: pixmaps, search, TOC, metadata, links, EPUB reflow, export, watchdog | port of kitpdf `renderer.rs` (near-verbatim) |
| `office.rs` | Resolves the CLI path to a MuPDF-readable one: DOCX/PPTX/ODT/ODP → PDF in a self-deleting temp dir, via `soffice` or the pure-Rust importers | **new** (D9) |
| `compositor.rs` | Page pixmap + view transform → viewport RGBA buffer (crop/scale/highlight/crop-margins) | derived from kitpdf `image_pipeline.rs` + `compute_page_surface` |
| `presenter.rs` | `Presenter` trait + `VividPresenter`: session, viewport raster source, scene node, frame send, resize, teardown | **new** (replaces `kitty.rs`); wraps `vivid_sdk` |
| `vivid_thread.rs` | Owns `ProducerSession` + presenter; `PresentCmd`/`PresentEvent` loop; frame coalescing | **new** |
| `terminal.rs` | Raw mode / alt screen / cursor / mouse guard; status bar; TOC/metadata/links/help/loading text draw | port of kitpdf `terminal.rs` |
| `geometry.rs` | Terminal grid ↔ pixel geometry; drawable area; reconcile local size vs presenter `display_state` | merge of kitpdf `terminal.rs` sizing + `vivi` `terminal_geometry.rs` |
| `state.rs` | Per-file persisted state (page, rotation, invert, crop, tint, EPUB em) in XDG cache | port of kitpdf `state.rs` (verbatim) |
| `export.rs` | Page → PNG export paths & writing | port of kitpdf `export.rs` (verbatim) |
| `error.rs`, `perf.rs` | Error types, perf logging | port (verbatim) |

Deleted vs kitpdf: `kitty.rs` (and its SHM/tmux/response-parsing machinery). Tokio,
`futures-util`, `memmap2`, `psx-shm`, `rustix` shm dependencies are dropped.

---

## 6. Vivid feature set

Mirrors `vivi`'s producer config (`vivi/src/client.rs::producer_config`), pruned to what a reader
needs.

**Required** (fail fast if the presenter lacks them):

- `FEATURE_RASTER_RGBA8` (1) — the display substrate
- `FEATURE_SCENE_TRANSACTIONS` (3) — place/move/hide the page node
- `FEATURE_GRID_CELL_NODES` (4) — absolute full-viewport placement
- `FEATURE_CREDIT_FLOW_CONTROL` (5) — media backpressure

**Optional** (used when present):

- `FEATURE_RASTER_ZSTD_V1` (8) — compressed frames; SDK-internal
- `FEATURE_VISIBILITY_EVENTS_V1` (10) — pause compositing/sending while off-screen or occluded
- `FEATURE_ENCODED_IMAGE_V1` (7) — enables the Phase-7 per-page PNG/JPEG cache optimization
- `FEATURE_NODE_CLIP_RECT_V1` (15) — clean clipping for the Phase-7 oversized-node scroll model
- `FEATURE_OBSERVABILITY_CORE_V1` (18) — query accepted media identity around recovery
- `FEATURE_SOURCE_DESCRIPTOR_V1` (20) — publish document semantics
- `FEATURE_SOURCE_CAPTURE_POLICY_V1` (22) — protect sensitive document paths
- `FEATURE_RASTER_DELTA_V1` (23) — copy/overwrite retained viewport updates

Text anchors (`TEXT_ANCHORS_V2`) are **not required**: the full-screen reader uses grid-cell
coordinates. (An anchor may be adopted later only if an inline/split-view mode is added.)

---

## 7. Geometry & coordinate model

- **Grid** — `display_state()` (authoritative from `WELCOME`/`DISPLAY_CHANGED`) gives
  `grid_columns`, `grid_rows`, `cell_width`, `cell_height`. Under `vvmux` these are the **pane's**
  dimensions (the virtual presenter reports pane size), and they match vvrd's PTY size from
  `crossterm::size()` / `TIOCGWINSZ`. vvrd uses this grid for scene coordinates and reconciles on
  `DISPLAY_CHANGED` (which fires on pane resize too).
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
- **Frame identity** — `frame_id` strictly increasing & nonzero; `epoch` monotonic. Width/height
  must exactly match the source, so a **resize recreates the source** (new dims) and re-binds the
  node's `source_id`.

---

## 8. Lifecycle sequences

**Startup**
1. Parse CLI; read `VIVID_ENDPOINT`/`VIVID_TOKEN` (or dry-run/trace). Preflight-probe the document
   in a subprocess (kitpdf pattern) to fail cleanly on corrupt files.
2. `ProducerSession::connect` (HELLO/WELCOME, feature check). Read authoritative grid.
3. Enter alt screen / raw mode / hide cursor (terminal guard). Install panic hook that restores the
   terminal *and* tears down Vivid.
4. Spawn render thread (MuPDF) and Vivid thread (owns the session). Send initial `Area`
   (viewport × zoom) to the renderer; create the viewport raster source + full-viewport node.
5. Load persisted per-file state; jump to saved/`-p` page.

**Page turn / navigation** — UI updates `App.page`; sends `RenderNotif::JumpToPage` (renderer
prioritizes that page's window) and `PresentCmd::ShowView{page, transform}`. Vivid thread composites
the newest ready pixmap for that page into the viewport buffer and sends a frame. Until the pixmap is
ready, UI shows the delayed "Loading…" text (node hidden).

**Scroll / zoom / pan / rotate / invert / tint / crop** — UI mutates transform state and posts
`ShowView`. Rotate/invert/tint change pixel content → also notify the renderer to re-rasterize (as
kitpdf does). Zoom changes render resolution → new `Area`. Vivid thread coalesces to the latest view.

**Resize** — debounce (kitpdf's 120 ms). On settle: recompute grid, send new `Area` to renderer,
`PresentCmd::Resize` recreates the raster source at new dims and `UPDATE_NODE`s the geometry, then
re-sends the current view.

**Overlay (TOC/metadata/links/help/search/goto)** — UI sends `PresentCmd::HideNode`, draws text over
the full screen; on exit `PresentCmd::ShowView` re-shows and re-sends the page.

**Search / links / export** — reuse kitpdf's renderer-side flows verbatim (`Search`, `GetLinks`,
`ExportPage`); results flow back as `RenderInfo`. External links open via the system opener.

**Quit / panic** — Vivid thread teardown guard: `delete_scene_node` → `destroy_source` → `goodbye`;
UI restores the primary screen; state is persisted.

---

## 9. Running inside vvmux (nested presenter)

vvrd **must** run inside `vvmux`, including a **floating pane** (where `vivi` already works). This is
a first-class target, not an afterthought. The good news from auditing `vvmux/src` is that the
framebuffer + single-node model (D3/D4) is exactly the shape `vvmux` handles best.

**The nested path.** `vvmux` runs a per-pane *virtual presenter*: it sets `VIVID_ENDPOINT`/
`VIVID_TOKEN` for the pane's shell to point at itself, terminates vvrd's Vivid session, scopes
sources/nodes/anchors/credits to the pane, and **bridges** a revisioned projection snapshot up to
the outer presenter (Vivido) in one transaction (`vvmux/src/bridge.rs`, `media.rs`, `session.rs`).
vvrd is unaware of any of this — it speaks the ordinary producer contract.

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
  re-projects. vvrd only reacts to a **content-size change** (its PTY resize → recreate the raster
  source at the new pane dims, the same resize path as everywhere else).
- **Occlusion is handled upstream, but favors a single node.** Higher floats are subtracted from
  vvrd's node into fragments; a logical node is **dropped entirely above `MAX_NODE_FRAGMENTS` (8)**.
  One full-viewport node fragments minimally (1 rect, or a few under partial occlusion), so it stays
  well under the limit. The Phase-7 multi-node model (many page nodes) is *more* exposed to this and
  to per-pane budgets — a second reason the framebuffer model is the primary path.
- **Per-pane media budgets** (defaults: 16 producers, 64 sources, 256 nodes, **256 MiB retained**).
  The framebuffer model uses **1 source + 1 node + a few MB** — trivially safe. Any Phase-7 per-page
  cache MUST bound residency to stay under 64 sources / 256 MiB *per pane*.
- **Raster coalescing is native.** `vvmux` "coalesces raster updates to the latest body," exactly
  matching vvrd's "re-send the viewport on each view change" model — intermediate scroll frames are
  dropped for free, on top of vvrd's own producer-side coalescing.
- **Clipping is added upstream.** `vvmux` requires `node-clip-rect-v1` from the *outer* presenter so
  fitted quads can't escape pane content; vvrd does **not** need to send clips for the framebuffer
  model. (Only the Phase-7 oversized-node scroll model would send producer-side clips, via the
  optional `NODE_CLIP_RECT_V1` feature.) Occluded regions are clipped, not re-fit, so the page image
  shows correctly cropped under a float rather than squished.
- **Visibility is per-pane.** `vvmux` emits `VISIBILITY=false` when vvrd's pane is on a background
  tab, hidden by another pane's zoom, or fully occluded. vvrd's Phase-6 visibility handling pauses
  compositing/sending until visible again.
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
| PDF & EPUB via MuPDF | Same MuPDF render thread |
| Sharp zoom (re-render at resolution) | Render page at `viewport×zoom`; crop viewport region into framebuffer |
| Vertical scroll + auto page-turn at bounds | Same `App` scroll logic; compositor crops at scroll offset |
| Horizontal pan (zoom mode) | Compositor crops at pan offset |
| Auto-crop whitespace | `compositor.rs` (ported `crop_whitespace`) before blit |
| Colour tint (sepia) / invert / custom B&W | MuPDF `tint`/invert at render (unchanged) |
| Rotate 90° | MuPDF matrix rotate at render (unchanged) |
| Search + highlight + n/N + cross-page counts | Renderer search flow (unchanged); highlight composited into buffer |
| Table of contents (+ mouse) | MuPDF outlines; full-screen text overlay; node hidden |
| Metadata / links / follow links | MuPDF extract; text overlays; internal jump / external open |
| Go-to-page, PgUp/PgDn, arrows, hjkl, space | Same `App` key handling |
| EPUB reflow + font size `<`/`>` | Renderer EPUB layout path (unchanged) |
| State persistence (XDG) | `state.rs` (unchanged) |
| Export page to PNG (`-e`, `e`) | `export.rs` (unchanged) |
| Loading indicator w/ delay | Same UI timer; text drawn while node hidden |
| Panic isolation + slow-render watchdog | Ported into the render thread |
| Clean exit / Ctrl-C / panic cleanup | Terminal restore **+ Vivid node/source teardown + goodbye** |
| Kitty capability detection / SHM probe | Replaced by Vivid feature negotiation (HELLO/WELCOME) |
| tmux passthrough placeholders | N/A (Vivid media is off-PTY); grid-cell node needs no passthrough |

---

## 11. Invariants & security (from repo `AGENTS.md`)

- **Media off the PTY.** Only status/overlay text and (if ever adopted) the bounded authenticated
  anchor marker touch the PTY. Page pixels go over Vivid media connections only.
- **Never log/serialize/forward** `VIVID_TOKEN`, tickets, or derived material; token-bearing config
  has no `Debug`. Don't pass the token into child processes (external link opener, export).
- **Validate before allocate.** Clamp render dimensions to the raster admissibility budget; use
  checked arithmetic for geometry and buffer sizes; honor each source's `max_media_body`.
- **Bounded resources.** Pixmap residency budget (MB, like kitpdf); single viewport source; one
  in-flight frame via credit; coalesce superseded views.
- **Source-scoped behavior.** A malformed page produces a caught panic / status error, never
  corrupted terminal text or a lost session. Handle `SOURCE_LOST` by recreating the source.
- **Full-duplex liveness.** The SDK answers `PING`, routes credit/visibility, and correlates
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
