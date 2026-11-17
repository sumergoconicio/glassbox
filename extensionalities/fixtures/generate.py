"""Reproducible, self-authored tiny fixtures. No downloaded documents."""
from pathlib import Path
import struct
import zlib
import zipfile

ROOT = Path(__file__).resolve().parent


def archive(name, entries):
    with zipfile.ZipFile(ROOT / name, "w", zipfile.ZIP_DEFLATED) as out:
        for path, body in entries.items():
            info = zipfile.ZipInfo(path, (2026, 1, 2, 3, 4, 6))
            info.compress_type = zipfile.ZIP_DEFLATED
            out.writestr(info, body)


def png(w, h):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress((b"\0" + b"\xff\xff\xff" * w) * h))
            + chunk(b"IEND", b""))


archive("listing.zip", {"note.txt": "Hello archive\n", "../escape.txt": "Never extract me\n"})
(ROOT / "image.png").write_bytes(png(640, 400))
(ROOT / "disguised.pdf").write_bytes(png(10, 10))
(ROOT / "truncated.zip").write_bytes(b"PK\x03\x04broken")
archive("entry-cap.zip", {f"entry-{i}": "" for i in range(4097)})
archive("bomb.zip", {"huge.bin": b""})
bomb = bytearray((ROOT / "bomb.zip").read_bytes())
central = bomb.index(b"PK\x01\x02")
struct.pack_into("<I", bomb, central + 24, 257 << 20)
(ROOT / "bomb.zip").write_bytes(bomb)

W = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
A = "http://schemas.openxmlformats.org/drawingml/2006/main"
P = "http://schemas.openxmlformats.org/presentationml/2006/main"
S = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
PKG = "http://schemas.openxmlformats.org/package/2006/relationships"
CT = "http://schemas.openxmlformats.org/package/2006/content-types"
DC = "http://purl.org/dc/elements/1.1/"
CORE = f'''<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="{DC}"><dc:title>Field Notes</dc:title><dc:creator>Ada Example</dc:creator><dc:language>en-GB</dc:language></cp:coreProperties>'''


def ooxml(name, main, content_type, body, extra=None):
    entries = {
        "[Content_Types].xml": f'<Types xmlns="{CT}"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/{main}" ContentType="{content_type}"/></Types>',
        "_rels/.rels": f'<Relationships xmlns="{PKG}"><Relationship Id="rId1" Type="{R}/officeDocument" Target="{main}"/></Relationships>',
        "docProps/core.xml": CORE,
        "docProps/app.xml": '<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>Self-authored fixture</Application><Pages>999</Pages></Properties>',
        main: body,
    }
    entries.update(extra or {})
    archive(name, entries)


DOCX_TYPE = "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"
DOCX = f'''<w:document xmlns:w="{W}" xmlns:a="{A}"><w:body>
<w:p><w:r><w:t>This is a native text layer with an ampersand &amp; evidence.</w:t></w:r></w:p>
<w:p><w:pPr><w:sectPr/></w:pPr><w:r><w:t>Section one ends here.</w:t></w:r></w:p>
<w:p><w:r><w:t>Scan</w:t><w:drawing><a:blip/></w:drawing></w:r></w:p><w:sectPr/>
</w:body></w:document>'''
ooxml("notes.docx", "word/document.xml", DOCX_TYPE, DOCX, {"docProps/thumbnail.png": png(640, 400)})
ooxml("renamed.zip", "word/document.xml", DOCX_TYPE, DOCX)
ooxml("malformed.docx", "word/document.xml", DOCX_TYPE, '<w:document xmlns:w="' + W + '"><w:body>')
ooxml("deck.pptx", "ppt/presentation.xml",
      "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
      f'<p:presentation xmlns:p="{P}" xmlns:r="{R}"><p:sldIdLst><p:sldId id="256" r:id="second"/><p:sldId id="257" r:id="first"/></p:sldIdLst></p:presentation>', {
          "ppt/_rels/presentation.xml.rels": f'<Relationships xmlns="{PKG}"><Relationship Id="first" Type="{R}/slide" Target="slides/slide1.xml"/><Relationship Id="second" Type="{R}/slide" Target="slides/slide2.xml"/></Relationships>',
          "ppt/slides/slide1.xml": f'<p:sld xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree><p:pic><a:blip/></p:pic><p:sp><a:p><a:r><a:t>Scan</a:t></a:r></a:p></p:sp></p:spTree></p:cSld></p:sld>',
          "ppt/slides/slide2.xml": f'<p:sld xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree><p:sp><a:p><a:r><a:t>This is the first slide in presentation order.</a:t></a:r></a:p></p:sp></p:spTree></p:cSld></p:sld>',
      })

PROSE = "We can inspect the original source and verify the evidence in this answer."
SECOND_PROSE = "This is another native note about the source and its evidence."
ooxml("grid.xlsx", "xl/workbook.xml",
      "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
      f'<workbook xmlns="{S}" xmlns:r="{R}"><sheets><sheet name="Summary" sheetId="1" r:id="summary"/><sheet name="Notes" sheetId="2" r:id="notes"/></sheets></workbook>', {
          "xl/_rels/workbook.xml.rels": f'<Relationships xmlns="{PKG}"><Relationship Id="notes" Type="{R}/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="summary" Type="{R}/worksheet" Target="worksheets/sheet2.xml"/><Relationship Id="strings" Type="{R}/sharedStrings" Target="sharedStrings.xml"/></Relationships>',
          "xl/sharedStrings.xml": f'<sst xmlns="{S}"><si><t>{PROSE}</t></si><si><r><t>This is another native note </t></r><r><t>about the source and its evidence.</t></r></si></sst>',
          "xl/worksheets/sheet2.xml": f'<worksheet xmlns="{S}"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Label</t></is></c><c r="B1" t="inlineStr"><is><t>Total</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>count</t></is></c><c r="B2"><v>7</v></c></row></sheetData></worksheet>',
          "xl/worksheets/sheet1.xml": f'''<worksheet xmlns="{S}"><dimension ref="A1:XFD1048576"/><sheetData>
<row r="1"><c r="A1" t="inlineStr"><is><t>ID</t></is></c><c r="B1" t="inlineStr"><is><t>Notes</t></is></c><c r="C1" t="inlineStr"><is><t>Flag</t></is></c></row>
<row r="2"><c r="A2"><v>1</v></c><c r="B2" t="s"><v>0</v></c><c r="C2" t="b"><v>1</v></c></row>
<row r="3"><c r="A3"><v>2</v></c><c r="B3" t="s"><v>0</v></c><c r="C3"><f>1+2</f><v>3</v></c></row>
<row r="4"><c r="A4"><v>3</v></c><c r="B4" t="s"><v>1</v></c></row></sheetData><mergeCells><mergeCell ref="B4:C4"/></mergeCells></worksheet>''',
      })

O = "urn:oasis:names:tc:opendocument:xmlns:office:1.0"
T = "urn:oasis:names:tc:opendocument:xmlns:text:1.0"
TABLE = "urn:oasis:names:tc:opendocument:xmlns:table:1.0"
DRAW = "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
META = "urn:oasis:names:tc:opendocument:xmlns:meta:1.0"


def odf(name, kind, body):
    archive(name, {
        "mimetype": "application/vnd.oasis.opendocument." + kind,
        "meta.xml": f'<office:document-meta xmlns:office="{O}" xmlns:dc="{DC}" xmlns:meta="{META}"><office:meta><dc:title>Open Notes</dc:title><dc:creator>Grace Example</dc:creator><dc:language>fr-FR</dc:language></office:meta></office:document-meta>',
        "content.xml": f'<office:document-content xmlns:office="{O}" xmlns:text="{T}" xmlns:table="{TABLE}" xmlns:draw="{DRAW}"><office:body>{body}</office:body></office:document-content>',
        "Thumbnails/thumbnail.png": png(64, 40),
    })


odf("notes.odt", "text", '<office:text><text:section text:name="intro"><text:p>Café <text:span>et preuves</text:span> ici.<text:tab/>Texte<text:s text:c="2"/>natif.</text:p></text:section><text:section text:name="scan"><text:p>Scan</text:p><draw:frame><draw:image/></draw:frame></text:section></office:text>')
odf("deck.odp", "presentation", '<office:presentation><draw:page draw:name="one"><text:p>This is the first native ODF presentation slide.</text:p></draw:page><draw:page draw:name="two"><draw:image/><text:p>Scan</text:p></draw:page></office:presentation>')
odf("grid.ods", "spreadsheet", f'''<office:spreadsheet><table:table table:name="Notes">
<table:table-row><table:table-cell office:value-type="string"><text:p>ID</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Notes</text:p></table:table-cell></table:table-row>
<table:table-row table:number-rows-repeated="2"><table:table-cell office:value-type="float" office:value="7"/><table:table-cell office:value-type="string"><text:p>{PROSE}</text:p></table:table-cell></table:table-row>
<table:table-row><table:table-cell office:value-type="float" office:value="3" table:formula="of:=1+2"/><table:table-cell office:value-type="string" table:number-columns-spanned="2"><text:p>{SECOND_PROSE}</text:p></table:table-cell><table:covered-table-cell/></table:table-row>
<table:table-row table:number-rows-repeated="1048572"><table:table-cell table:number-columns-repeated="16384"/></table:table-row>
</table:table></office:spreadsheet>''')


def repack(source, name, changes):
    with zipfile.ZipFile(ROOT / source) as z:
        entries = {n: z.read(n) for n in z.namelist()}
    entries.update(changes)
    archive(name, entries)


repack("notes.docx", "spoofed-language.docx", {
    "docProps/core.xml": '<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:fake="urn:not-dublin-core"><fake:language>en</fake:language></cp:coreProperties>'
})
repack("notes.docx", "entity.docx", {
    "word/document.xml": f'<!DOCTYPE w:document [<!ENTITY secret SYSTEM "file:///etc/passwd">]><w:document xmlns:w="{W}"><w:body><w:p><w:r><w:t>&secret;</w:t></w:r></w:p></w:body></w:document>'
})
repack("notes.docx", "read-cap.docx", {"word/document.xml": b" " * ((16 << 20) + 1)})
repack("grid.xlsx", "external.xlsx", {
    "xl/_rels/workbook.xml.rels": f'<Relationships xmlns="{PKG}"><Relationship Id="summary" Type="{R}/worksheet" Target="https://example.invalid/data" TargetMode="External"/><Relationship Id="notes" Type="{R}/worksheet" Target="../../outside.xml"/></Relationships>'
})
repack("notes.docx", "encrypted.docx", {})
encrypted = bytearray((ROOT / "encrypted.docx").read_bytes())
pos = 0
while pos < len(encrypted) - 4:
    if encrypted[pos:pos+4] == b"PK\x03\x04":
        flags = struct.unpack_from("<H", encrypted, pos + 6)[0]
        struct.pack_into("<H", encrypted, pos + 6, flags | 1)
    elif encrypted[pos:pos+4] == b"PK\x01\x02":
        flags = struct.unpack_from("<H", encrypted, pos + 8)[0]
        struct.pack_into("<H", encrypted, pos + 8, flags | 1)
    pos += 1
(ROOT / "encrypted.docx").write_bytes(encrypted)
(ROOT / "image.docx").write_bytes(png(640, 400))


def wp(text):
    return f"<w:p><w:r><w:t>{text}</w:t></w:r></w:p>"


WORD_TABLE = f'''<w:tbl><w:tblGrid><w:gridCol w:w="1000"/><w:gridCol w:w="2000"/><w:gridCol w:w="2000"/></w:tblGrid>
<w:tr><w:trPr><w:tblHeader/></w:trPr><w:tc>{wp("Name")}</w:tc><w:tc>{wp("Analysis")}</w:tc><w:tc>{wp("Status")}</w:tc></w:tr>
<w:tr><w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr>{wp("Group")}</w:tc><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr>{wp("Shared analysis across two columns.")}</w:tc></w:tr>
<w:tr><w:tc><w:tcPr><w:vMerge/></w:tcPr>{wp("")}</w:tc><w:tc>{wp("Before nested.")}<w:tbl><w:tblGrid><w:gridCol/></w:tblGrid><w:tr><w:tc>{wp("Nested evidence.")}</w:tc></w:tr></w:tbl>{wp("After nested.")}</w:tc><w:tc>{wp("")}</w:tc></w:tr>
</w:tbl>'''
ooxml("tables.docx", "word/document.xml", DOCX_TYPE, f'<w:document xmlns:w="{W}"><w:body>{wp("A document with a structured table.")}{WORD_TABLE}<w:sectPr/></w:body></w:document>')
ooxml("bad-merge.docx", "word/document.xml", DOCX_TYPE, f'<w:document xmlns:w="{W}"><w:body><w:tbl><w:tr><w:tc><w:tcPr><w:vMerge/></w:tcPr>{wp("Orphan continuation")}</w:tc></w:tr></w:tbl></w:body></w:document>')


def ac(text, attrs=""):
    return f'<a:tc {attrs}><a:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></a:txBody></a:tc>'


DRAWING_TABLE = f'''<a:tbl><a:tblPr firstRow="1"/><a:tblGrid><a:gridCol w="100"/><a:gridCol w="100"/><a:gridCol w="100"/></a:tblGrid>
<a:tr h="100">{ac("Wide", 'gridSpan="2"')}{ac("", 'hMerge="1"')}{ac("Right")}</a:tr>
<a:tr h="100">{ac("Tall", 'rowSpan="2"')}{ac("B")}{ac("C")}</a:tr>
<a:tr h="100">{ac("", 'vMerge="1"')}{ac("Bottom", 'gridSpan="2"')}{ac("", 'hMerge="1"')}</a:tr></a:tbl>'''
repack("deck.pptx", "tables.pptx", {
    "ppt/slides/slide2.xml": f'<p:sld xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree><p:graphicFrame><a:graphic><a:graphicData>{DRAWING_TABLE}</a:graphicData></a:graphic></p:graphicFrame></p:spTree></p:cSld></p:sld>'
})

ODF_TABLE = '''<table:table table:name="Evidence"><table:table-header-rows><table:table-row>
<table:table-cell><text:p>Name</text:p></table:table-cell><table:table-cell><text:p>Analysis</text:p></table:table-cell><table:table-cell><text:p>Status</text:p></table:table-cell>
</table:table-row></table:table-header-rows><table:table-row>
<table:table-cell table:number-rows-spanned="2"><text:p>Group</text:p></table:table-cell><table:table-cell table:number-columns-spanned="2"><text:p>Shared analysis across two columns.</text:p></table:table-cell><table:covered-table-cell/>
</table:table-row><table:table-row><table:covered-table-cell/><table:table-cell><text:p>Before nested.</text:p>
<table:table table:name="Nested"><table:table-row><table:table-cell><text:p>Nested evidence.</text:p></table:table-cell></table:table-row></table:table>
<text:p>After nested.</text:p></table:table-cell><table:table-cell><text:p/></table:table-cell></table:table-row></table:table>'''
odf("tables.odt", "text", f"<office:text><text:p>A structured ODF table.</text:p>{ODF_TABLE}</office:text>")
odf("tables.odp", "presentation", f'<office:presentation><draw:page draw:name="one"><text:p>Introduction</text:p></draw:page><draw:page draw:name="two">{ODF_TABLE}</draw:page></office:presentation>')
archive("directory-cap.zip", {})
directory = bytearray((ROOT / "directory-cap.zip").read_bytes())
struct.pack_into("<HH", directory, 8, 4097, 4097)
(ROOT / "directory-cap.zip").write_bytes(directory)
archive("zip64-directory.zip", {})
end = (ROOT / "zip64-directory.zip").read_bytes()
(ROOT / "zip64-directory.zip").write_bytes(struct.pack("<4sIQI", b"PK\x06\x07", 0, 0, 1) + end)
