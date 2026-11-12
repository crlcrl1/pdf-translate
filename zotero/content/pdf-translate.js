/* global Zotero, Services, ChromeUtils, IOUtils, PathUtils, Localization, PdfTranslateUtil */

var { Subprocess } = ChromeUtils.importESModule("resource://gre/modules/Subprocess.sys.mjs");

const PREF_BRANCH = "extensions.pdf-translate.";
const PLUGIN_NAME = "PDF Translate";
const LOG_LINES_KEPT = 400;
const OUTPUT_NOTE_PREFIX = "pdf-translate:";

/** An error whose message is already localized for the user. */
class UserError extends Error {}

var PdfTranslate = {
  id: null,
  version: null,
  rootURI: null,
  l10n: null,
  active: false,
  readerHandler: null,
  menuIDs: [],
  prefPaneID: null,
  queue: [],
  pumping: false,
  running: new Set(),

  init({ id, version, rootURI }) {
    this.id = id;
    this.version = version;
    this.rootURI = rootURI;
  },

  async startup() {
    this.l10n = new Localization(["pdf-translate.ftl"], true);
    // The preferences pane runs in its own scope and reaches the plugin here.
    Zotero.PdfTranslate = this;
    this.registerReader();
    this.registerMenus();
    this.prefPaneID = await Zotero.PreferencePanes.register({
      pluginID: this.id,
      src: this.rootURI + "content/preferences.xhtml",
      scripts: [this.rootURI + "content/preferences.js"],
    });
    for (let win of Zotero.getMainWindows()) {
      this.addToWindow(win);
    }
    this.active = true;
    Zotero.debug(`[pdf-translate] started ${this.version} (${this.id})`);
  },

  shutdown() {
    this.active = false;
    this.queue = [];
    Zotero.debug(`[pdf-translate] shutting down ${this.version}`);
    if (this.readerHandler) {
      Zotero.Reader.unregisterEventListener("renderToolbar", this.readerHandler);
      this.readerHandler = null;
    }
    for (let menuID of this.menuIDs) {
      Zotero.MenuManager.unregisterMenu(menuID);
    }
    this.menuIDs = [];
    if (this.prefPaneID) {
      Zotero.PreferencePanes.unregister(this.prefPaneID);
      this.prefPaneID = null;
    }
    for (let win of Zotero.getMainWindows()) {
      this.removeFromWindow(win);
    }
    delete Zotero.PdfTranslate;
  },

  addToWindow(win) {
    win.MozXULElement.insertFTLIfNeeded("pdf-translate.ftl");
  },

  removeFromWindow(win) {
    win.document.querySelector('[href="pdf-translate.ftl"]')?.remove();
  },

  // Localization and preferences

  t(id, args) {
    return this.l10n.formatValueSync(id, args) || id;
  },

  getPref(name) {
    return Zotero.Prefs.get(PREF_BRANCH + name, true);
  },

  setPref(name, value) {
    Zotero.Prefs.set(PREF_BRANCH + name, value, true);
  },

  get settings() {
    let mode = this.getPref("mode");
    if (mode !== "overlay") {
      mode = "clean";
    }
    return {
      binaryPath: (this.getPref("binaryPath") || "").trim(),
      configPath: (this.getPref("configPath") || "").trim(),
      targetLang: (this.getPref("targetLang") || "zh-CN").trim() || "zh-CN",
      pages: (this.getPref("pages") || "1-").trim() || "1-",
      mode,
      bilingual: !!this.getPref("bilingual"),
      translateReferences: !!this.getPref("translateReferences"),
      keepAnnotations: !!this.getPref("keepAnnotations"),
      fake: !!this.getPref("fake"),
      extraArgs: (this.getPref("extraArgs") || "").trim(),
      importAttachment: !!this.getPref("importAttachment"),
      openWhenDone: !!this.getPref("openWhenDone"),
    };
  },

  get dataDir() {
    return PathUtils.join(Zotero.DataDirectory.dir, "pdf-translate");
  },

  get outputDir() {
    return PathUtils.join(this.dataDir, "outputs");
  },

  // Menus and reader integration

  registerMenus() {
    this.menuIDs = [
      Zotero.MenuManager.registerMenu({
        menuID: "pdf-translate-item",
        pluginID: this.id,
        target: "main/library/item",
        menus: [{
          menuType: "menuitem",
          l10nID: "pdf-translate-menu-translate",
          icon: this.rootURI + "content/icon.svg",
          onShowing: (event, context) => {
            context.setVisible(this.getPDFAttachments(context.items).length > 0);
          },
          onCommand: (event, context) => {
            this.enqueue(this.getPDFAttachments(context.items));
          },
        }],
      }),
      Zotero.MenuManager.registerMenu({
        menuID: "pdf-translate-tools",
        pluginID: this.id,
        target: "main/menubar/tools",
        menus: [{
          menuType: "menuitem",
          l10nID: "pdf-translate-menu-check",
          icon: this.rootURI + "content/icon.svg",
          onCommand: () => this.showEnvironment(),
        }],
      }),
    ].filter(Boolean);
  },

  registerReader() {
    this.readerHandler = (event) => {
      if (event.reader?.type !== "pdf") {
        return;
      }
      let doc = event.doc;
      if (!doc) {
        return;
      }
      let attachment = Zotero.Items.get(event.reader.itemID);
      if (!attachment || this.isTranslationOutput(attachment)) {
        return;
      }
      this.installReaderStyle(doc);
      let wrapper = doc.createElement("span");
      wrapper.className = "pdf-translate-toolbar";
      let button = doc.createElement("button");
      button.className = "pdf-translate-toolbar-button";
      button.textContent = this.t("reader-button");
      button.title = this.t("reader-button-title");
      button.addEventListener("click", () => {
        this.enqueue([attachment]);
      });
      wrapper.append(button);
      event.append(wrapper);
    };
    Zotero.Reader.registerEventListener("renderToolbar", this.readerHandler, this.id);
  },

  installReaderStyle(doc) {
    if (doc.getElementById("pdf-translate-style")) {
      return;
    }
    let style = doc.createElement("style");
    style.id = "pdf-translate-style";
    style.textContent = `
      .pdf-translate-toolbar { display: inline-flex; align-items: center; }
      .pdf-translate-toolbar-button {
        font: inherit; color: var(--fill-primary, #222); white-space: nowrap;
        padding: 3px 7px; border: 1px solid transparent; border-radius: 3px;
        background: transparent; cursor: pointer;
      }
      .pdf-translate-toolbar-button:hover { background: var(--fill-quinary, #e6e6e6); }
      .pdf-translate-toolbar-button[disabled] { opacity: .5; cursor: default; }
    `;
    doc.head.append(style);
  },

  // Attachment selection

  isPDFAttachment(item) {
    return !!item?.isPDFAttachment?.();
  },

  isTranslationOutput(item) {
    let note = item?.getNote?.() || "";
    return note.startsWith(OUTPUT_NOTE_PREFIX);
  },

  getPDFAttachments(items) {
    let attachments = [];
    let seen = new Set();
    for (let item of items || []) {
      if (!item) {
        continue;
      }
      let candidates = item.isRegularItem() ? Zotero.Items.get(item.getAttachments()) : [item];
      for (let candidate of candidates) {
        if (
          !candidate
          || !this.isPDFAttachment(candidate)
          || candidate.attachmentLinkMode === Zotero.Attachments.LINK_MODE_LINKED_URL
          || this.isTranslationOutput(candidate)
          || seen.has(candidate.id)
        ) {
          continue;
        }
        seen.add(candidate.id);
        attachments.push(candidate);
        break;
      }
    }
    return attachments;
  },

  // Queue

  enqueue(attachments, options = {}) {
    if (!this.active) {
      return;
    }
    let jobs = (attachments || []).filter(Boolean);
    if (!jobs.length) {
      Zotero.alert(null, PLUGIN_NAME, this.t("error-no-pdf"));
      return;
    }
    for (let attachment of jobs) {
      this.queue.push({ attachment, options });
    }
    this.pump().catch((error) => Zotero.logError(error));
  },

  async pump() {
    if (this.pumping) {
      return;
    }
    this.pumping = true;
    try {
      while (this.active && this.queue.length) {
        let { attachment, options } = this.queue.shift();
        await this.translateAttachment(attachment, options);
      }
    }
    finally {
      this.pumping = false;
    }
  },

  // Translation

  async findExistingBilingual(attachment, settings) {
    if (!settings.bilingual) {
      return null;
    }
    let marker = `${OUTPUT_NOTE_PREFIX}${attachment.key}:${settings.targetLang}:${settings.mode}:bilingual`;
    let candidates = [];
    if (attachment.parentItemID) {
      let parent = Zotero.Items.get(attachment.parentItemID);
      candidates = Zotero.Items.get(parent?.getAttachments() || []);
    }
    else {
      let items = await Zotero.Items.getAll(attachment.libraryID);
      candidates = items.filter((item) => item.isAttachment());
    }
    for (let item of candidates) {
      if (item.deleted || !item.isPDFAttachment?.()) {
        continue;
      }
      let note = "";
      try {
        note = item.getNote() || "";
      }
      catch {
        continue;
      }
      if (note === marker && await item.getFilePathAsync()) {
        return item;
      }
    }
    return null;
  },

  async translateAttachment(attachment, options = {}) {
    let key = `${attachment.libraryID}/${attachment.key}`;
    if (this.running.has(key)) {
      Zotero.alert(null, PLUGIN_NAME, this.t("error-already-running"));
      return;
    }
    this.running.add(key);

    let settings = this.settings;
    if (options.noImport) {
      settings.importAttachment = false;
    }
    if (options.openWhenDone !== undefined) {
      settings.openWhenDone = options.openWhenDone;
    }

    let progress = this.createProgress(attachment);
    try {
      let input = await attachment.getFilePathAsync();
      if (!input) {
        throw new UserError(this.t("error-no-file"));
      }

      // Reuse an existing bilingual attachment before touching the binary or
      // the model. This is what makes a second click open the old version.
      if (settings.importAttachment) {
        let existing = await this.findExistingBilingual(attachment, settings);
        if (existing) {
          let path = await existing.getFilePathAsync();
          let sourceStat = await IOUtils.stat(input);
          let outputStat = await IOUtils.stat(path);
          if (outputStat.lastModified >= sourceStat.lastModified) {
            progress.status(this.t("progress-cached"), 100);
            if (settings.openWhenDone) {
              await Zotero.Reader.open(existing.id);
            }
            progress.done(this.t("progress-done", { title: existing.getField("title") }));
            return;
          }
        }
      }

      progress.status(this.t("progress-binary"), 1);
      let binary = await this.findBinary();
      let output = PathUtils.join(
        this.outputDir,
        PdfTranslateUtil.outputFileName(
          attachment.key,
          settings.targetLang,
          settings.mode,
          settings.bilingual,
        ),
      );
      let metadataPath = `${output}.json`;
      await IOUtils.makeDirectory(this.outputDir, { ignoreExisting: true });
      let sourceStat = await IOUtils.stat(input);
      let binaryStat = await IOUtils.stat(binary);
      let currentCache = PdfTranslateUtil.cacheState({
        source: input,
        sourceMtime: Math.round(sourceStat.lastModified),
        sourceSize: sourceStat.size,
        binary,
        binaryMtime: Math.round(binaryStat.lastModified),
        pluginVersion: this.version,
        configPath: settings.configPath,
        targetLang: settings.targetLang,
        pages: settings.pages,
        mode: settings.mode,
        bilingual: settings.bilingual,
        translateReferences: settings.translateReferences,
        keepAnnotations: settings.keepAnnotations,
        fake: settings.fake,
        extraArgs: settings.extraArgs,
      });

      let cached = null;
      if (await IOUtils.exists(output) && await IOUtils.exists(metadataPath)) {
        try {
          cached = JSON.parse(await IOUtils.readUTF8(metadataPath));
        }
        catch (error) {
          Zotero.logError(error);
        }
      }
      if (cached && PdfTranslateUtil.cacheMatches(cached, currentCache)) {
        progress.status(this.t("progress-cached"), 100);
        let result = await this.finish(attachment, output, settings, cached);
        progress.done(this.t("progress-done", { title: result.title }));
        return;
      }

      await IOUtils.remove(output, { ignoreAbsent: true });
      await IOUtils.remove(metadataPath, { ignoreAbsent: true });
      let args = PdfTranslateUtil.buildArgs({
        input,
        output,
        configPath: settings.configPath,
        targetLang: settings.targetLang,
        pages: settings.pages,
        mode: settings.mode,
        bilingual: settings.bilingual,
        translateReferences: settings.translateReferences,
        keepAnnotations: settings.keepAnnotations,
        fake: settings.fake,
        extraArgs: settings.extraArgs,
      });
      Zotero.debug(`[pdf-translate] ${binary} ${args.map((arg) => JSON.stringify(arg)).join(" ")}`);

      progress.status(this.t("progress-starting"), 2);
      let { exitCode, log } = await this.run(binary, args, (line) => {
        let parsed = PdfTranslateUtil.parseProgressLine(line);
        if (!parsed) {
          return;
        }
        let label = this.t(`progress-stage-${parsed.stage}`);
        let detail = parsed.stage === "translate" || parsed.stage === "log"
          ? PdfTranslateUtil.truncate(parsed.detail, 90)
          : "";
        progress.status(detail ? `${label} · ${detail}` : label, parsed.percent);
      });
      if (exitCode !== 0) {
        throw new UserError(await this.failureMessage(log));
      }
      if (!(await IOUtils.exists(output))) {
        throw new UserError(this.t("error-no-output"));
      }

      progress.status(this.t("progress-finished"), 99);
      let result = await this.finish(attachment, output, settings);
      if (result.attachmentID) {
        currentCache.attachmentID = result.attachmentID;
      }
      await IOUtils.writeUTF8(metadataPath, JSON.stringify(currentCache, null, 2));
      progress.done(this.t("progress-done", { title: result.title }));
    }
    catch (error) {
      let message = error instanceof UserError ? error.message : String(error?.message || error);
      progress.fail(message);
      Zotero.logError(error);
      Zotero.alert(null, PLUGIN_NAME, message);
    }
    finally {
      this.running.delete(key);
    }
  },

  async finish(original, output, settings, cached = null) {
    if (!settings.importAttachment) {
      if (settings.openWhenDone) {
        await this.openCached(output);
      }
      else {
        Zotero.File.reveal(output);
      }
      return { title: PathUtils.filename(output) };
    }
    let attachment = cached?.attachmentID ? Zotero.Items.get(cached.attachmentID) : null;
    if (!attachment || attachment.deleted) {
      attachment = await this.importOutput(original, output, settings);
    }
    if (settings.openWhenDone) {
      await Zotero.Reader.open(attachment.id);
    }
    return { title: attachment.getField("title"), attachmentID: attachment.id };
  },

  async openCached(path) {
    try {
      await Zotero.launchFile(path);
    }
    catch (error) {
      Zotero.logError(error);
      Zotero.File.reveal(path);
    }
  },

  async importOutput(original, output, settings) {
    let parentItemID = original.parentItemID || undefined;
    let title = this.t(
      settings.bilingual ? "attachment-title-bilingual" : "attachment-title",
      { lang: settings.targetLang },
    );
    if (parentItemID) {
      let parent = Zotero.Items.get(parentItemID);
      let siblings = Zotero.Items.get(parent?.getAttachments() || []);
      let count = siblings.filter((item) => item.getField("title") === title).length;
      if (count) {
        title += ` (${count + 1})`;
      }
    }
    let baseName = Zotero.File.truncateFileName(
      (original.attachmentFilename || "translation").replace(/\.pdf$/i, ""),
      80,
    );
    let suffixes = [];
    if (settings.mode === "overlay") {
      suffixes.push("overlay");
    }
    if (settings.bilingual) {
      suffixes.push("bilingual");
    }
    let suffix = suffixes.length ? `.${suffixes.join(".")}` : "";
    let attachment = await Zotero.Attachments.importFromFile({
      file: output,
      libraryID: original.libraryID,
      parentItemID,
      collections: parentItemID ? undefined : original.getCollections(),
      fileBaseName: `${baseName}.${PdfTranslateUtil.safeLang(settings.targetLang)}${suffix}`,
      title,
    });
    attachment.setNote(`${OUTPUT_NOTE_PREFIX}${original.key}:${settings.targetLang}:${settings.mode}${settings.bilingual ? ":bilingual" : ""}`);
    await attachment.saveTx();
    return attachment;
  },

  createProgress(attachment) {
    let title = attachment.getField("title")
      || attachment.attachmentFilename
      || this.t("untitled");
    let window = new Zotero.ProgressWindow({ closeOnClick: false });
    window.changeHeadline(this.t("progress-headline"));
    let itemProgress = new window.ItemProgress("attachment-pdf", title);
    itemProgress.setProgress(1);
    window.show();
    let lastPercent = 1;
    return {
      status: (text, percent) => {
        itemProgress.setText(text);
        if (percent !== null && percent !== undefined) {
          // Parallel translation finishes paragraphs out of order, so keep the
          // bar from jumping backwards.
          lastPercent = Math.max(lastPercent, Math.round(percent));
          itemProgress.setProgress(Math.max(1, Math.min(100, lastPercent)));
        }
      },
      done: (text) => {
        itemProgress.setProgress(100);
        itemProgress.setText(text);
        window.startCloseTimer(5000);
      },
      fail: (text) => {
        itemProgress.setText(text);
        itemProgress.setError();
        window.startCloseTimer(12000);
      },
    };
  },

  async failureMessage(log) {
    let detail = PdfTranslateUtil.lastError(log) || this.t("error-unknown");
    let logPath = await this.saveLog(log).catch(() => "");
    return logPath
      ? this.t("error-failed-with-log", { detail, log: logPath })
      : this.t("error-failed", { detail });
  },

  async saveLog(lines) {
    let dir = PathUtils.join(this.dataDir, "logs");
    await IOUtils.makeDirectory(dir, { ignoreExisting: true });
    let name = `translate-${new Date().toISOString().replace(/[:.]/g, "-")}.log`;
    let path = PathUtils.join(dir, name);
    await IOUtils.writeUTF8(path, (lines || []).join("\n"));
    return path;
  },

  // Process handling

  async findBinary() {
    let custom = this.settings.binaryPath;
    if (custom) {
      if (await IOUtils.exists(custom)) {
        return custom;
      }
      throw new UserError(this.t("error-binary-missing", { path: custom }));
    }

    let name = Zotero.isWin ? "pdf-translate.exe" : "pdf-translate";
    try {
      return await Subprocess.pathSearch(name);
    }
    catch {
      // GUI apps on macOS do not inherit the shell's PATH.
    }

    let home = Services.dirsvc.get("Home", Ci.nsIFile).path;
    let candidates = Zotero.isWin
      ? [
          PathUtils.join(home, ".local", "bin", name),
          PathUtils.join(home, ".cargo", "bin", name),
        ]
      : [
          "/opt/homebrew/bin/pdf-translate",
          "/usr/local/bin/pdf-translate",
          "/usr/bin/pdf-translate",
          PathUtils.join(home, ".local", "bin", "pdf-translate"),
          PathUtils.join(home, ".cargo", "bin", "pdf-translate"),
        ];
    for (let path of candidates) {
      if (await IOUtils.exists(path)) {
        return path;
      }
    }
    throw new UserError(this.t("error-binary-not-found"));
  },

  async checkEnvironment() {
    let path = await this.findBinary();
    let { exitCode, log } = await this.run(path, ["--version"]);
    if (exitCode !== 0) {
      throw new UserError(this.t("error-version", { path }));
    }
    return { path, version: log.find((line) => line.trim()) || "" };
  },

  async showEnvironment() {
    try {
      let info = await this.checkEnvironment();
      Zotero.alert(null, PLUGIN_NAME, this.t("env-ok", { path: info.path, version: info.version }));
    }
    catch (error) {
      Zotero.alert(null, PLUGIN_NAME, this.t("env-missing", { detail: String(error?.message || error) }));
    }
  },

  /**
   * Run a command, merge stderr into stdout and pass every complete line to
   * onLine. Returns the exit code and the tail of the log.
   */
  async run(command, args, onLine) {
    await IOUtils.makeDirectory(this.dataDir, { ignoreExisting: true });
    let proc;
    try {
      proc = await Subprocess.call({
        command,
        arguments: args,
        workdir: this.dataDir,
        stderr: "stdout",
        environment: {
          NO_COLOR: "1",
          CLICOLOR: "0",
          RUST_LOG: "info",
        },
        environmentAppend: true,
      });
    }
    catch (error) {
      throw new UserError(this.t("error-spawn", { detail: String(error?.message || error) }));
    }

    let log = [];
    let handleLine = (line) => {
      line = line.replace(/\r$/, "");
      if (!line.trim()) {
        return;
      }
      onLine?.(line);
      log.push(line);
      if (log.length > LOG_LINES_KEPT) {
        log.shift();
      }
    };
    let buffer = "";
    let chunk;
    while ((chunk = await proc.stdout.readString())) {
      buffer += chunk;
      let parts = buffer.split(/\r\n|\r|\n/);
      buffer = parts.pop();
      parts.forEach(handleLine);
    }
    if (buffer) {
      handleLine(buffer);
    }
    let { exitCode } = await proc.wait();
    return { exitCode, log };
  },
};
