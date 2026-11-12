# PDF Translate for Zotero

A small Zotero 7+ plugin that adds a **翻译** button to the PDF reader and a
**Translate PDF** item context menu entry. It runs the `pdf-translate` CLI on
the current attachment, shows the CLI's stages in a Zotero progress window, and
imports a separate side-by-side bilingual PDF as a child attachment. Each page
has the original on the left and the translation on the right. If a matching
bilingual attachment or cache already exists, the plugin opens it directly
instead of translating again.

This is the first vertical slice of Stage 4 in `docs/DESIGN.md`. It does not
start a local HTTP server or draw paragraph overlays yet; the plugin calls the
single binary directly.

## Build and install

```bash
# from the repository root
cargo build --release
./zotero/build.sh
```

The XPI is written to `zotero/dist/pdf-translate-zotero-<version>.xpi`. In
Zotero, open **Tools → Plugins**, click the gear menu, choose **Install Plugin
From File…**, and select the XPI.

The plugin and Rust binary share the package version in `Cargo.toml`. Keep
`zotero/manifest.json` at that same version when releasing; the build script
rejects mismatches before writing an XPI, and the manifest test checks it too.

Put the binary somewhere the plugin can find it:

```bash
mkdir -p ~/.local/bin
cp target/release/pdf-translate ~/.local/bin/
```

The plugin searches `PATH`, then `/usr/local/bin`, `/usr/bin`, `~/.local/bin`
and `~/.cargo/bin`. Both Linux and macOS are supported. On macOS, GUI
applications often do not inherit the shell `PATH`, so installing to
`/usr/local/bin` or setting the path explicitly in the plugin settings is the
most reliable choice.

Configuration (API endpoint, model, key, concurrency, font, …) comes from the
normal `pdf-translate` config file, so the plugin needs no separate API
settings:

- Linux/macOS: `~/.config/pdf-translate/config.toml`
- Windows: `%APPDATA%\pdf-translate\config.toml`

Use **Tools → PDF Translate: Check command** to verify that Zotero can run the
binary.

## Usage

- Open a PDF in Zotero and click **翻译** in the reader toolbar.
- Or right-click a PDF attachment / a parent item and choose **Translate PDF**.
  If several items are selected, the jobs run one after another.
- The plugin first looks for an existing bilingual attachment for this paper
  (identified by its hidden note) and opens it if its file is newer than the
  source PDF. Otherwise it checks the cache descriptor and reuses that. Only
  when neither is valid does it run `pdf-translate` again.
- The bilingual PDF is added under the original parent item and opened in a
  reader tab of the same Zotero window. Disable **Import the translated PDF as
  an attachment** to keep the library untouched and open the cache in the
  system viewer instead.

Cached translations are stored as `<key>.<lang>.bilingual.pdf` next to a
`<key>.<lang>.bilingual.pdf.json` descriptor containing the source mtime/size,
binary mtime and settings. Changing the paper, the settings or the binary
invalidates the cache and triggers a new run.

The plugin's own attachments carry a hidden note beginning with
`pdf-translate:<source-key>:<lang>:<mode>`; the plugin uses it to find and hide
its own outputs. Imported titles look like `Bilingual (zh-CN)` or
`双语对照（zh-CN）`.

Intermediate PDFs and logs live in:

```text
<Zotero data directory>/pdf-translate/outputs
<Zotero data directory>/pdf-translate/logs
```

The **Open data folder** button in the plugin settings opens this directory.

## Settings

The plugin settings are under **Edit → Settings → PDF Translate**:

| Setting | Default | Purpose |
| --- | --- | --- |
| Executable | empty | Empty means search `PATH` and common install locations |
| Target language | `zh-CN` | Passed as `--target-lang` |
| Pages | `1-` | Passed as `--pages` when not `1-` |
| Composition mode | `clean` | `clean` or `overlay` |
| Side-by-side bilingual | on | Add `--bilingual`: original left, translation right |
| Translate the reference list | off | `--translate-references` |
| Keep link annotations | off | `--keep-annotations` |
| Fake mode | off | `--fake`, no API call; useful for testing |
| Extra arguments | empty | Appended to the command; quotes and backslashes work |
| Config file | empty | Passed as `--config` when set |
| Import as attachment | on | Import the result into the library; otherwise open the cache |
| Open when done | on | Open the cached or imported translation |

## Development

```bash
# syntax-check the JavaScript
node --check zotero/bootstrap.js
node --check zotero/content/pdf-translate.js
node --check zotero/content/preferences.js
node --check zotero/content/util.js

# pure-helper tests (no Zotero required)
node --test zotero/test/*.test.mjs

# build the XPI
./zotero/build.sh
```

After reinstalling the XPI, reload Zotero or disable/enable the plugin. Plugin
errors are printed to **Help → Debug Output Logging → View Output** and to
`<Zotero data directory>/pdf-translate/logs`.

### Layout

```text
zotero/
  manifest.json          Zotero 7+ manifest
  bootstrap.js           startup/shutdown hooks
  prefs.js               default preferences
  content/
    pdf-translate.js     menus, reader button, queue, subprocess, import
    preferences.xhtml    settings pane
    preferences.js       settings behavior
    util.js              pure argument/progress helpers
    icon.svg
  locale/en-US, zh-CN    Fluent strings
  test/util.test.mjs
  build.sh
```

The plugin uses `Subprocess.call` with `stderr: "stdout"` and parses the CLI's
existing human-readable output. If the CLI output format changes, update
`parseProgressLine()` in `content/util.js` and its tests.

Zotero 10 requires `applications.zotero.update_url` in `manifest.json`.
Omitting it makes the installer report a generic incompatibility (AddonManager
error `-3`, parse error), even for a local XPI. Keep an HTTPS update URL in the
manifest; `zotero/test/manifest.test.mjs` guards this and the other required
fields.

## Limitations

- Whole-document translation only. Per-paragraph overlays and a local HTTP/SSE
  service are still planned.
- The bilingual composer does not support pages with a non-zero `/Rotate` yet,
  and does not copy annotations or links from the original pages.
- Table cells and the reference list are intentionally kept as original text;
  a protected page therefore appears unchanged on both sides.
- Zotero's plugin sandbox must be able to execute the binary. Linux package
  managers and macOS Gatekeeper may require an explicit path or a signed
  binary.
- Re-running after changing settings creates another bilingual attachment; the
  plugin does not overwrite older outputs.
