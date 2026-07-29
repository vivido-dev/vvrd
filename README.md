# vvrd

`vvrd` is a full-screen PDF, EPUB, DOCX, and PPTX reader for the Vivido terminal. It renders
PDF/EPUB documents with MuPDF; DOCX/PPTX documents are first converted to a private temporary PDF
by LibreOffice. Page pixels travel through the Vivid 1.1 side channel, never through terminal
escape sequences. The same binary runs directly in Vivido, in tiled or floating vvmux panes, and
in a remote shell reached with `vvssh`.

```sh
cargo build --release
target/release/vvrd book.epub
target/release/vvrd --page 12 paper.pdf
target/release/vvrd report.docx
target/release/vvrd slides.pptx
target/release/vvrd --export paper.pdf
```

DOCX and PPTX viewing requires a local LibreOffice installation. Vvrd finds `soffice` or
`libreoffice` on `PATH` and in common installation locations. Use `--soffice PATH` or
`VVRD_SOFFICE` to select it explicitly, and `--office-timeout SECONDS` to change the 120-second
conversion limit (maximum 3600 seconds). Embedded images are retained by LibreOffice's PDF export.
PPTX pages are static slides: transitions, animations, audio, and video are not played.
Macro-enabled DOCM/PPTM files are rejected.

Office conversion uses an isolated LibreOffice profile and generic filenames in a private
temporary directory, removes Vivid credentials from the child environment, and deletes the
converted PDF on exit. It does not create a persistent document cache.

Vivido, vvmux, and vvssh provide `VIVID_ENDPOINT`, optional `VIVID_ENDPOINT_BULK`, and
`VIVID_TOKEN`; vvrd discovers them automatically. `--dry-run` exercises the renderer and protocol
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
| `R` or F5 | Reload rendered pages |
| `q`, Esc, Ctrl-C | Quit |

Reader state is saved per document in the platform cache directory. See
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the transport and compositor design.
