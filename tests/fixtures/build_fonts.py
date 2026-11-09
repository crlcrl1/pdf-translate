"""Generate original, box-shaped fonts for tests; no production font dependency."""

from pathlib import Path

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.t2CharStringPen import T2CharStringPen
from fontTools.pens.ttGlyphPen import TTGlyphPen


def build_font(path: Path, is_ttf: bool) -> None:
    units = 2048 if is_ttf else 1000
    chars = [*range(32, 127), 0x8BD1, 0x2026, 0x22EF]
    cmap = {code: f"uni{code:04X}" for code in chars}
    cmap[0x22EF] = cmap[0x2026]
    names = [".notdef", *dict.fromkeys(cmap.values())]
    builder = FontBuilder(units, isTTF=is_ttf)
    builder.setupGlyphOrder(names)
    builder.setupCharacterMap(cmap)
    glyphs = {}
    metrics = {}
    for name in names:
        advance = int(units * (0.3 if name == "uni0020" else 0.55))
        pen = TTGlyphPen(None) if is_ttf else T2CharStringPen(advance, None)
        if name != "uni0020":
            pen.moveTo((0, 0))
            pen.lineTo((int(units * 0.45), 0))
            pen.lineTo((int(units * 0.45), int(units * 0.7)))
            pen.lineTo((0, int(units * 0.7)))
            pen.closePath()
        glyphs[name] = pen.glyph() if is_ttf else pen.getCharString()
        metrics[name] = (advance, 0)
    builder.setupHorizontalMetrics(metrics)
    builder.setupHorizontalHeader(ascent=int(units * 0.8), descent=-int(units * 0.2))
    builder.setupNameTable(
        {
            "familyName": "PdfTranslateTest",
            "styleName": "Regular",
            "psName": "PdfTranslateTest-Regular",
            "fullName": "PdfTranslateTest Regular",
        }
    )
    builder.setupOS2(
        sTypoAscender=int(units * 0.8),
        sTypoDescender=-int(units * 0.2),
        usWinAscent=int(units * 0.8),
        usWinDescent=int(units * 0.2),
    )
    builder.setupPost()
    if is_ttf:
        builder.setupGlyf(glyphs)
    else:
        builder.setupCFF("PdfTranslateTest-Regular", {}, glyphs, {})
    builder.setupMaxp()
    builder.save(path)


if __name__ == "__main__":
    root = Path(__file__).resolve().parent
    build_font(root / "test.ttf", True)
    build_font(root / "test.otf", False)
