# AGENTS.md

Repo-specific guidance for OpenCode sessions. `README.md` is the user-facing reference; `docs/DESIGN.md` is a forward-looking Chinese design plan, not a description of the current tree.

## Commands

- Verify with `cargo fmt && cargo test && cargo clippy --all-targets`. Keep the tree fmt/clippy clean.
- There is no library target. `cargo test` runs unit tests inside `src/**` plus `tests/cli_smoke.rs`; use `cargo test --test cli_smoke` for only the CLI tests. Filter with `cargo test <substring>`.
- Iterate with debug builds. `[profile.release]` uses thin LTO, `codegen-units = 1`, `strip`, and embeds the compressed CJK fonts, so release builds are much slower.
- Build the optional desktop GUI with `--features gui`, launch it with `pdf-translate gui [input.pdf]`, and verify it with `cargo test --features gui` and `cargo clippy --all-targets --features gui`. GUI uses eframe/egui, rfd and open; it must start on the main thread while translation and preview rendering stay on large-stack workers.
- Manual smoke test without a model: `cargo run -- translate <pdf> --fake --limit 5 -o /tmp/out.pdf`. Inspect layout JSON with `cargo run -- inspect <pdf> --pages 1`. `samples/` is gitignored/local-only.
- The test suite is offline and needs no API key or network. Keep new tests offline (use `--fake`, fake clients, or pure functions).
- Zotero plugin: `node --test zotero/test/*.test.mjs`; syntax-check the scripts with `node --check`; build the XPI with `./zotero/build.sh` (output is gitignored under `zotero/dist/`).
- Keep the Zotero manifest version equal to the Rust package version in `Cargo.toml`. The plugin build rejects mismatches before packaging.

## Architecture

- Single binary crate rooted at `src/main.rs`; there is no `lib.rs`. Unit tests live in the module files.
- Pipeline: extract (`hayro`) → layout → translate (OpenAI-compatible API + JSON cache) → typeset (CJK) → compose.
  - `src/workflow.rs` builds shared translation requests and runs the pipeline for CLI/GUI; `src/progress.rs` defines structured progress and cooperative cancellation.
  - `src/gui.rs` and `src/gui/preview.rs` implement the optional native GUI, including system/light/dark themes and hayro page previews.
  - `src/pdf/`: PDF parsing, content-stream rewriting, fonts, composition (`clean` default deletes replaced glyphs; `overlay` covers the original). Link rectangles are remapped onto the translated text by default; `--keep-annotations` keeps them in place.
  - `src/layout/`: line/paragraph/column heuristics.
  - `src/model.rs`: shared layout structures; `src/typeset.rs`: CJK wrapping and font fitting.
- The inline `regression` module in `src/pdf.rs` is the main end-to-end safety net for `pdf/rewrite`, `pdf/writer`, `pdf/font`, and typeset changes. It renders outputs with hayro and asserts visible text, absent source bytes, clipping/rotation, etc. Run the full suite after touching those areas.
- `src/pdf/bilingual.rs` composes the `--bilingual` output: each page embeds the original and translated page as Form XObjects side by side. It expects matching page counts/geometry and supports `/Rotate` 0/90/180/270 with cropped page origins.
- Module convention: `src/foo.rs` declares `src/foo/bar.rs`; never add `mod.rs`.
- `docs/DESIGN.md` proposes a lib + multiple bin targets and files that do not exist. Trust `src/` and the README "Module layout" section.
- `zotero/` is a Zotero 7+ plugin (manifest v2, bootstrapped JS). It calls the built CLI through `Subprocess`, parses its merged stdout/stderr for progress, and imports a side-by-side bilingual PDF as a child attachment. It reopens an existing bilingual attachment or cache before translating again. The plugin is not part of the Cargo workspace.

## Tests and fixtures

- Keep unit and internal regression tests in inline `#[cfg(test)]` modules beside the implementation; do not add standalone test-only files under `src/`. CLI integration tests belong under `tests/`.
- Test PDFs are generated in memory with `lopdf`. Regression tests use the checked-in `tests/fixtures/test.ttf` / `test.otf`; normal `cargo test` needs no Python or system CJK font. Regenerating fixtures needs Python + fontTools (`python3 tests/fixtures/build_fonts.py`).
- The fixture fonts are intentionally minimal: U+31350 is absent and U+2026/U+22EF share a glyph. Do not replace them with system fonts or regenerate casually.
- `*.pdf` is gitignored except `tests/fixtures/*.pdf`; `/samples` is fully ignored. Do not make tests depend on `samples/` or local files; reuse the `Fixture` helper in `src/pdf.rs`'s inline `regression` module or build a PDF with `lopdf`.
- CLI smoke tests inherit the developer's config (`PDF_TRANSLATE_CONFIG` or `~/.config/pdf-translate/config.toml`). If they fail while parsing config, unset that variable or move the file.
- Translation cache: `translation_key()` in `src/translate.rs` uses v4 fake and v35 real keys, stored at `$XDG_CACHE_HOME/pdf-translate/translations.json`. Latin ligatures are expanded in model input and cache keys without changing original PDF glyphs. v29 preserves decimal list labels locally; v30 also rejects newly introduced kana and Hangul in Chinese prose. v31 retries language failures with concise prompts and sentence chunks. v32 uses a positive target-language instruction on the final retry. v34 preserves original list-symbol glyphs and migrates validated literal-bullet entries by rebinding marker IDs without retranslating prose. v35 carries per-span colors with validated scope; mixed-color inputs get new keys, while unchanged v34 inputs can migrate. Compatible v28/v29/v30/v31/v32/v34 replies, v27 marked text, v24 short fragments, v23 enumerations and v21 plain text migrate only after passing all current markup/language/list-label validators. Older v17/v19 plain-text entries may also migrate; pre-v27 marked text never migrates. Bump the affected version when changing prompts, markup validation, or cache semantics, or stale translations will be silently reused.

## Config and build quirks

- Default features embed the regular Noto Serif CJK SC face and the FandolKai italic face (`assets/fonts/*.zst`; `embedded-italic-font` is on by default, GPL v3 + font exception); `embedded-bold-font` adds the bold face. A DejaVu Serif symbol face (Bitstream Vera / DejaVu license) covers glyphs absent from every CJK face; typeset and writer select faces per character (style face → regular → bold → italic → symbol). Without the italic feature, italic falls back to system Kaiti faces or synthetic slant; `--no-default-features` switches to system fonts and needs a CJK face at runtime.
- Precedence: CLI flags > `--config`/`PDF_TRANSLATE_CONFIG` > `OPENAI_*` env > built-in defaults. Unknown TOML keys are rejected; an explicitly selected config file must exist. GUI settings save back to the selected config, and `[gui].theme` accepts `system`, `light` or `dark`.
- `--fake` needs no API key; a loopback API base also works without one.
- MIT project: BabelDOC and DocLayout-YOLO are AGPL and only appear as optional references in the design doc; do not port their code/models without a licensing decision.
