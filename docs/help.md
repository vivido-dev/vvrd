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
-l, --landscape          landscape pages for Markdown/Mermaid and HTML/EPUB
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

In vvmux, moving a floating pane changes only the outer projection; vvrd continues using local
`(0,0)` coordinates. Resizing a pane recreates its one raster source after a 120 ms debounce.
Background tabs, zoom-hidden panes, and fully occluded panes pause frame submission until vvmux
reports them visible again.
