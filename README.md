# vvrd

`vvrd` is a full-screen PDF and EPUB reader for the Vivido terminal. It renders documents with
MuPDF and sends page pixels through the Vivid 1.1 side channel, never through terminal escape
sequences. The same binary runs directly in Vivido, in tiled or floating vvmux panes, and in a
remote shell reached with `vvssh`.

```sh
cargo build --release
target/release/vvrd book.epub
target/release/vvrd --page 12 paper.pdf
target/release/vvrd --export paper.pdf
```

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
