# vvrd contributor guidance

The repository-wide rules in the root `AGENTS.md` apply here in full. This file adds what is
specific to vvrd. Read `docs/ARCHITECTURE.md` before changing the Vivid layer.

## What vvrd is

A terminal PDF/EPUB/Markdown/Mermaid reader: MuPDF or the native fixed-Letter markup backend
renders pages, a compositor blits the visible region into a viewport-sized RGBA framebuffer, and a
**Vivid Protocol 1.5** producer submits it as raster media.
The PTY carries only status text and overlays; page pixels never touch it.

## Object model — the part to get right

vvrd owns exactly one of each:

| Object | Identity | Changes when |
|---|---|---|
| Surface | `(root context, surface ID)`, stable for the process | never |
| Scene node | one terminal grid-cell node referencing the surface | geometry, visibility, teardown |
| Track | live raster, slot 3, **immutable** dimensions | settled resize, track loss |
| Track channel | one authenticated generation for the active track | transport failure |

Rules that follow from this, and that a change must not break:

- A track is immutable. Any geometry change is a **replacement track**, never a mutated one.
- A replacement is created, its channel opened, a full frame sent, and `MILESTONE_OUTPUT_READY`
  awaited **before** `ACTIVATE_TRACK`. Never activate a slot onto a track with no decoded output.
- The retired track's transport must outlive its ordered `DESTROY_TRACK`. Closing first makes a
  relay remove the track on EOF and then reject the destroy.
- Media IDs belong to the track across channel generations. A channel advance keeps `frame_id`
  climbing; only a replacement track restarts it.
- Milestones are generation-local. Never treat a bit from an older channel generation as current
  readiness, and always log the generation alongside them.
- Match every object by its complete `(context, surface, track)` tuple. `TRACK_LOST` filtering is
  the specific place a bare numeric ID comparison would be wrong.

## The three recoveries are distinct

`NEED_FULL_FRAME` keeps the channel; a transport failure keeps the track (`ADVANCE_CHANNEL` +
reopen + full frame); `TRACK_LOST` keeps the surface, node, descriptor, and policy. Do not collapse
them back into one "recreate everything" path — that was the 1.1 design, and it blanked the display.

## Other vvrd-specific rules

- Profiles, not features. Required: `vivid-core-control-v1`, `terminal-surface-v1`,
  `live-media-v1`. Optional: `observability-v1`. Per-track capabilities (raster delta, zstd) are
  probed with `PROBE_TRACK_CONFIG` and fall back to a plainer configuration.
- Terminal metrics come from the **target descriptor**, never from core session state. Reject a
  descriptor that does not report anchor marker version 3.
- Surface policy is a strictest union and can never be relaxed. Sensitive document paths get
  deny-capture, deny-poster, deny-image-cache, and deny-descriptor-export before the first frame.
- Any child process (document preflight, link opener, export) inherits neither `VIVID_ROOT_SECRET`
  nor any `VIVID_ENDPOINT_*` variable. The retired `VIVID_TOKEN` name is scrubbed too.
- Deltas are planned against the **granted** `Track::delta_operation_limit`, not the requested one,
  and vvrd owns the delta-versus-full choice: 1.5 has no `send_raster_delta_or_full`.
- Frame sends block on channel flow, so they stay off the UI thread.

## Verification

From `vvrd/`:

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

`src/fake_presenter.rs` is a test-only Vivid 1.5 presenter that runs the real authentication
transcript, framing, and channel handshake. The live regressions built on it bind real TCP sockets;
if a sandbox denies socket creation, rerun them where it is permitted — a skipped socket path is not
integration evidence.

A change to lifecycle, teardown, track loss, or channel recovery needs a **two-owner** regression:
two owners deliberately reusing the same numeric surface, node, and track IDs, where one owner's
failure and recovery leaves the other's surface, node, generation, and next accepted frame intact.
A single-owner happy path is not sufficient.

Changes that touch `vivid_sdk` must also run the `vivid_sdk` and `vivi` suites.

`vvmux` is still Vivid 1.1, so nested operation is currently unavailable and no 1.1↔1.5 adapter
should be written for it.
