# vvrd — Implementation Plan

Phased build order for the Vivido PDF/EPUB/DOCX/PPTX reader. Read
[`ARCHITECTURE.md`](./ARCHITECTURE.md) first — this document assumes its decisions (D1–D9), the
three-thread runtime, the Office preparation step, the module map, and the viewport-framebuffer
display model.

Guiding principle: **port kitpdf's MuPDF + UI + pixel machinery nearly verbatim, and replace only
the Kitty layer with a `vivid_sdk` presenter layer.** Get a correct end-to-end path working early
(one page on screen through Vivid), then layer features in kitpdf's order.

---

## 0. Dependencies & scaffolding

### `vvrd/Cargo.toml`

Start from kitpdf's manifest, **remove** the Kitty/async stack, **add** the Vivid crates.

```toml
[package]
name = "vvrd"
version = "0.1.0"
edition = "2024"
rust-version = "1.86"          # MuPDF needs 1.86; SDK/protocol are 1.85 — take the max
description = "A terminal PDF and EPUB reader for the Vivido terminal (Vivid Protocol)"
license = "MIT"

[[bin]]
name = "vvrd"
path = "src/main.rs"

[dependencies]
# Vivid producer stack
vivid_sdk      = { path = "../vivid_sdk" }
vivid_protocol = { path = "../vivid_protocol" }

# Document rendering (identical to kitpdf)
mupdf = { version = "0.8.0", default-features = false, features = ["svg","system-fonts","img","html","epub"] }

# Terminal I/O + pixel work
crossterm = "0.29"
image     = { version = "0.25", features = ["png","rayon"], default-features = false }
rayon     = { version = "1", default-features = false }

# CLI, colours, state, hashing, dirs
clap            = { version = "4", features = ["derive","env"] }
csscolorparser  = { version = "0.8", default-features = false }
serde           = { version = "1", features = ["derive"] }
serde_json      = "1"
directories     = "6"
md-5            = "0.11"

# Channels, errors, logging
flume       = { version = "0.12", default-features = false }
anyhow      = "1"
thiserror   = "2"
log         = "0.4"
flexi_logger = "0.31"
libc        = "0.2"            # ioctl winsize, optional signal handling

[target.'cfg(unix)'.dependencies]
nix = { version = "0.31", features = ["signal"] }   # optional: SIGTSTP/SIGCONT (Ctrl-Z)

[profile.production]
inherits = "release"
lto = "fat"
```

**Removed vs kitpdf:** `tokio`, `futures-util`, `memmap2`, `psx-shm`, `rustix`, `mimalloc`
(optional; can re-add), `lazy_static`. No `zstd` needed — `vivid_protocol` compresses raster
frames internally when the feature is negotiated.

### Tasks
- [ ] Replace the placeholder `vvrd/Cargo.toml` and `src/main.rs`.
- [ ] Add `vvrd/AGENTS.md` mirroring the repo pattern (invariants: media off PTY, no token leaks,
      bounded resources, source-scoped recovery; verification block).
- [ ] Confirm the build resolves against the local path crates; commit `Cargo.lock`.
- [ ] Update root `.gitmodules`/workspace listing only if the repo tracks vvrd like siblings
      (check `git status`; do not touch unrelated trees).

**Acceptance:** `cargo build` succeeds; `vvrd --help` prints usage; `vvrd --version` works.

---

## Phase 1 — One page on screen through Vivid (the vertical slice)

The riskiest integration first: prove a rendered page reaches the Vivido window over Vivid.

### Tasks
- [ ] `geometry.rs`: terminal grid ↔ pixel geometry. Port kitpdf `WindowSize` + `vivi`
      `terminal_geometry.rs` (`TIOCGWINSZ`, CSI `16t` fallback, `cells_for_pixels`, drawable area).
      Add a helper to reconcile local size with `session.display_state()`.
- [ ] `presenter.rs`: define the `Presenter` trait and `VividPresenter`:
  - `new(session) -> create viewport raster source (drawable px) + full-viewport grid-cell node`
    via `session.create_raster_source(id, w, h)`, `session.open_media_channel(&src,
    ConnectionKind::Raster)`, `session.create_scene_node(&SceneNodeConfig{ anchor_id: None,
    context_id: session.root_context_id(), x:0, y:0, width: cols<<32, height:(rows-1)<<32, .. })`.
  - `show_frame(&rgba)` → `session.send_raster_frame(&mut src, &mut chan, epoch, frame_id++, (w,h),
    rgba)`.
  - `set_node_visible(bool)`, `resize(new_dims)`, `teardown()`.
- [ ] `vivid_thread.rs`: own `ProducerSession` + `VividPresenter`; process `PresentCmd`; emit
      `PresentEvent`. For Phase 1 accept a raw `ShowFrame(rgba)`.
- [ ] `main.rs` (minimal): connect (D1 config, features from §6 of ARCHITECTURE), terminal guard
      (alt screen/raw/hide cursor), render **page 0 only** via a stripped `renderer.rs`, composite
      to the drawable buffer (fit-to-viewport, centered), `ShowFrame`, wait for a key, tear down.
- [ ] `--dry-run` / `--trace <dir>` wired through `ProducerConfig` for CI without a live presenter.

**Acceptance:** inside a Vivido window, `vvrd sample.pdf` shows page 1 filling the page area;
`q` exits cleanly with **no ghost image** left in the window. `--trace` writes control + raster
`.vivid` files. Verify against a real Unix socket, not only dry-run (a `PermissionDenied` sandbox
skip is **not** a pass — see `AGENTS.md`). **Smoke-test the same slice in a `vvmux` pane now** — this
is the earliest point the pane-local placement assumption (ARCHITECTURE §9) can be validated cheaply.

---

## Phase 2 — Render thread, navigation, prerender cache, loading

### Tasks
- [ ] Port kitpdf `renderer.rs` in full (MuPDF `Document`, `RenderNotif`/`RenderInfo`, windowed
      prerender, `InterleavedAroundWithMax`, watchdog, panic-caught per page, EPUB layout, TOC,
      metadata, links, export entry points). Keep the enums identical so the UI port is mechanical.
- [ ] Port kitpdf `app.rs` (page/scroll/zoom/pan state, residency pruning, search bookkeeping),
      dropping the Kitty `ImageId` fields; residency now bounds **CPU pixmaps**.
- [ ] `compositor.rs`: given a page pixmap + `ViewTransform{scroll, pan, zoom, crop, highlights}`,
      produce the viewport RGBA buffer. Reuse `image_pipeline.rs` (`crop_whitespace`,
      `apply_search_highlights`, scaling) and the crop/centre logic from kitpdf's
      `compute_page_surface`, writing into the buffer instead of a Kitty source-rect.
- [ ] UI event loop (port of kitpdf `enter_event_loop`/`handle_event`) minus tokio: use
      `crossterm::event::poll(timeout)` + `read()`; timers for loading-delay (90 ms) and resize
      debounce (120 ms) become deadline checks.
- [ ] `PresentCmd::ShowView{page, transform}` + Vivid-thread **coalescing** (drop superseded views);
      pull the latest ready pixmap for `page` from a shared cache, composite, send.
- [ ] Loading indicator: when the current page's pixmap isn't ready, `HideNode` + draw "Loading…"
      text after the delay (kitpdf's `should_delay_loading_indicator`).

**Acceptance:** page turns (space, arrows, `j/k`, PgUp/PgDn), go-to-page, and neighbor prerender
work; navigating a large PDF stays responsive; loading text appears only after the delay; scroll
within a tall page turns pages at the boundaries.

---

## Phase 3 — Display transforms (parity with kitpdf's view controls)

### Tasks
- [ ] Scroll (`↑/↓`, auto page-turn), pan (`h/l` in zoom), PgUp/PgDn viewport jumps — reuse `App`
      logic; compositor crops at the offset.
- [ ] Zoom (`z` toggle, `o/O`): send `RenderNotif::Area{ w×zoom, h×zoom }` so MuPDF re-rasterizes
      sharp; compositor crops the viewport region. No Kitty 10 000 px clamp needed (viewport-bounded
      frames); still clamp MuPDF render size to a sane max.
- [ ] Rotate (`r`), invert (`i`), custom/`-b`/`-w`, tint (`d`): MuPDF-side at render (unchanged);
      invalidate pixmaps + re-show.
- [ ] Auto-crop (`c`): `compositor.rs` crop-margins path; toggle invalidates + re-shows.
- [ ] EPUB font size (`<`/`>`) and reflow on resize: renderer EPUB layout path (unchanged).

**Acceptance:** every kitpdf view control behaves identically on screen; zoomed text is crisp;
invert/tint/rotate/crop update promptly without ghosting; EPUB reflows on font-size and resize.

---

## Phase 4 — Overlays, status bar, search UX

### Tasks
- [ ] `terminal.rs`: port status bar + `draw_toc`/`draw_metadata`/`draw_links`/`draw_loading` and
      overlay input modes. Every full-screen overlay first sends `PresentCmd::HideNode`, then draws
      text; exiting re-shows the page (D4 — glyphs render over media, so the node must be hidden).
- [ ] Search (`/`, `n/N`): renderer search flow (unchanged); highlights composited into the buffer;
      cross-page result counting + pending-jump resolution ported from `app.rs`/`main.rs`.
- [ ] TOC navigation (keys + mouse row mapping), metadata view, link listing + follow (internal jump
      / external `open`/`xdg-open` — **never** with the token in env), go-to-page, help (`?`),
      refresh (`R`/F5).

**Acceptance:** TOC/metadata/links/help/search overlays match kitpdf; no stale page shows behind an
overlay; following an internal link jumps and re-shows the page; external links open in the browser.

---

## Phase 5 — Resize, teardown, persistence, export, CLI completeness

### Tasks
- [ ] Resize: debounce; on settle recompute grid (reconcile with `display_state`), send `Area`,
      `PresentCmd::Resize` → recreate raster source at new dims + `UPDATE_NODE` geometry + re-send.
      Handle `DISPLAY_CHANGED`/`STALE_DISPLAY_GENERATION` (SDK already retries commits).
- [ ] Teardown guard on the Vivid thread: `delete_scene_node` → `destroy_source` → `goodbye` on
      normal exit, `q`/`Esc`/`Ctrl-C`, error, and panic. Panic hook restores the terminal (kitpdf's
      `SUPPRESS_PANIC_HOOK` pattern) **and** signals teardown.
- [ ] `state.rs` + `export.rs`: port verbatim (XDG cache state; `-e`/`e` export; `--probe-document`
      subprocess preflight).
- [ ] Full CLI parity: `-p/--page`, `-e/--export`, `-i/--invert`, `-b/-w` colours, `-h`, `--version`
      (clap with `env` for endpoint/token/bulk, plus `--dry-run`/`--trace`/`--verbose` like `vivi`).

**Acceptance:** resize reflows/re-renders once on settle with no ghost/torn frames; exit/panic never
leaves a lingering node in Vivido; state round-trips per file; export writes a valid PNG; all kitpdf
flags behave the same.

---

## Phase 6 — Robustness & the Vivid edge cases kitpdf never had

### Tasks
- [ ] **Source loss / credit exhaustion:** on `PresentEvent::SourceLost`, recreate the viewport
      source + rebind the node and re-send (source-scoped recovery). Surface transient errors in the
      status bar; never kill the session for one bad frame.
- [ ] **Visibility events** (optional feature): while the source is reported invisible/occluded,
      skip compositing+sending; re-send on becoming visible (saves work; spec §7.10 is advisory).
- [ ] **Heartbeat/close:** if the SDK reports the control connection closed/timed out, restore the
      terminal and exit with a clear message.
- [ ] **Ctrl-Z / SIGTSTP** (optional): suspend/resume cleanly (leave alt screen on stop, restore +
      re-show on continue), mirroring kitpdf's `nix` handling.
- [ ] **vvmux (required target):** vvrd must work in a `vvmux` **tiled pane and floating pane**
      (see ARCHITECTURE §9). No `vvmux`-specific code — only verify the design assumptions hold:
      pane-local placement lands correctly; **moving** a float triggers no source churn (only a
      **resize** recreates the source at new pane dims); a background tab / other-pane zoom / full
      occlusion yields `VISIBILITY=false` and pauses sends; the single full-viewport node stays under
      `MAX_NODE_FRAGMENTS` when partially occluded by floats. Keep to **one source + one node** per
      pane to respect per-pane budgets (16 producers / 64 sources / 256 nodes / 256 MiB).
- [ ] **Remote/bulk & tmux:** respect `VIVID_ENDPOINT_BULK` (SDK handles fallback); under tmux/screen
      keep grid-cell nodes (no anchors needed). Verify a `vvssh` session and `vvssh`-into-`vvmux`.
- [ ] Regression tests for: coalesced-frame correctness, resize source-recreate, teardown-on-panic,
      source-loss recovery, credit backpressure not blocking input.

**Acceptance:** runs correctly in a `vvmux` tiled **and** floating pane (move/resize/occlude/
tab-switch behave); kill/restart the presenter mid-session → vvrd reports it and exits cleanly;
occluding the pane pauses sends; remote `vvssh` (and `vvssh`→`vvmux`) renders; no input stall under
slow credit.

---

## Phase 7 — (Optional) per-page cached sources for remote bandwidth

Behind the same `Presenter` seam, add a strategy that mirrors kitpdf's residency: one **encoded-image
(PNG) or raster source per page**, transmitted once, re-displayed by committing/showing its node;
scroll/zoom via an **oversized node + negative grid origin + `NODE_CLIP_RECT_V1`** instead of
re-sending pixels. Select the strategy by config/heuristic (e.g., enable on `VIVID_REMOTE`).

### Tasks
- [ ] `Presenter` impl `CachedPagePresenter`: bounded map of page→(source,node); create on demand,
      destroy far pages (port `app.rs` residency); `NEED_KEYFRAME`/loss recreate. **Bound residency
      to the per-pane `vvmux` budget** (≤64 sources, ≤256 MiB) — the single-node framebuffer model
      has no such exposure, so this strategy must degrade to it under a tight pane budget, and keep
      the visible node count low to avoid the `MAX_NODE_FRAGMENTS` drop under float occlusion.
- [ ] Scroll/zoom via node geometry + clip; verify `contain`-fit + clip math against the framebuffer
      reference for pixel parity on a few pages.
- [ ] Benchmark navigation bytes vs the framebuffer model over a simulated-latency socket.

**Acceptance:** identical on-screen result to the framebuffer model with materially lower bytes-per
page-turn on remote; feature-gated and off by default.

---

## Phase 8 — DOCX/PPTX preparation through LibreOffice

Office viewing is a startup adapter into the existing fixed-page MuPDF path. It does not add a
second renderer or change Vivid presentation.

### Tasks

- [x] Classify DOCX/PPTX case-insensitively and reject DOCM/PPTM.
- [x] Discover `soffice`/`libreoffice`, with `--soffice` and `VVRD_SOFFICE` override support.
- [x] Stage the source under a generic name in a private temporary directory; use an isolated
      LibreOffice profile and remove all Vivid endpoint/token variables from the child.
- [x] Export with `pdf:writer_pdf_Export` or `pdf:impress_pdf_Export`; validate and bound the
      generated PDF before MuPDF preflight.
- [x] Enforce 512 MiB input, 1 GiB output, 16 KiB diagnostics, and a 120-second default timeout
      (CLI maximum 3600); terminate/reap the process group on timeout or output overflow.
- [x] Preserve original-path identity for state, title, capture policy, and export naming while the
      temporary PDF remains alive through render shutdown.
- [x] Generate DOCX/PPTX fixtures with embedded PNGs and provide ignored real-LibreOffice tests
      that assert the embedded red/blue pixels survive conversion and MuPDF rendering.
- [x] Document the local LibreOffice requirement and static-slide semantics.

**Acceptance:** DOCX and PPTX page/slide images, including embedded pictures, use all existing
fixed-page controls and PNG export. No conversion artifact persists after exit, and presentation
animations/audio/video are not claimed or played.

---

## File-by-file port checklist (kitpdf → vvrd)

| kitpdf file | vvrd action | notes |
|---|---|---|
| `main.rs` | Rewrite loop, de-tokio | keep CLI parse, panic hook, key handling; swap draw path for `PresentCmd` |
| `app.rs` | Port, drop `ImageId`/`pending_image_deletes` | residency now bounds pixmaps |
| `renderer.rs` | Port ~verbatim | MuPDF `!Send` thread, watchdog, EPUB, search, TOC, meta, links, export |
| `image_pipeline.rs` | Fold into `compositor.rs` | keep crop/highlight/interleave; output a buffer, not a Kitty image |
| `kitty.rs` | **Delete** → `presenter.rs` + `vivid_thread.rs` | all Kitty/SHM/tmux/response code gone |
| `terminal.rs` | Port | add `HideNode` before overlays; status bar unchanged |
| `state.rs` | Port verbatim | XDG per-file state |
| `export.rs` | Port verbatim | PNG export |
| `error.rs`, `perf.rs` | Port verbatim | add Vivid error variants |
| — | **New** `geometry.rs` | merge kitpdf sizing + `vivi` `terminal_geometry.rs` |
| — | **New** `presenter.rs`, `vivid_thread.rs` | the Vivid layer |
| — | **New** `office.rs` | isolated LibreOffice DOCX/PPTX-to-PDF preparation and cleanup |

---

## Verification (run from `vvrd/`, per repo `AGENTS.md`)

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

- Port kitpdf's unit tests that survive the model change: `app.rs` (scroll/zoom/pan/search/
  residency), `renderer.rs` (`scale_fit`, EPUB layout, jump/rerender, invalid-document exit),
  `compositor.rs` (crop/highlight parity, `InterleavedAroundWithMax`), CLI parsing, color parsing,
  geometry (`cells_for_pixels`, CSI parse).
- Add Vivid-specific tests: frame coalescing keeps only the latest view; resize recreates the source
  with matching dims; teardown emits delete-node + destroy-source + goodbye; source-loss triggers
  recreate; `frame_id` strictly increases.
- With LibreOffice installed, run
  `VVRD_TEST_SOFFICE=/path/to/soffice cargo test office::tests::libreoffice_ -- --ignored` and
  require both generated embedded-image fixtures to pass.
- Because vvrd changes wire behavior only through `vivid_sdk`/`vivid_protocol` (which it does not
  modify), the cross-project rule is light — but **do run the real Vivido presenter path** manually;
  a socket test that hit `PermissionDenied` in a sandbox must be rerun where socket creation is
  allowed. Use the `/verify` and `/run` skills to drive the app.
- **vvmux test matrix (required):** in `vvmux`, open a PDF, EPUB, DOCX, and PPTX and confirm each of —
  (1) tiled pane renders and navigates; (2) **floating** pane renders; (3) **move** the float (page
  stays put, no flicker / no source recreate); (4) **resize** the float/tiled pane (re-renders once
  on settle at new pane dims); (5) another pane **zoomed** or a **background tab** hides vvrd → sends
  pause, restore on return; (6) a float **partially occluding** a tiled vvrd shows the page correctly
  cropped (not squished, not dropped); (7) exit leaves **no ghost node** in the pane. Also verify
  `vvssh` and `vvssh`→`vvmux`.

---

## Risks & open questions

1. **Per-scroll retransmit cost (framebuffer model).** Mitigated by zstd + one-frame-at-a-time
   credit + coalescing on local sockets; Phase 7 addresses remote. *Validate early on `vvssh`.*
2. **Grid authority (local size vs `display_state`).** They should match under Vivido; reconcile on
   `DISPLAY_CHANGED`. Confirm no off-by-one between the status row and node height.
3. **Glyph-over-media layering.** Confirmed by spec (§7.7, single legal text layer) and
   `vivido/src/display`. The HideNode-before-overlay rule is mandatory; test that no page pixels
   bleed through overlays.
4. **MuPDF Rust version vs SDK/protocol (1.86 vs 1.85).** Take the max; verify the workspace builds.
5. **Raster admissibility on huge zoom.** Framebuffer frames are viewport-bounded (safe); still
   clamp MuPDF render size and honor each source's `max_media_body`.
6. **Ctrl-C in raw mode.** Delivered as a key event (handle like kitpdf); ensure teardown runs on
   every exit path including panic.
7. **`vvmux` nesting (required target).** Verified against `vvmux/src`
   (`media.rs::projection_snapshot`, `session.rs::project_logical_node`, `bridge.rs`): the virtual
   presenter treats vvrd's nodes as **pane-local**, offsets by the pane origin, clips to the pane,
   and coalesces raster to the latest body — so the single full-viewport framebuffer node is not
   only compatible but optimal. Watch items, not blockers: (a) a partially-occluded node dropped
   past `MAX_NODE_FRAGMENTS` (8) — keep to one node; (b) per-pane budgets (64 sources / 256 MiB) —
   trivial for the framebuffer model, a real constraint for the Phase-7 cache; (c) float **move** vs
   **resize** — only resize should recreate the source. Confirm all in the vvmux test matrix above.

---

## Suggested delivery order (summary)

`Phase 0 → 1` proves the integration (one page over Vivid, clean teardown). `Phase 2 → 3` reaches
interactive reading parity with kitpdf's view controls. `Phase 4 → 5` completes the UX (overlays,
search, resize, persistence, export, CLI). `Phase 6` hardens the Vivid-specific edges. `Phase 7` is
an optional bandwidth optimization behind the `Presenter` seam. Ship parity at the end of Phase 5;
Phase 6 is required for a robust release; Phase 7 is opportunistic.
