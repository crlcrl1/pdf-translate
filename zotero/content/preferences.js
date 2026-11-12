/* global Zotero, IOUtils, window, document */

// Runs before the pane's markup is inserted.
window.MozXULElement.insertFTLIfNeeded("pdf-translate.ftl");

var PdfTranslatePrefs = {
  get plugin() {
    return Zotero.PdfTranslate;
  },

  $(id) {
    return document.getElementById(id);
  },

  init() {
    this.$("pt-detect").addEventListener("click", () => this.detect());
    this.$("pt-test").addEventListener("click", () => this.test());
    this.$("pt-open-data").addEventListener("click", () => this.openDataFolder());
    this.setStatus("", "");
  },

  setStatus(text, kind = "") {
    let elem = this.$("pt-status");
    elem.textContent = text;
    elem.classList.toggle("ok", kind === "ok");
    elem.classList.toggle("error", kind === "error");
  },

  async detect() {
    if (!this.plugin) {
      return;
    }
    this.setStatus(this.plugin.t("status-checking"), "");
    try {
      let path = await this.plugin.findBinary();
      this.plugin.setPref("binaryPath", path);
      this.$("pt-binary").value = path;
      this.setStatus(path, "ok");
    }
    catch (error) {
      this.setStatus(String(error?.message || error), "error");
    }
  },

  async test() {
    if (!this.plugin) {
      return;
    }
    this.setStatus(this.plugin.t("status-checking"), "");
    try {
      let info = await this.plugin.checkEnvironment();
      this.setStatus(this.plugin.t("status-ok", info), "ok");
    }
    catch (error) {
      this.setStatus(String(error?.message || error), "error");
    }
  },

  async openDataFolder() {
    if (!this.plugin) {
      return;
    }
    await IOUtils.makeDirectory(this.plugin.dataDir, { ignoreExisting: true });
    Zotero.File.reveal(this.plugin.dataDir);
  },
};

// The pane dispatches a non-bubbling 'load' event at its top-level element.
document.addEventListener(
  "load",
  function onPaneLoad(event) {
    if (event.target.id === "pdf-translate-pane") {
      document.removeEventListener("load", onPaneLoad, true);
      PdfTranslatePrefs.init();
    }
  },
  true,
);
