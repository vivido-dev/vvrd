#!/usr/bin/env python3
"""Generate the minimal DOCX/PPTX/ODT/ODP fixtures used by vvrd's office tests.

Kept tiny and hand-written so the tracked binaries stay small and every part is
accounted for. Real LibreOffice opens all four.
"""
import sys
import zipfile
from pathlib import Path

OUT = Path(sys.argv[1])
OUT.mkdir(parents=True, exist_ok=True)

TEXT = "vvrd office fixture"
BULLET = "second paragraph"


def write(name, parts, mimetype=None):
    path = OUT / name
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as zf:
        if mimetype is not None:
            # ODF requires an uncompressed `mimetype` entry first.
            zi = zipfile.ZipInfo("mimetype")
            zi.compress_type = zipfile.ZIP_STORED
            zf.writestr(zi, mimetype)
        for member, data in parts.items():
            zf.writestr(member, data)
    print(f"{name}: {path.stat().st_size} bytes")


# ---------------------------------------------------------------- DOCX
DOCX_TYPES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"""

ROOT_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{target}"/>
</Relationships>"""

DOCX_BODY = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:r><w:t>{TEXT}</w:t></w:r></w:p>
<w:p><w:r><w:t>{BULLET}</w:t></w:r></w:p>
<w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr>
</w:body>
</w:document>"""

write(
    "sample.docx",
    {
        "[Content_Types].xml": DOCX_TYPES,
        "_rels/.rels": ROOT_RELS.format(target="word/document.xml"),
        "word/document.xml": DOCX_BODY,
    },
)

# ---------------------------------------------------------------- PPTX
PPTX_TYPES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
</Types>"""

PPTX_PRESENTATION = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
<p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst>
<p:sldSz cx="9144000" cy="6858000"/>
<p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"""

PPTX_PRESENTATION_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"""


def shape(name, idx, x, y, cx, cy, text, placeholder):
    return f"""<p:sp>
<p:nvSpPr><p:cNvPr id="{idx}" name="{name}"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="{placeholder}"/></p:nvPr></p:nvSpPr>
<p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>{text}</a:t></a:r></a:p></p:txBody>
</p:sp>"""


PPTX_SLIDE = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
{shape("Title 1", 2, 685800, 457200, 7772400, 1143000, TEXT, "title")}
{shape("Content 2", 3, 685800, 1828800, 7772400, 3429000, BULLET, "body")}
</p:spTree></p:cSld>
</p:sld>"""

PPTX_SLIDE_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"""

PPTX_LAYOUT = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="obj">
<p:cSld name="Title and Content"><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
</p:spTree></p:cSld>
</p:sldLayout>"""

PPTX_LAYOUT_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"""

PPTX_MASTER = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
</p:spTree></p:cSld>
<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
</p:sldMaster>"""

PPTX_MASTER_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>"""


def scheme_colors():
    names = [
        ("dk1", "000000"), ("lt1", "FFFFFF"), ("dk2", "44546A"), ("lt2", "E7E6E6"),
        ("accent1", "4472C4"), ("accent2", "ED7D31"), ("accent3", "A5A5A5"),
        ("accent4", "FFC000"), ("accent5", "5B9BD5"), ("accent6", "70AD47"),
        ("hlink", "0563C1"), ("folHlink", "954F72"),
    ]
    return "".join(f'<a:{n}><a:srgbClr val="{v}"/></a:{n}>' for n, v in names)


PPTX_THEME = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office">
<a:themeElements>
<a:clrScheme name="Office">{scheme_colors()}</a:clrScheme>
<a:fontScheme name="Office">
<a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont>
<a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont>
</a:fontScheme>
<a:fmtScheme name="Office">
<a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst>
<a:lnStyleLst><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst>
<a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst>
<a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst>
</a:fmtScheme>
</a:themeElements>
</a:theme>"""

write(
    "sample.pptx",
    {
        "[Content_Types].xml": PPTX_TYPES,
        "_rels/.rels": ROOT_RELS.format(target="ppt/presentation.xml"),
        "ppt/presentation.xml": PPTX_PRESENTATION,
        "ppt/_rels/presentation.xml.rels": PPTX_PRESENTATION_RELS,
        "ppt/slides/slide1.xml": PPTX_SLIDE,
        "ppt/slides/_rels/slide1.xml.rels": PPTX_SLIDE_RELS,
        "ppt/slideLayouts/slideLayout1.xml": PPTX_LAYOUT,
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels": PPTX_LAYOUT_RELS,
        "ppt/slideMasters/slideMaster1.xml": PPTX_MASTER,
        "ppt/slideMasters/_rels/slideMaster1.xml.rels": PPTX_MASTER_RELS,
        "ppt/theme/theme1.xml": PPTX_THEME,
    },
)

# ---------------------------------------------------------------- ODF
ODF_NS = (
    'xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" '
    'xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" '
    'xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" '
    'xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" '
    'xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" '
    'xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" '
    'xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"'
)


def manifest(mimetype, extra=()):
    entries = "".join(
        f'<manifest:file-entry manifest:full-path="{p}" manifest:media-type="text/xml"/>'
        for p in ("content.xml", "styles.xml", *extra)
    )
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3">
<manifest:file-entry manifest:full-path="/" manifest:media-type="{mimetype}"/>
{entries}
</manifest:manifest>"""


ODT_MIME = "application/vnd.oasis.opendocument.text"
ODP_MIME = "application/vnd.oasis.opendocument.presentation"

ODT_CONTENT = f"""<?xml version="1.0" encoding="UTF-8"?>
<office:document-content {ODF_NS} office:version="1.3">
<office:body><office:text>
<text:p>{TEXT}</text:p>
<text:p>{BULLET}</text:p>
</office:text></office:body>
</office:document-content>"""

ODP_CONTENT = f"""<?xml version="1.0" encoding="UTF-8"?>
<office:document-content {ODF_NS} office:version="1.3">
<office:body><office:presentation>
<draw:page draw:name="page1" draw:master-page-name="Default">
<draw:frame draw:layer="layout" svg:width="20cm" svg:height="3cm" svg:x="2cm" svg:y="2cm" presentation:class="title">
<draw:text-box><text:p>{TEXT}</text:p></draw:text-box>
</draw:frame>
<draw:frame draw:layer="layout" svg:width="20cm" svg:height="8cm" svg:x="2cm" svg:y="6cm" presentation:class="subtitle">
<draw:text-box><text:p>{BULLET}</text:p></draw:text-box>
</draw:frame>
</draw:page>
</office:presentation></office:body>
</office:document-content>"""


def odf_styles(master):
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<office:document-styles {ODF_NS} office:version="1.3">
<office:automatic-styles>
<style:page-layout style:name="pm1"><style:page-layout-properties fo:page-width="21.001cm" fo:page-height="29.7cm" style:print-orientation="portrait" fo:margin-top="2cm" fo:margin-bottom="2cm" fo:margin-left="2cm" fo:margin-right="2cm"/></style:page-layout>
</office:automatic-styles>
<office:master-styles>{master}</office:master-styles>
</office:document-styles>"""


write(
    "sample.odt",
    {
        "META-INF/manifest.xml": manifest(ODT_MIME),
        "content.xml": ODT_CONTENT,
        "styles.xml": odf_styles(
            '<style:master-page style:name="Standard" style:page-layout-name="pm1"/>'
        ),
    },
    mimetype=ODT_MIME,
)

write(
    "sample.odp",
    {
        "META-INF/manifest.xml": manifest(ODP_MIME),
        "content.xml": ODP_CONTENT,
        "styles.xml": odf_styles(
            '<style:master-page style:name="Default" style:page-layout-name="pm1"/>'
        ),
    },
    mimetype=ODP_MIME,
)
