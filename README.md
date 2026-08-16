# vvrd

`vvrd` is a full-screen PDF, EPUB, Markdown, Mermaid, and office-document reader for the Vivido
terminal. MuPDF handles PDF/EPUB; the built-in Markdown/Mermaid backend paginates markup onto fixed
portrait Letter pages (2040×2640 pixels at 240 DPI, with 180-pixel margins); PPTX, DOCX, ODP, and
ODT files are converted to PDF once per distinct content by a headless LibreOffice and then read
like any PDF. Page pixels use the Vivid 1.5 side
channel, never terminal escape sequences. The same binary runs directly in Vivido and in a remote
shell reached with `vvssh`.
(Nested operation in vvmux panes returns once vvmux migrates to Vivid 1.5.)

```sh
cargo build --release
target/release/vvrd book.epub
target/release/vvrd --page 12 paper.pdf
target/release/vvrd --theme dark guide.md
target/release/vvrd diagram.mmd
target/release/vvrd deck.pptx
target/release/vvrd --export paper.pdf
```

Vivido and vvssh provide `VIVID_ENDPOINT_CONTROL`, optional `VIVID_ENDPOINT_BULK`, and
`VIVID_ROOT_SECRET`; vvrd discovers them automatically and never puts the secret on the wire. `--dry-run` exercises the renderer and protocol
without a live presenter. `--trace DIR` writes Vivid control and raster streams for debugging.

## Controls

| Key | Action |
|---|---|
| Left/Right, Space | Previous/next page |
| Up/Down | Scroll; turn at the page boundary |
| `j`/`k` | Next/previous page without scrolling |
| `h`/`l` | Page turn, or horizontal pan in zoom mode |
| PageUp/PageDown | Page turn, or viewport jump in zoom mode |
| `g` | Go to page |
| `z`, `o`/`O` | Toggle zoom; zoom in/out |
| `<`/`>` | Decrease/increase EPUB font size |
| `i`, `r`, `c`, `d` | Invert, rotate, auto-crop, warm tint |
| `/`, `n`/`N` | Search; next/previous matching page |
| `t`, `M`, `f`, `?` | TOC, metadata, links, help |
| `e` | Export the current page as PNG |
| `R` or F5 | Atomically reread and repaginate the document and local assets |
| `q`, Esc, Ctrl-C | Quit |

Reader state is saved per document in the platform cache directory. See
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the transport and compositor design.

Markdown supports GFM tables, strikethrough, task lists, autolinks, styled inline/code text,
blockquotes, local raster/SVG/data-URI images, and fenced Mermaid. Remote images are represented by
an in-page placeholder and are never fetched. `--theme light|dark` selects the Markdown/Mermaid
paper theme (default: `light`); the existing invert, tint, colour mapping, rotation, crop, search,
TOC, links, zoom/pan, state restore, and PNG export controls apply to markup pages too. A failed
reload leaves the previous document visible.

PPTX, DOCX, ODP, and ODT documents require LibreOffice. vvrd locates the `soffice` launcher on
`PATH` or in the standard install location per platform (override with `VVRD_SOFFICE`), prints a
warning and exits when it is unavailable, and otherwise converts headlessly to a PDF cached under
the platform cache directory keyed by the source content hash (override with `VVRD_OFFICE_CACHE`).
Converted documents support the full fixed-layout feature set — search, TOC, links, zoom, rotation,
export — and `R` reconverts when the source file changes.
