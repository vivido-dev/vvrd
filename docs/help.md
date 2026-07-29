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
    --dry-run             validate without a live presenter
    --trace DIR           write control.vivid and raster-*.vivid traces
-v, --verbose             diagnostic logging without credentials
    --soffice PATH        LibreOffice executable for DOCX/PPTX
    --office-timeout N    conversion timeout in seconds (default 120, max 3600)
```

PDF pages support sharp rerendered zoom, vertical scrolling, horizontal panning, rotation,
inversion, custom black/white colours, warm tint, whitespace crop, search highlights, links,
metadata, TOC navigation, and PNG export. EPUB documents use MuPDF reflow and bind `<`/`>` to the
font size; fixed-layout zoom is intentionally disabled for reflowable content.

DOCX and PPTX require LibreOffice (`soffice` or `libreoffice`). Vvrd converts each document to a
temporary PDF with an isolated profile before entering the terminal UI. Embedded document and
slide images are preserved. Presentation pages are static; transitions, animations, audio, and
video do not play. DOCM and PPTM are not accepted. Set `VVRD_SOFFICE` or pass `--soffice` if
automatic discovery fails.

In vvmux, moving a floating pane changes only the outer projection; vvrd continues using local
`(0,0)` coordinates. Resizing a pane recreates its one raster source after a 120 ms debounce.
Background tabs, zoom-hidden panes, and fully occluded panes pause frame submission until vvmux
reports them visible again.
