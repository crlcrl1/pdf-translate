pdf-translate-menu-translate = Translate PDF
pdf-translate-menu-check = PDF Translate: Check command

reader-button = Translate
reader-button-title = Translate this PDF with pdf-translate

pref-binary-heading = pdf-translate command
pref-binary = Executable
pref-binary-input =
    .placeholder = Leave empty to search PATH
pref-detect = Detect
pref-test = Test
pref-open-data = Open data folder

pref-translation-heading = Translation
pref-target-lang = Target language
pref-pages = Pages
pref-mode = Composition mode
pref-mode-clean = Clean (replace original glyphs)
pref-mode-overlay = Overlay (cover original text)
pref-translate-references = Translate the reference list
pref-keep-annotations = Keep link annotations in place
pref-fake = Fake mode (no API; prefixes text with [译])
pref-extra-args = Extra arguments
pref-extra-args-hint = Passed to `pdf-translate translate` after the options above. Quotes and backslashes are supported.
pref-config = Config file
pref-config-input =
    .placeholder = Default: ~/.config/pdf-translate/config.toml

pref-output-heading = Output
pref-bilingual = Generate a side-by-side bilingual PDF (original left, translation right)
pref-import = Import the translated PDF as an attachment
pref-open-when-done = Open the translated PDF when done

status-checking = Checking…
status-ok = Ready: { $version }
status-missing = pdf-translate was not found

env-ok = pdf-translate is ready.
    { $version }
    { $path }
env-missing = pdf-translate is not available: { $detail }

error-no-pdf = Select a PDF attachment or a parent item that contains one.
error-no-file = The PDF file could not be found. It may be a linked file that is not available.
error-already-running = This PDF is already being translated.
error-binary-missing = The configured pdf-translate executable does not exist: { $path }
error-binary-not-found = pdf-translate was not found. Build it and add it to PATH, or set the executable path in Settings.
error-version = pdf-translate at { $path } failed to report its version.
error-spawn = Could not start pdf-translate: { $detail }
error-no-output = pdf-translate finished without writing an output PDF.
error-failed = Translation failed: { $detail }
error-failed-with-log = Translation failed: { $detail }
    Log: { $log }
error-unknown = unknown error

progress-headline = PDF Translate
progress-binary = Locating pdf-translate…
progress-cached = Using cached translation…
progress-starting = Starting…
progress-stage-layout = Analyzing layout…
progress-stage-translate = Translating…
progress-stage-translation = Translation finished…
progress-stage-typeset = Typesetting…
progress-stage-write = Writing PDF…
progress-stage-error = Error
progress-stage-log = Working…
progress-finished = Adding the translated PDF…
progress-done = Done: { $title }

attachment-title = Translation ({ $lang })
attachment-title-bilingual = Bilingual ({ $lang })
untitled = Untitled PDF
