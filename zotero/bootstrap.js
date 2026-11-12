var PdfTranslate;
var PdfTranslateUtil;

function install() {}

async function startup({ id, version, rootURI }) {
  Services.scriptloader.loadSubScript(rootURI + "content/util.js");
  Services.scriptloader.loadSubScript(rootURI + "content/pdf-translate.js");
  PdfTranslate.init({ id, version, rootURI });
  await PdfTranslate.startup();
}

function onMainWindowLoad({ window }) {
  PdfTranslate?.addToWindow(window);
}

function onMainWindowUnload({ window }) {
  PdfTranslate?.removeFromWindow(window);
}

function shutdown() {
  PdfTranslate?.shutdown();
  PdfTranslate = undefined;
  PdfTranslateUtil = undefined;
}

function uninstall() {}
