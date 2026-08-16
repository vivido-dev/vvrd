# vvrd help

vvrd reserves the final terminal row for status and uses the remaining pane-local cell rectangle
for one Vivid raster node. Full-screen text overlays hide that node first, so TOC, metadata, links,
and help remain readable in Vivido and nested vvmux panes.

## Command line

```text
vvrd [OPTIONS] <DOCUMENT>

-p, --page N             open one-based page N instead of the saved page
-e, --export             export one page as PNG and exit
-i, --invert             start inverted
-b, --black-color CSS    custom document black
-w, --white-color CSS    custom document white
    --theme light|dark   Markdown/Mermaid paper theme (default: light)
    --dry-run             validate without a live presenter
    --trace DIR           write control.vivid and raster-*.vivid traces
-v, --verbose             diagnostic logging without credentials
```

PDF and fixed Letter markup pages support zoom, vertical scrolling, horizontal panning, rotation,
inversion, custom black/white colours, warm tint, whitespace crop, search highlights, links,
metadata, TOC navigation, and PNG export. EPUB documents use MuPDF reflow and bind `<`/`>` to the
font size; fixed-layout zoom is intentionally disabled for reflowable content.

`.md`, `.markdown`, and `.mkd` files are block-aware paginated onto portrait 2040×2640 Letter
pages with 180-pixel margins. `.mmd` and `.mermaid` files are contain-fitted and centred on one
Letter page. Markdown includes GFM tables/task lists/autolinks/strikethrough, local raster/SVG/data
URI images, and fenced Mermaid; it never fetches remote images. `R`/F5 atomically rereads source
and local assets. If parsing or pagination fails, the prior document remains visible.

On macOS, PowerPoint (`.pptx`, `.pptm`, `.ppsx`, `.ppt`, …) and Word (`.docx`, `.docm`, `.doc`, …)
files are rendered by Quick Look, which uses the system's own Office renderer. Quick Look previews
a document rather than paginating it, so vvrd shows **the first slide or page only**; the status
line and the metadata overlay both say so. Everything else — zoom, rotation, invert, tint, crop,
and PNG export — behaves as it does for a PDF page. Previews are cached per source file and
refreshed by `R`/F5 when the file changes. Off macOS these formats report that Quick Look is
required.

In vvmux, moving a floating pane changes only the outer projection; vvrd continues using local
`(0,0)` coordinates. Resizing a pane recreates its one raster source after a 120 ms debounce.
Background tabs, zoom-hidden panes, and fully occluded panes pause frame submission until vvmux
reports them visible again.
