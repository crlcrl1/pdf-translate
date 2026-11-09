# AGENTS.md

Repo-specific guidance for OpenCode sessions. `README.md` is the user-facing reference; `docs/DESIGN.md` is a forward-looking Chinese design plan, not a description of the current tree.

## Commands

- Verify with `cargo fmt && cargo test && cargo clippy --all-targets`. Keep the tree fmt/clippy clean.
- There is no library target. `cargo test` runs unit tests inside `src/**` plus `tests/cli_smoke.rs`; use `cargo test --test cli_smoke` for only the CLI tests. Filter with `cargo test <substring>`.
- Iterate with debug builds. `[profile.release]` uses thin LTO, `codegen-units = 1`, `strip`, and embeds the compressed CJK fonts, so release builds are much slower.
- Manual smoke test without a model: `cargo run -- translate <pdf> --fake --limit 5 -o /tmp/out.pdf`. Inspect layout JSON with `cargo run -- inspect <pdf> --pages 1`. `samples/` is gitignored/local-only.
- The test suite is offline and needs no API key or network. Keep new tests offline (use `--fake`, fake clients, or pure functions).

## Architecture

- Single binary crate rooted at `src/main.rs`; there is no `lib.rs`. Unit tests live in the module files.
- Pipeline: extract (`hayro`) → layout → translate (OpenAI-compatible API + JSON cache) → typeset (CJK) → compose.
  - `src/pdf/`: PDF parsing, content-stream rewriting, fonts, composition (`clean` default deletes replaced glyphs; `overlay` covers the original).
  - `src/layout/`: line/paragraph/column heuristics.
  - `src/model.rs`: shared layout structures; `src/typeset.rs`: CJK wrapping and font fitting.
- `src/pdf/regression.rs` is the main end-to-end safety net for `pdf/rewrite`, `pdf/writer`, `pdf/font`, and typeset changes. It renders outputs with hayro and asserts visible text, absent source bytes, clipping/rotation, etc. Run the full suite after touching those areas.
- Module convention: `src/foo.rs` declares `src/foo/bar.rs`; never add `mod.rs`.
- `docs/DESIGN.md` proposes a lib + multiple bin targets and files that do not exist. Trust `src/` and the README "Module layout" section.

## Tests and fixtures

- Test PDFs are generated in memory with `lopdf`. Regression tests use the checked-in `tests/fixtures/test.ttf` / `test.otf`; normal `cargo test` needs no Python or system CJK font. Regenerating fixtures needs Python + fontTools (`python3 tests/fixtures/build_fonts.py`).
- The fixture fonts are intentionally minimal: U+31350 is absent and U+2026/U+22EF share a glyph. Do not replace them with system fonts or regenerate casually.
- `*.pdf` is gitignored except `tests/fixtures/*.pdf`; `/samples` is fully ignored. Do not make tests depend on `samples/` or local files; reuse the `Fixture` helper in `src/pdf/regression.rs` or build a PDF with `lopdf`.
- CLI smoke tests inherit the developer's config (`PDF_TRANSLATE_CONFIG` or `~/.config/pdf-translate/config.toml`). If they fail while parsing config, unset that variable or move the file.
- Translation cache: `translation_key()` in `src/translate.rs` uses versioned keys (v4 fake, v5 real) and stores them at `$XDG_CACHE_HOME/pdf-translate/translations.json`. Bump the version when changing prompts, markup validation, or cache semantics, or stale translations will be silently reused.

## Config and build quirks

- Default features embed the regular Noto Serif CJK SC face (`assets/fonts/*.zst`); `embedded-bold-font` adds the bold face; `--no-default-features` switches to system fonts and needs a CJK face at runtime.
- Precedence: CLI flags > `--config`/`PDF_TRANSLATE_CONFIG` > `OPENAI_*` env > built-in defaults. Unknown TOML keys are rejected; an explicitly selected config file must exist.
- `--fake` needs no API key; a loopback API base also works without one.
- MIT project: BabelDOC and DocLayout-YOLO are AGPL and only appear as optional references in the design doc; do not port their code/models without a licensing decision.
