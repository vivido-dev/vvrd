# vvrd

`vvrd` is a full-screen PDF, EPUB, and Office document reader for the Vivido terminal. It renders
documents with MuPDF and sends page pixels through the Vivid 1.1 side channel, never through
terminal escape sequences. The same binary runs directly in Vivido, in tiled or floating vvmux
panes, and in a remote shell reached with `vvssh`.

```sh
cargo build --release
target/release/vvrd book.epub
target/release/vvrd --page 12 paper.pdf
target/release/vvrd report.docx
target/release/vvrd --export paper.pdf
```

Vivido, vvmux, and vvssh provide `VIVID_ENDPOINT`, optional `VIVID_ENDPOINT_BULK`, and
`VIVID_TOKEN`; vvrd discovers them automatically. `--dry-run` exercises the renderer and protocol
without a live presenter. `--trace DIR` writes Vivid control and raster streams for debugging.

## Office documents

`.docx`, `.pptx`, `.odt`, and `.odp` are converted to PDF at startup, into a temporary directory
that is deleted when vvrd exits. They then behave exactly like any other fixed-layout PDF —
rerendered zoom, search, links, TOC, and export all work.

Conversion prefers a real LibreOffice install (`soffice` or `libreoffice` on `PATH`), which is the
only fully faithful option. Without one, vvrd falls back to the pure-Rust `lo_writer`/`lo_impress`
importers and says so in the status row: that path reproduces the text but **drops embedded
images**, always lays Writer documents out as A4, and ignores the slide size recorded in a `.pptx`.
Install LibreOffice if fidelity matters.

Set `VVRD_OFFICE_BACKEND=soffice` or `=pure` to pin one backend instead of choosing automatically;
`soffice` then reports an error rather than silently degrading.

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
| `R` or F5 | Reload rendered pages |
| `q`, Esc, Ctrl-C | Quit |

Reader state is saved per document in the platform cache directory. See
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the transport and compositor design.
