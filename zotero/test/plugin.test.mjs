import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const utilSource = readFileSync(new URL("../content/util.js", import.meta.url), "utf8");
const pluginSource = readFileSync(new URL("../content/pdf-translate.js", import.meta.url), "utf8");

const PREFS = {
  "extensions.pdf-translate.binaryPath": "",
  "extensions.pdf-translate.configPath": "",
  "extensions.pdf-translate.targetLang": "zh-CN",
  "extensions.pdf-translate.pages": "1-",
  "extensions.pdf-translate.mode": "clean",
  "extensions.pdf-translate.bilingual": true,
  "extensions.pdf-translate.translateReferences": false,
  "extensions.pdf-translate.keepAnnotations": false,
  "extensions.pdf-translate.fake": false,
  "extensions.pdf-translate.extraArgs": "",
  "extensions.pdf-translate.importAttachment": true,
  "extensions.pdf-translate.openWhenDone": true,
};

function harness({ existingBilingual = false } = {}) {
  const prefs = { ...PREFS };
  const calls = {
    runs: [],
    imported: [],
    opened: [],
    alerts: [],
    menus: [],
    readerListeners: [],
    pane: null,
  };

  const attachment = {
    id: 1,
    key: "SOURCEKEY",
    libraryID: 1,
    parentItemID: 2,
    attachmentFilename: "paper.pdf",
    attachmentLinkMode: 0,
    isRegularItem: () => false,
    isPDFAttachment: () => true,
    getFilePathAsync: async () => "/tmp/paper.pdf",
    getField: (field) => (field === "title" ? "A Paper" : ""),
    getNote: () => "",
    getCollections: () => [7],
  };
  const existing = {
    id: 5,
    key: "EXISTING",
    libraryID: 1,
    parentItemID: 2,
    deleted: false,
    isPDFAttachment: () => true,
    getFilePathAsync: async () => "/tmp/zotero-data/pdf-translate/outputs/OLD.bilingual.pdf",
    getField: (field) => (field === "title" ? "Bilingual (zh-CN)" : ""),
    getNote: () => "pdf-translate:SOURCEKEY:zh-CN:clean:bilingual",
  };
  const parent = {
    id: 2,
    isRegularItem: () => true,
    getAttachments: () => (existingBilingual ? [1, 5] : [1]),
    getField: () => "A Paper",
  };
  const imported = {
    id: 9,
    key: "IMPORTED",
    getField: (field) => (field === "title" ? "Translation (zh-CN)" : ""),
    setNote(note) {
      calls.importedNote = note;
    },
    async saveTx() {},
  };

  const Zotero = {
    isWin: false,
    DataDirectory: { dir: "/tmp/zotero-data" },
    debug() {},
    logError(error) {
      console.error(error);
    },
    alert(window, title, text) {
      calls.alerts.push({ title, text });
    },
    Prefs: {
      get: (name) => prefs[name],
      set: (name, value) => {
        prefs[name] = value;
      },
    },
    Items: {
      get: (value) => {
        if (Array.isArray(value)) {
          return value.map((id) => (id === 1 ? attachment : id === 5 ? existing : parent));
        }
        return value === 1 ? attachment : value === 5 ? existing : parent;
      },
    },
    Attachments: {
      LINK_MODE_LINKED_URL: 3,
      async importFromFile(options) {
        calls.imported.push(options);
        return imported;
      },
    },
    Reader: {
      registerEventListener(type, handler) {
        calls.readerListeners.push({ type, handler });
      },
      unregisterEventListener() {},
      async open(id) {
        calls.opened.push(id);
      },
    },
    MenuManager: {
      registerMenu(options) {
        calls.menus.push(options);
        return options.menuID;
      },
      unregisterMenu() {},
    },
    PreferencePanes: {
      async register(options) {
        calls.pane = options;
        return "pdf-translate-pane";
      },
      unregister() {},
    },
    getMainWindows: () => [],
    File: {
      truncateFileName: (name) => name,
      reveal(path) {
        calls.revealed = path;
      },
    },
    async launchFile(path) {
      calls.launched = path;
    },
    ProgressWindow: class {
      changeHeadline() {}
      show() {}
      startCloseTimer() {}
      close() {}
    },
  };
  Zotero.ProgressWindow.prototype.ItemProgress = class {
    setProgress() {}
    setText() {}
    setError() {}
  };

  const Subprocess = {
    async pathSearch() {
      return "/usr/bin/pdf-translate";
    },
    async call(options) {
      calls.runs.push(options);
      const lines = [
        "layout: 2 pages, 10 lines, 3 paragraphs, 0 protected",
        " INFO translated 3/3",
        "translation: 3 candidates, 3 translated (0 cached), 0 failed",
        "font: <embedded> (index 0)",
        "typeset: 3 paragraphs, 0 overflow",
        "wrote /tmp/zotero-data/pdf-translate/outputs/SOURCEKEY.zh-CN.pdf",
      ];
      let index = 0;
      return {
        stdout: {
          async readString() {
            return index < lines.length ? `${lines[index++]}\n` : null;
          },
        },
        async wait() {
          return { exitCode: 0 };
        },
      };
    },
  };

  const IOUtils = {
    async makeDirectory() {},
    async remove() {},
    async exists() {
      return true;
    },
    async stat() {
      return { lastModified: 1000, size: 1024 };
    },
    async readUTF8() {
      return "{}";
    },
    async writeUTF8(path, text) {
      calls.written = { path, text };
    },
  };
  const PathUtils = {
    join: (...parts) => parts.join("/"),
    filename: (path) => path.split("/").pop(),
  };
  const Services = {
    dirsvc: {
      get: () => ({ path: "/home/test" }),
    },
  };

  const sandbox = {
    console,
    ChromeUtils: {
      importESModule: () => ({ Subprocess }),
    },
    Zotero,
    Services,
    IOUtils,
    PathUtils,
    Localization: class {
      formatValueSync(id, args) {
        return args ? `${id} ${JSON.stringify(args)}` : id;
      }
    },
    Ci: {},
    setTimeout,
    clearTimeout,
  };
  vm.createContext(sandbox);
  vm.runInContext(utilSource, sandbox);
  vm.runInContext(pluginSource, sandbox);

  return { sandbox, prefs, calls, attachment, imported };
}

test("plugin startup registers the reader, menus and preferences pane", async () => {
  const { sandbox, calls } = harness();
  const plugin = sandbox.PdfTranslate;
  plugin.init({ id: "pdf-translate@crl.local", version: "0.1.0", rootURI: "file:///plugin/" });
  await plugin.startup();

  assert.equal(calls.readerListeners.length, 1);
  assert.equal(calls.readerListeners[0].type, "renderToolbar");
  assert.deepEqual(calls.menus.map((menu) => menu.target), ["main/library/item", "main/menubar/tools"]);
  assert.equal(calls.pane.pluginID, "pdf-translate@crl.local");
  assert.ok(calls.pane.src.endsWith("content/preferences.xhtml"));
  assert.equal(plugin.settings.targetLang, "zh-CN");
  plugin.shutdown();
});

test("a clean run builds the CLI arguments and imports the output", async () => {
  const { sandbox, calls, attachment, imported } = harness();
  const plugin = sandbox.PdfTranslate;
  plugin.init({ id: "pdf-translate@crl.local", version: "0.1.0", rootURI: "file:///plugin/" });
  await plugin.startup();

  await plugin.translateAttachment(attachment);

  assert.equal(calls.runs.length, 1);
  const run = calls.runs[0];
  assert.equal(run.command, "/usr/bin/pdf-translate");
  assert.deepEqual([...run.arguments], [
    "translate", "/tmp/paper.pdf", "-o",
    "/tmp/zotero-data/pdf-translate/outputs/SOURCEKEY.zh-CN.bilingual.pdf",
    "--target-lang", "zh-CN",
    "--bilingual",
  ]);
  assert.equal(run.stderr, "stdout");
  assert.equal(run.environment.RUST_LOG, "info");

  assert.equal(calls.imported.length, 1);
  assert.equal(calls.imported[0].parentItemID, 2);
  assert.equal(calls.imported[0].fileBaseName, "paper.zh-CN.bilingual");
  assert.equal(calls.importedNote, "pdf-translate:SOURCEKEY:zh-CN:clean:bilingual");
  assert.deepEqual(calls.opened, [imported.id]);
  assert.equal(calls.alerts.length, 0);

  plugin.shutdown();
});

test("an existing bilingual attachment is opened without translating", async () => {
  const { sandbox, calls, attachment } = harness({ existingBilingual: true });
  const plugin = sandbox.PdfTranslate;
  plugin.init({ id: "pdf-translate@crl.local", version: "0.3.0", rootURI: "file:///plugin/" });
  await plugin.startup();

  await plugin.translateAttachment(attachment);

  assert.equal(calls.runs.length, 0);
  assert.deepEqual(calls.opened, [5]);
  assert.equal(calls.imported.length, 0);
  plugin.shutdown();
});

test("without importing, opens the cached PDF externally", async () => {
  const { sandbox, calls, attachment } = harness();
  const plugin = sandbox.PdfTranslate;
  plugin.init({ id: "pdf-translate@crl.local", version: "0.3.0", rootURI: "file:///plugin/" });
  await plugin.startup();
  plugin.setPref("importAttachment", false);

  await plugin.translateAttachment(attachment);

  assert.equal(calls.imported.length, 0);
  assert.equal(
    calls.launched,
    "/tmp/zotero-data/pdf-translate/outputs/SOURCEKEY.zh-CN.bilingual.pdf",
  );
  plugin.shutdown();
});

test("plugin output attachments are not offered for translation again", async () => {
  const { sandbox, attachment } = harness();
  const plugin = sandbox.PdfTranslate;
  plugin.init({ id: "pdf-translate@crl.local", version: "0.1.0", rootURI: "file:///plugin/" });
  await plugin.startup();

  attachment.getNote = () => "pdf-translate:SOURCEKEY:zh-CN:clean";
  assert.equal(plugin.getPDFAttachments([attachment]).length, 0);
  plugin.shutdown();
});
