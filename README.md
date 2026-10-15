# vvrd

**Every document, in your terminal.**

vvrd is a fast, full-screen document reader for the [Vivido](https://vivido.dev) terminal. Open a
PDF, an EPUB, a Markdown file, a Mermaid diagram, or a PowerPoint deck, and it shows up as crisp,
print-quality pages right where you're working. You don't need a separate viewer window or a
browser tab, and nothing gets mangled into ASCII art.

```sh
cargo install vvrd
vvrd paper.pdf
```

▶ [Watch vvrd working alongside AI agents](https://youtu.be/Qa1DrxK1Bu8)

## Why vvrd

- **Real pages, not approximations.** vvrd renders actual pixels at full resolution, with real
  fonts, real figures, and real typography.
- **One reader for everything.** PDF, EPUB, Markdown, Mermaid, PPTX, DOCX, ODP, and ODT all open
  with the same command and the same keys.
- **Live Markdown preview.** Write in your editor in one pane and keep `vvrd notes.md` open in the
  next. Every save repaginates within a moment, and a broken edit never blanks the screen.
- **Mermaid without the baggage.** Flowcharts, sequence, class, state, ER, Gantt, pie, mindmap,
  timeline, and more come from a built-in Rust engine. There's no Node, no headless browser, and
  no network.
- **Works where you work.** The same binary runs locally, on a remote machine over `vvssh`, and
  inside a `vvmux` pane. Your remote PDFs appear on your local screen.
- **Remembers your place.** Every document reopens at the page you left it on.

## Built for developers

- **A home for your agent's output.** Have an AI agent write its report as Markdown with Mermaid
  diagrams, then read it as a typeset document instead of a scrolling wall of text.
- **Review docs before you push.** READMEs, design docs, and architecture diagrams render the way
  a reader will see them, with GFM tables, task lists, code blocks, and local images.
- **Script it.** `vvrd --export` turns any page into a PNG, and `--preview` embeds vvrd in file
  managers such as [vfm](https://github.com/wensheng/vfm).
- **A clean terminal.** Page pixels travel on Vivid Protocol's authenticated side channel, never
  as escape sequences in your PTY, so your terminal session never gets corrupted.

## Quick start

```sh
vvrd book.epub                 # paginated like print
vvrd --page 12 paper.pdf       # jump straight to a page
vvrd --theme dark design.md    # Markdown on dark paper
vvrd architecture.mmd          # a Mermaid diagram, centred on the page
vvrd deck.pptx                 # slides, via LibreOffice
vvrd --export paper.pdf        # save the current page as PNG
```

## Keys

| Key | Action |
|---|---|
| ← / → / Space, or click | Previous / next page |
| ↑ / ↓ | Scroll, turning at page edges |
| `g` | Go to page |
| `/`, `n` / `N` | Search, then next / previous match |
| `t` · `f` · `M` | Table of contents · links · metadata |
| `z`, `o` / `O` | Zoom, then zoom in / out |
| `i` · `d` · `c` · `r` | Invert · warm tint · auto-crop margins · rotate |
| `<` / `>` | EPUB font size |
| `e` | Export the page as PNG |
| `R` / F5 | Reload |
| `?` | All keys |
| `q` | Quit |

## Install

The Vivido installers already include vvrd. Otherwise, use Cargo:

```sh
cargo install vvrd
```

For PPTX, DOCX, ODP, and ODT files, install [LibreOffice](https://www.libreoffice.org/). vvrd
finds it on its own, converts each document once, and caches the result. Every other format works
without LibreOffice.

## Learn more

- [`docs/help.md`](docs/help.md) covers every option, format detail, and environment variable.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) explains how vvrd streams pages over Vivid 1.5.
  It's a good starting point if you want to build your own terminal media app with
  [`vivid_sdk`](https://crates.io/crates/vivid_sdk).

## License

[AGPL-3.0](LICENSE). Third-party notices are in [`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES).
