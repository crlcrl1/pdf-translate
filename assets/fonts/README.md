# Embedded fonts

These fonts are embedded into the `pdf-translate` binary when the default
`embedded-fonts` Cargo feature is enabled.

- `NotoSerifCJKsc-Regular.otf.zst` (always embedded)
- `NotoSerifCJKsc-Bold.otf.zst` (only when `embedded-bold-font` is enabled)

They are zstd-compressed copies of the Simplified Chinese (SC) faces extracted
from the Noto Serif CJK OpenType collection (`NotoSerifCJK-Regular.ttc` /
`NotoSerifCJK-Bold.ttc`). Decompress with:

```bash
zstd -d NotoSerifCJKsc-Regular.otf.zst
```

Source: https://github.com/notofonts/noto-cjk

License: SIL Open Font License 1.1, see `LICENSE`.
