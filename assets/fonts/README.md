# Embedded fonts

These fonts are embedded into the `pdf-translate` binary when the default
`embedded-fonts` Cargo feature is enabled.

- `NotoSerifCJKsc-Regular.otf.zst` (always embedded)
- `NotoSerifCJKsc-Bold.otf.zst` (only when `embedded-bold-font` is enabled)
- `FandolKai-Regular.otf.zst` (on by default via `embedded-italic-font`)
- `DejaVuSerif.ttf.zst` (always embedded; symbol fallback face)

The Noto assets are zstd-compressed copies of the Simplified Chinese (SC) faces
extracted from the Noto Serif CJK OpenType collection (`NotoSerifCJK-Regular.ttc` /
`NotoSerifCJK-Bold.ttc`). Decompress with:

```bash
zstd -d NotoSerifCJKsc-Regular.otf.zst
```

Source: https://github.com/notofonts/noto-cjk

License: SIL Open Font License 1.1, see `LICENSE`.

The italic asset is a zstd-compressed, unmodified copy of FandolKai Regular
from the Fandol package, used as the real italic face because Noto Serif CJK
has none. Decompress with:

```bash
zstd -d FandolKai-Regular.otf.zst
```

Source: https://www.ctan.org/pkg/fandol

License: GPL v3 with the GPL font exception, see `LICENSE-Fandol`. The font
keeps its own license while the repository code remains MIT; keep the asset
unmodified and ship `LICENSE-Fandol` with any redistribution. Proprietary
fonts (for example Adobe Kaiti Std) must not be added to this directory.

The symbol asset is a zstd-compressed, unmodified copy of DejaVu Serif 2.37,
used per character for glyphs absent from every CJK face (math brackets,
geometric shapes, ...). Decompress with:

```bash
zstd -d DejaVuSerif.ttf.zst
```

Source: https://github.com/dejavu-fonts/dejavu-fonts

License: Bitstream Vera / DejaVu license, see `LICENSE-DejaVu`.