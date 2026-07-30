# vvrd — Vivid 1.1 → 1.5 migration plan

**Status: executed.** vvrd is on Vivid 1.5. `cargo fmt --all --check`, `cargo test --all-targets`
(47 tests, including live-socket regressions), and `cargo clippy --all-targets -- -D warnings` pass
in `vvrd/`, and the `vivid_sdk` and `vivi` suites pass alongside the SDK change. `vivid_protocol` was
not modified. The plan below is retained as the design record; §10 lists where the implementation
deviated from it.

**Scope:** `vvrd/` plus three small additive `vivid_sdk` APIs it depends on.
**Reference implementations:** `vivi/` (migrated producer), `vivido/` (migrated presenter).
**Normative sources:** `vivid_protocol/vivid-protocol-1.5-{core,media,terminal-surface}.md`,
`vivid_protocol/vivid-protocol-1.1-to-1.5-migration.md`.

---

## 0. Starting state

vvrd currently does not build against the migrated crates:

```
error: failed to select a version for the requirement `vivid_sdk = "^0.2"`
candidate versions found which didn't match: 1.5.0
```

`vivid_protocol` is 1.5 (`surface`, `track`, `identity`, `lease`, `auth`, `registry` modules) and
`vivid_sdk` is 1.5.0 (`Session`/`Surface`/`Track`/`TrackChannel`). vvrd still uses the deleted 1.1
surface: `ProducerSession`, `SourceHandle`, `MediaSender`, `SourceDescriptor`, `SourceEvent`,
`DisplayState`, `messages::FEATURE_*`, `messages::CREATE_RASTER`, ticket attachment, and
incremental credits. There is no compatibility shim and none should be written — 1.5 is a separate
protocol, per migration guide §2.

Protocol work required: **none**. The 1.5 wire records vvrd needs (`CREATE_SURFACE`,
`UPDATE_SURFACE`, `CREATE_TRACK`, `ACTIVATE_TRACK`, `ADVANCE_CHANNEL`, `CREATE_NODE`,
`UPDATE_NODE`, `DELETE_NODE`, `RASTER_FRAME`, `NEED_FULL_FRAME`) all exist in
`vivid_protocol::registry::record` and are all implemented by the migrated Vivido presenter
(`vivido/src/vivid/mod.rs:1102`, `:1117`). No spec change, no `vivid_protocol` change.

---

## 1. Object-model mapping

| vvrd concept (1.1) | vvrd concept (1.5) | Lifetime |
|---|---|---|
| One raster source per viewport geometry | One **surface** for the whole process | Created once at startup, destroyed at teardown |
| Source ID changes on every settled resize | Surface ID **never** changes | Stable across resize, track loss, channel loss |
| `SourceDescriptor` on the source | `SurfaceDescriptor` on the surface | Updated in place via `UPDATE_SURFACE` |
| Capture policy on the source | Policy on the surface (strictest union) | Set at create, never relaxed |
| Scene node referencing source ID | Scene node referencing **surface** identity | Created once, `UPDATE_NODE` for geometry/visibility |
| Media ticket + `ATTACH_CHANNEL` | Authenticated `CHANNEL_OPEN` / `CHANNEL_ACCEPTED` | One channel generation per track attachment |
| Incremental `CREDIT` | Cumulative `MAX_CHANNEL_DATA` on the track channel | Reset only on channel-generation advance |
| Source epoch bump on resize | New **track** in slot 3, activated atomically | One live raster track active at a time |
| `SOURCE_LOST` → replace source | `TRACK_LOST` → replace track, surface untouched | — |

Concretely, vvrd becomes:

- **1 surface**: `context_id = session.info().root_context_id`, `surface_id = allocate_id()`,
  `semantic_profile = generic-content-v1`, `coordinate_model = DesktopLogicalPixels`,
  `logical_width/height` = page-area pixels, `descriptor.role = SurfaceRole::Document`.
- **1 scene node**: terminal grid-cell space (coordinate space `1`), signed 32.32 cell geometry,
  text layer `1` (between background and glyphs), covering `cols × page_rows`.
- **N raster tracks over time**, at most one active in **slot 3 (`raster`)**, `TrackMode::Live`,
  `LaneClass::Bulk`, `RasterConfiguration { width, height, alpha_mode: 1, delta_enabled,
  maximum_delta_operations: 16, zstd_enabled }`.
- **1 `TrackChannel`** per track channel generation.

Rationale for `generic-content-v1` + `DesktopLogicalPixels` (not `terminal-content-v1` +
`TerminalContentCells`): vvrd renders a pixel framebuffer that is *placed* in cells, exactly like
`vivi`'s image path (`vivi/src/image_viewer.rs:127`). `terminal-content-v1` describes surfaces
whose own content is a cell grid, which vvrd's page raster is not.

---

## 2. Blocking dependency: two additive `vivid_sdk` APIs

`vivid_sdk` 1.5.0 exposes `Session::create_node` and `Session::place_terminal_surface` but **no
node update or delete**. vvrd needs both: it toggles node visibility when text overlays are shown
(`src/main.rs:428`, `:741`) and resizes node geometry on every unsettled drag step, and it deletes
the node during bounded teardown.

Add to `vivid_sdk/src/lib.rs`, by extracting the existing `create_node` transaction body into a
private helper:

```rust
fn scene_transaction(
    &mut self,
    owning_context_id: u64,
    mutation: u16,           // CREATE_NODE | UPDATE_NODE | DELETE_NODE
    object_id: u64,          // node ID
    payload: PayloadMap,
    metadata: &RequestMetadata,
) -> io::Result<SceneCommit>;

pub fn create_node(&mut self, node: &SceneNode, metadata: &RequestMetadata) -> io::Result<SceneCommit>;
pub fn update_node(&mut self, node: &SceneNode, metadata: &RequestMetadata) -> io::Result<SceneCommit>;
pub fn delete_node(&mut self, context_id: u64, node_id: u64, metadata: &RequestMetadata)
    -> io::Result<SceneCommit>;
```

- `update_node` reuses `SceneNode::payload()` unchanged; Vivido decodes `CREATE_NODE` and
  `UPDATE_NODE` with the same `SceneNode::decode` path.
- `delete_node` sends the two-key payload `{0: context_id, 1: node_id}` that Vivido's
  `DELETE_NODE` arm validates.
- All three keep the existing commit behaviour: `expected_target_generation` from the cached
  target, scene-revision precondition, abort-on-error, and `SCENE_PRESENTED` bookkeeping.

**Optional (recommended, not blocking):** `TrackChannel::flow_snapshot() -> ChannelFlow` so a
producer can size its own coalescing queue, and a bounded/`try_` raster send so a flow-blocked
writer cannot stall control-event servicing (see §7 risk R2). vvrd can ship without both by
sizing its queue from the in-flight claim it declares itself.

Per root `AGENTS.md`, an SDK change means running `vivid_sdk`, `vivi`, and `vvrd` suites.

---

## 3. File-by-file work

### 3.1 `Cargo.toml`

```toml
version = "0.3.0"
rust-version = "1.87"          # vivid_protocol 1.5 requires it

vivid_sdk = { version = "1.5", path = "../vivid_sdk" }
vivid_protocol = { version = "1.5", path = "../vivid_protocol" }
```

Everything else (mupdf, image, rayon, flume, crossterm, clap) is unchanged. Regenerate
`Cargo.lock` and keep it tracked.

### 3.2 `src/geometry.rs` — target descriptor replaces `DisplayState`

`vivid_sdk::DisplayState` is gone. Terminal metrics now live in the **terminal target profile
descriptor**, `session.info().target_descriptor`, keys 0–8: target pixel width/height, grid
columns, grid rows, cell width, cell height, settled flag, anchor marker version (must be `3`),
maximum active anchors.

Replace `WindowSize::current(DisplayState)` with, mirroring `vivi/src/terminal_geometry.rs:53`:

```rust
impl WindowSize {
    /// `WouldBlock` when the descriptor is valid but not settled.
    pub fn from_target_descriptor(descriptor: &PayloadMap) -> io::Result<(Self, bool)>;
    /// crossterm/ioctl fallback when the presenter descriptor is unusable.
    pub fn from_terminal() -> Self;
}
```

Keep `page_rows()`, `page_area_*_px()`, `framebuffer_len()`, and the two existing unit tests
verbatim — they are pure geometry and carry no protocol coupling. Add a test asserting an
unsettled descriptor yields `WouldBlock` and a settled one yields the expected cells, matching
`vivi/src/terminal_geometry.rs:466`.

### 3.3 `src/presenter.rs` — the core rewrite

New shape:

```rust
pub struct VividPresenter {
    session: Session,
    surface: Surface,
    node_id: u64,
    track: Track,                 // active raster track
    channel: Option<TrackChannel>,
    surface_viewport: WindowSize, // geometry the active track was configured for
    node_viewport: WindowSize,    // geometry the scene node currently claims
    visible: bool,
    epoch: u32,
    frame_id: u64,
    force_full_frame: bool,
    accumulated_damage_pixels: u64,
    recovery_reason: Option<u64>,
    torn_down: bool,
    policy: u64,
    descriptor: SurfaceDescriptor,
    semantic_state: (usize, Option<String>),
}
```

`resize_action`, `command_queue_capacity`, and `accumulated_damage_exceeds_fraction` keep their
current logic and tests; only what they act on changes (`ReplaceSource` → `ReplaceTrack`).

#### Startup

1. `create_surface(document_surface(...))` → `Surface`.
2. `create_track(raster_track(...))` → prime: `open_track_channel`, first full frame,
   `wait_track(MilestoneSet, MILESTONE_OUTPUT_READY)`.
3. `activate_tracks(&surface, &[SlotBinding { slot: 3, track_id, expected_channel_generation,
   required_milestone: MILESTONE_OUTPUT_READY }])`.
4. `create_node(&terminal_node(..., visible: true))`.

Startup submits its first frame *before* activation so the surface is never activated onto a track
with no decoded output (media spec §7: "the common live-media replacement requirement is
milestone 4").

#### `show_frame` — mostly retained

Retained byte-for-byte: full/delta decision, accumulated-damage half-area rule, damage logging.
Changes:

- `MediaSender::send_raster*` → `TrackChannel::send_raster(epoch, frame_id, rgba, compress)` and
  `send_raster_delta(epoch, frame_id, base_frame_id, pts_us, duration_us, operations, compress)`.
  Live raster passes `pts_us: 0, duration_us: 0`.
- There is no 1.5 `send_raster_delta_or_full`. vvrd keeps its own fallback: build the delta body
  size estimate from the planned operations and send a full frame when the delta is not smaller,
  or when `send_raster_delta` rejects it. Keep `RasterSendKind` as a vvrd-local enum.
- **Delete `session.wait_until_visible(...)`.** 1.5 has no push visibility for a source, and flow
  control now provides the backpressure that gate approximated: `write_charged_record` blocks
  until `MAX_CHANNEL_DATA` raises the cumulative maximum.
- The first frame of every channel generation must be a full frame. The SDK enforces this
  (`send_raster_delta` errors while `needs_recovery`), so vvrd must set `force_full_frame` from
  the same events that set `needs_recovery` — see `take_full_frame_request` below — rather than
  discovering it as a send error.

#### `resize` — track replacement under a stable surface

This is the single largest semantic change, and it removes the visible blank that 1.1 source
replacement caused.

- **Unsettled drag step** (`ResizeAction::UpdateNode`): `update_node` with new cell geometry only.
  No surface, track, or channel change. Same as today.
- **Settled resize** (`ResizeAction::ReplaceTrack`), ordered:
  1. `create_track` with the new `RasterConfiguration { width, height }` and a fresh track ID —
     raster dimensions are immutable, so new geometry always means a new track.
  2. `open_track_channel(&new_track)`; send one full frame at `epoch = 1, frame_id = 1`.
  3. `wait_track(&new_track, MilestoneSet, MILESTONE_OUTPUT_READY, timeout)`.
  4. `update_surface` with the new `logical_width/height` — this advances the **surface
     generation** (coordinate truth changed) but not surface identity.
  5. `activate_tracks(&surface, &[SlotBinding { slot: 3, track_id: new, .. }])` — atomic
     compositor-boundary swap.
  6. `update_node` if `cols`/`page_rows` changed.
  7. `destroy_track(&old_track)`, then drop the old `TrackChannel`.

  Step 7 preserves the existing ordering invariant that made the 1.1 resize regression pass: the
  media transport must outlive the ordered destroy request, or a relay removes the object on EOF
  and rejects the destroy. Keep the current comment and the regression that proves it.

  Failure handling: if any of 1–3 fails, `destroy_track` the half-built replacement and keep the
  current track active — the user sees a stale-size frame, not a blank. If 5 fails, the old track
  is still active and still valid; retry or fall back to the old geometry.

#### Recovery paths (three distinct ones, where 1.1 had one)

| Trigger | 1.1 response | 1.5 response |
|---|---|---|
| `NEED_FULL_FRAME` | `require_full_frame` | Unchanged: `force_full_frame = true`, next send is full |
| Channel loss (send `BrokenPipe`, `ChannelEvent::Error`, closed writer) | Replace whole source | `advance_channel(&track, reason)` → `open_track_channel` → full frame. **No** new surface, node, or track |
| `TRACK_LOST` for our track | `SourceEvent::Lost` → replace source | Create replacement track (same geometry), prime, `activate_tracks` slot 3, destroy lost track. Surface, node, descriptor, policy untouched |

`recover_source()` becomes `recover_track()` and `recover_channel()`. `require_full_frame` and
`take_full_frame_request` survive, backed by `TrackChannel::take_event()` returning
`ChannelEvent::NeedFullFrame` / `NeedKeyframe` instead of `MediaSender::take_full_frame_request`.

#### `set_visible` / `teardown`

- `set_visible` → `update_node` with `visible` toggled (needs SDK §2).
- `teardown` → `delete_node` → `destroy_track` → drop channel → `destroy_surface` → `session.close()`.
  Keep the "collect first error, continue" structure and the drop-after-destroy ordering.

#### `update_content_descriptor`

`update_source_descriptor` → `update_surface` carrying an unchanged mapping and a bumped
`descriptor.semantic_content_revision`. Because `Session::update_surface` advances the surface
generation only when mapping fields change, a descriptor-only update correctly leaves the
generation alone.

#### `observe_recovery`

`session.supports(messages::FEATURE_OBSERVABILITY_CORE_V1)` → `session.supports(OBSERVABILITY)`;
`query_source` → `query_track`, logging `TrackStatus { milestones, channel_generation,
last_media_id, last_media_record_sequence }`. Milestones are **generation-local** — always log the
generation with them and never treat a bit from an old generation as current readiness.

#### `command_queue_capacity`

`MediaQueueLimits` is gone. Derive the bound from the claim vvrd itself declares in
`TrackConfiguration`: `maximum_inflight_body_bytes / frame_bytes`, floored at 1. Keep the existing
unit test with the new input type.

### 3.4 `src/vivid_thread.rs`

- `ProducerSession` → `Session`; `SourceDescriptor` → `SurfaceDescriptor`.
- `PresentEvent::SourceLost` → `PresentEvent::TrackLost`; add `PresentEvent::ChannelRecovered` if
  the reader should log it (optional).
- **Drop `PresentEvent::Visibility`.** Nothing pushes source visibility in 1.5.
- `service_source_events` → `service_presenter_events`, now draining **two** queues:
  - `session.take_event()` → `SessionEvent::TargetChanged` (apply, recompute viewport, resize),
    `TrackLost` (filter by our complete `(context_id, surface_id, track_id)` tuple — never by
    `track_id` alone), `ConnectionClosed` (fatal, stop the thread).
  - `channel.take_event()` → `NeedFullFrame` / `NeedKeyframe` (arm full frame), `Error`.
- The display-poll block at the top of `run()` (`presenter.display_state()` comparison) is
  replaced by `SessionEvent::TargetChanged` handling; `WindowSize::from_target_descriptor` then
  drives the same `presenter.resize(viewport, settled)` call as today.
- `next_command` coalescing, deferred queue, and the "drop a view composed for a stale viewport"
  guard are unchanged.

### 3.5 `src/main.rs`

- `producer_config`:

  ```rust
  ProducerConfig {
      endpoint_control: std::env::var("VIVID_ENDPOINT_CONTROL").ok(),
      endpoint_bulk: std::env::var("VIVID_ENDPOINT_BULK").ok(),
      authentication: ProducerAuthentication::RootFromEnvironment, // reads VIVID_ROOT_SECRET
      producer_name: "vvrd".into(),
      producer_version: env!("CARGO_PKG_VERSION").into(),
      target_profile: TERMINAL_SURFACE.into(),
      required_profiles: vec![LIVE_MEDIA.into(), TERMINAL_SURFACE.into(), CORE_CONTROL.into()],
      optional_profiles: vec![OBSERVABILITY.into()],
      dry_run: cli.dry_run,
      trace_dir: cli.trace.clone(),
      ..ProducerConfig::default()
  }
  ```

  Required profiles must be sorted, unique, prerequisite-closed, and contain both `CORE_CONTROL`
  and the selected target profile — `ProducerConfig::validate` rejects anything else. The eight
  1.1 `FEATURE_*` constants map to exactly three profiles; there is no per-feature negotiation any
  more, so the D7 graceful-fallback logic in `docs/ARCHITECTURE.md` collapses to two checks:
  `session.info().target_profile == TERMINAL_SURFACE`, and `session.supports(OBSERVABILITY)`.
  Raster delta and zstd are now per-track configuration flags, not session features — probe them
  with `probe_track` and fall back to a non-delta track configuration if unsupported.

- After connect, validate the selected target like `vivi/src/client.rs:22`: reject a presenter
  that did not select `terminal-surface-v1`.
- `SourceDescriptor` → `SurfaceDescriptor { role: SurfaceRole::Document, title,
  semantic_content_revision: 1, semantic_availability: 0b0000_1101 (text | links | outline),
  locator_hint }`. Availability bits in 1.5 are text (0), structure (1), links (2), outline (3),
  actions (4).
- `document_capture_policy`: `messages::CAPTURE_POLICY_MASK` → `POLICY_DENY_CAPTURE |
  POLICY_DENY_POSTER_RETENTION | POLICY_DENY_IMAGE_CACHE | POLICY_DENY_DESCRIPTOR_EXPORT`. The
  sensitive-path heuristic is unchanged. Effective policy is a strictest union and can never be
  relaxed by a later `UPDATE_SURFACE`.
- `document_title`: `messages::MAX_SOURCE_DESCRIPTOR_TITLE_BYTES` → the 1.5 limit of 256 UTF-8
  bytes (`SurfaceDescriptor::validate`).
- `probe_document` scrubs `VIVID_TOKEN` from the child environment; it must scrub
  `VIVID_ROOT_SECRET` (and, for completeness, the four `VIVID_ENDPOINT_*` names) instead. **Do not
  drop the old name** — remove `VIVID_TOKEN` too so a stale 1.1 variable is not inherited.
- Event-loop arms for `PresentEvent::Visibility` are removed; `runtime.node_visible` stays as
  vvrd-owned state that only vvrd's own overlay logic writes.
- `wait_for_presenter` / `wait_for_document` / `run_event_loop` otherwise unchanged.

### 3.6 Untouched

`compositor.rs`, `renderer.rs`, `app.rs`, `semantic.rs`, `state.rs`, `export.rs`, `terminal.rs`
have no Vivid coupling beyond the `WindowSize` type. Expect only call-site churn where
`WindowSize::current` was constructed.

### 3.7 Docs

- `docs/ARCHITECTURE.md`: rewrite D1 (SDK object model), D3 (viewport framebuffer → surface +
  raster track slot), D4 (node placement), D7 (features → profiles), §6 (feature set → profile
  set), §8 (lifecycle sequences: startup, resize, track loss, channel recovery, teardown), §9
  (nested vvmux — see §8 below).
- `docs/IMPLEMENTATION_PLAN.md`: mark the 1.1 milestones historical, link this document.
- Add `vvrd/AGENTS.md`. The root `CLAUDE.md` table already points at it and it does not exist.

---

## 4. Sequencing

Ordered so each step compiles and tests before the next.

| Step | Work | Exit criterion |
|---|---|---|
| 1 | `vivid_sdk`: `update_node` / `delete_node` + unit tests | `vivid_sdk` and `vivi` suites green |
| 2 | vvrd `Cargo.toml` bump; `geometry.rs` target descriptor | Crate resolves; geometry tests green |
| 3 | `presenter.rs`: surface + node + first track + `show_frame`, offline (`dry_run`) only | Offline presenter test submits full and delta frames |
| 4 | `presenter.rs`: settled-resize track replacement | Resize test asserts surface ID stable, track ID changed, node updated |
| 5 | `presenter.rs`: `TRACK_LOST` and channel-advance recovery | Recovery tests green |
| 6 | `vivid_thread.rs`: dual event servicing, `TargetChanged` resize | Thread tests green |
| 7 | `main.rs`: config, descriptor, policy, env scrubbing, event arms | `cargo test --all-targets` green |
| 8 | Live-socket harness: ordering + two-owner isolation regressions | See §5 |
| 9 | Docs, `AGENTS.md`, manual runs against Vivido | Manual checklist §6 |

Steps 3–5 can each land independently; step 8 is the largest single test-infrastructure item.

---

## 5. Test plan

### 5.1 Offline (`ProducerConfig::offline()`) — covers most logic

`Session::connect_offline` synthesizes `SURFACE_READY`, `TRACK_READY`, `WAIT_SATISFIED`, and
`TRACK_ACTIVATED`, and gives channels a sink connection, so the whole resize/replace/recover flow
is testable without a socket. Port and extend the existing offline tests:

- `surface_survives_settled_resize`: resize settled → surface ID and node ID unchanged, track ID
  changed, `epoch` reset to 1, `frame_id` reset, `force_full_frame` armed.
- `delta_after_full_frame_and_forced_full_after_recovery` (port of
  `source_epoch_and_recovery_paths_reestablish_a_full_frame`).
- `accumulated_damage_forces_a_full_frame_only_after_half_area` (unchanged).
- `command_queue_capacity_obeys_the_declared_inflight_claim`.
- `descriptor_update_does_not_advance_surface_generation`.
- `channel_recovery_advances_generation_and_requires_a_full_frame`.

### 5.2 Live socket harness

A 1.5 fake presenter is more work than the 1.1 one: it must run the authentication transcript.
`vivid_protocol::auth` exposes every server-side primitive needed —
`verify_root_hello_proof`, `extract_handshake_prk`, `derive_session_keys`,
`welcome_confirmation`, `channel_tag` / `verify_tag`. Build it once in
`vvrd/tests/support/fake_presenter.rs` (~350 lines) and reuse it for:

- **Lifecycle ordering regression** (port of
  `resize_and_teardown_destroy_sources_before_closing_media_connections`): assert the track
  channel stays open until `DESTROY_TRACK` is answered, for both the resize replacement and
  teardown, and that `DESTROY_SURFACE` follows `DELETE_NODE`.
- **Two-owner isolation regression** (required by root `AGENTS.md`): two sessions on one fake
  presenter, deliberately reusing **identical numeric** context, surface, track, node, and channel
  IDs. Emit `TRACK_LOST` for owner A's track and assert owner B's surface revision/generation,
  node, active slot, channel generation, cumulative flow counters, and next accepted frame are all
  unchanged; then assert owner A recovered onto a new track under its **original** surface ID.
- **Cumulative-flow regression**: duplicate `MAX_CHANNEL_DATA` records are harmless; a maximum
  below the already-sent total is rejected; totals reset only on `ADVANCE_CHANNEL`.
- **Stale target generation**: reply `STALE_TARGET_GENERATION` to a node commit, push
  `TARGET_CHANGED`, assert vvrd applies it and retries once (the pattern at
  `vivi/src/terminal_geometry.rs:223`).

Socket tests that hit `PermissionDenied` in a sandbox must be rerun where socket creation is
permitted; a skipped socket path is not evidence.

### 5.3 Verification commands

From `vvrd/`, and from `vivid_sdk/` for step 1:

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Because step 1 touches the SDK, also run `vivi`'s suite (`vivi/`: same three commands) to prove
the shared transaction refactor did not regress the migrated producer.

---

## 6. Manual acceptance against Vivido

1. `vvrd document.pdf` in Vivido: first page renders, no blank flash.
2. Page navigation: deltas dominate, occasional full frames, no `NEED_FULL_FRAME` storm.
3. Drag-resize the window: node tracks the drag, one track replacement on settle, **no blank
   frame** at the swap (the 1.5 improvement over 1.1).
4. Text overlay (help/TOC/search): node hides and restores.
5. `q` / `SIGTERM`: `DELETE_NODE` → `DESTROY_TRACK` → `DESTROY_SURFACE` → `GOODBYE`, no presenter
   error, terminal restored.
6. `--dry-run` and `--trace <dir>` still produce a complete record stream.
7. Sensitive path (e.g. `~/.ssh/doc.pdf`): presenter reports the deny-capture policy in effect.

---

## 7. Risks

- **R1 — SDK node APIs are on the critical path.** Nothing in vvrd's scene handling works without
  §2. Land it first; it is ~80 lines of extraction plus two thin wrappers.
- **R2 — A flow-blocked send stalls control-event servicing.** `TrackChannel::write_charged_record`
  waits on a condvar with no timeout. vvrd's single Vivid thread would therefore not process
  `TARGET_CHANGED` or `TRACK_LOST` while starved. 1.1 had the same shape with credits, so this is
  not a regression, but it is worth an SDK `send` deadline or a dedicated control-drain thread if
  it shows up in practice.
- **R3 — Track replacement cost on resize.** Every settled resize now allocates a track, a channel,
  and a decoder-side raster surface, and waits for `MILESTONE_OUTPUT_READY` before swapping. Under
  rapid resize this is heavier than the 1.1 source swap. The existing settle debounce
  (`resize_action`) is what keeps it bounded — do not weaken it. Cap in-flight replacements at one.
- **R4 — Test-harness effort.** The live 1.5 harness is the biggest single item. Keep as much
  coverage as possible in offline mode and build the harness once, reusably.
- **R5 — Two-owner isolation is a hard requirement, not a nicety.** vvrd is single-surface, so it
  is tempting to key state by `track_id`. Every predicate must use the complete
  `(context_id, surface_id, track_id)` tuple; `TRACK_LOST` filtering is the specific place this
  bug would appear.

---

## 8. Explicitly out of scope

- **`vvmux` nesting.** vvmux is still 1.1 and is not part of this migration. A 1.5 vvrd will not
  run inside a 1.1 vvmux, and no 1.1↔1.5 adapter should be written for it (migration guide §2.3:
  such an adapter is a terminating gateway with reduced guarantees, never the semantic reference).
  `docs/ARCHITECTURE.md` §9 should say so plainly until vvmux migrates.
- **Desktop input.** vvrd input stays pure PTY stdin (D6). `desktop-input-v1` requires
  `desktop-surface-v1`, which vvrd does not select.
- **Timed media, session leases, delegated contexts, web bindings, anchors.** vvrd places its node
  in grid-cell space and needs none of them. Anchor placement (marker v3) is a possible later
  enhancement for inline document embedding, not part of this migration.
- **Any change to `vivid_protocol` or the 1.5 specification.**

---

## 10. Deviations from the plan, as implemented

Recorded because each one changed a decision the plan had made differently.

1. **A third SDK addition was needed.** `Track::delta_operation_limit()` exposes the *granted*
   limit from `TRACK_READY`. Without it a producer can only plan against the limit it requested, and
   a presenter that grants less would make every delta fail and silently degrade to full frames.

2. **`WindowSize::from_target_descriptor` returns unsettled geometry instead of `WouldBlock`.**
   The plan copied `vivi`'s settle-gated reader, but vvrd needs the transient geometry: it follows a
   drag with `UPDATE_NODE` and replaces the track only on settle. Rejecting unsettled descriptors
   would have deleted that behaviour. Structural validation (key set, marker version 3, nonzero
   dimensions, at least two rows) still rejects.

3. **A transport-failure path was missing from the plan.** The SDK reports a dead track connection
   only as a send failure — no `ChannelEvent` is queued — so channel recovery would never have been
   reached and a recoverable failure would have killed the reader. `classify_send_failure` turns a
   transport-kind send error into a `ChannelLost` signal.
   Covered by `a_dead_track_transport_becomes_an_actionable_channel_loss`.

4. **A lost track must not be destroyed again.** After `TRACK_LOST` the SDK marks the track
   terminal, so the replacement path's `DESTROY_TRACK` failed with `NotFound` and took recovery down
   with it. `destroy_if_live` treats `NotFound` as already-done. Found by the two-owner regression.

5. **`App::visible` was deleted rather than kept.** It existed only to hold presenter-pushed source
   visibility, which 1.5 does not have; retaining it would have left a field that is always `true`
   gating frame submission.

6. **The live harness lives in `src/fake_presenter.rs`, not `tests/support/`.** vvrd is a binary
   crate with no library target, so an integration test cannot reach `presenter::VividPresenter`.
   It logs each accepted channel at `CHANNEL_ACCEPTED` rather than at EOF, because dropping a
   `TrackChannel` does not close the socket while the SDK's reader thread still holds it — waiting
   for EOF made the assertion racy and, for a live channel, unreachable.

7. **One pre-existing clippy lint was fixed.** `renderer.rs` tripped `manual_is_multiple_of` under
   the newer toolchain the 1.5 crates require. Unrelated to the migration, but the verification gate
   is `-D warnings`.

8. **`--dry-run`/offline mode carried more of the test suite than expected.** Offline sessions
   synthesize `SURFACE_READY`, `TRACK_READY`, `WAIT_SATISFIED`, and `TRACK_ACTIVATED`, so the whole
   resize/replace/recover flow is testable without a socket. The live harness is reserved for the
   ordering, isolation, and transport-failure regressions that genuinely need real connections.
