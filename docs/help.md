# vvrd help

vvrd reserves the final terminal row for status and uses the remaining pane-local cell rectangle
for one Vivid raster node. Full-screen text overlays hide that node first, so TOC, metadata, links,
and help remain readable in Vivido and nested vvmux panes.

## Command line

```text
vvrd [OPTIONS] <DOCUMENT>
vvrd --preview [OPTIONS] <DOCUMENT> <COLUMNS> <ROWS> [COLUMN ROW]

-p, --page N             open one-based page N instead of the saved page
-e, --export             export one page as PNG and exit
-i, --invert             start inverted
-l, --landscape          landscape pages for Markdown/Mermaid and HTML/EPUB
-b, --black-color CSS    custom document black
-w, --white-color CSS    custom document white
    --theme light|dark   Markdown/Mermaid paper theme (default: light)
    --preview             draw into a file manager's preview pane (see below)
    --dry-run             validate without a live presenter
    --trace DIR           write metadata-only control.ndjson and track-*.ndjson traces
-v, --verbose             diagnostic logging without credentials
```

Vivido and vvssh provide `VIVID_ENDPOINT_CONTROL`, optional `VIVID_ENDPOINT_BULK`, and
`VIVID_ROOT_SECRET`; vvrd discovers them automatically and never puts the secret on the wire.
Reader state is saved per document in the platform cache directory.

## Controls

| Input | Action |
|---|---|
| Left, right mouse click | Previous page |
| Right, Space, left mouse click outside a link | Next page |
| Left mouse click on a link | Open local reader documents in vvrd; open web links in the default browser |
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
| `R` or F5 | Manually reread and repaginate the document and local assets |
| `q`, Esc, Ctrl-C | Quit |

## Formats

PDF and fixed Letter markup pages support zoom, vertical scrolling, horizontal panning, rotation,
inversion, custom black/white colours, warm tint, whitespace crop, search highlights, links,
metadata, TOC navigation, and PNG export. EPUB documents use MuPDF reflow and bind `<`/`>` to the
font size; fixed-layout zoom is intentionally disabled for reflowable content.

`.pptx`, `.docx`, `.odp`, and `.odt` files are converted to PDF once per distinct content by a
headless LibreOffice (`soffice` from `VVRD_SOFFICE`, `PATH`, or the platform install location;
converted PDFs cache under the platform cache directory, or `VVRD_OFFICE_CACHE`) and then behave
like any PDF. Without LibreOffice, vvrd prints a warning and exits.

`.md`, `.markdown`, and `.mkd` files are block-aware paginated onto portrait 2040×2640 Letter
pages with 180-pixel margins (landscape 2640×2040 with `-l`). `.mmd` and `.mermaid` files are
contain-fitted and centred on one Letter page. Markdown includes GFM tables/task
lists/autolinks/strikethrough, local raster/SVG/data
URI images, and fenced Mermaid; it never fetches remote images. `R`/F5 atomically rereads source
and local assets. Interactive Markdown, DOCX, and PPTX views also watch the opened source and
perform that reload automatically after a short save debounce. Linked Markdown assets and
ODP/ODT sources still require `R`/F5. If parsing or pagination fails, the prior document remains
visible.

## Preview pane

`--preview` shows the document inside a file manager's preview pane, as
[vfm](https://github.com/wensheng/vfm) runs it inside Vivido:

```sh
vvrd --preview book.epub 48 27 52 2      # DOCUMENT COLUMNS ROWS [COLUMN ROW]
```

The page fills that 0-based grid-cell rectangle of the window. Nothing is drawn in the terminal, the
document opens at its first page (or `--page`), and no reader state is saved. Stdin takes one command
per line: `next`, `prev`, `first`, `last`, and `hide`/`show` while something covers the pane. The
preview ends, removing its page, when stdin closes. Errors go to stderr.

## vvmux

In vvmux, moving a floating pane changes only the outer projection; vvrd continues using local
`(0,0)` coordinates. Resizing a pane recreates its one raster source after a 120 ms debounce.
Background tabs, zoom-hidden panes, and fully occluded panes pause frame submission until vvmux
reports them visible again.
