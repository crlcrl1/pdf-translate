import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const source = readFileSync(new URL("../content/util.js", import.meta.url), "utf8");
const context = vm.createContext({});
vm.runInContext(source, context);
const util = context.PdfTranslateUtil;

// Values from the vm realm have different prototypes, so compare plain copies.
const plain = (value) => JSON.parse(JSON.stringify(value));

test("parseExtraArgs handles whitespace, quotes and escapes", () => {
  assert.deepEqual(plain(util.parseExtraArgs("")), []);
  assert.deepEqual(plain(util.parseExtraArgs("  --limit 5  ")), ["--limit", "5"]);
  assert.deepEqual(plain(util.parseExtraArgs('--font "/tmp/a b.ttf"')), ["--font", "/tmp/a b.ttf"]);
  assert.deepEqual(plain(util.parseExtraArgs("--model 'my model'")), ["--model", "my model"]);
  assert.deepEqual(plain(util.parseExtraArgs("a\\ b")), ["a b"]);
  assert.deepEqual(plain(util.parseExtraArgs('"a\\"b"')), ['a"b']);
  assert.deepEqual(plain(util.parseExtraArgs('C:\\Users\\me "D:\\a b"')), ["C:\\Users\\me", "D:\\a b"]);
  assert.throws(() => util.parseExtraArgs("'unterminated"), /unterminated quote/);
});

test("buildArgs produces minimal and full argument lists", () => {
  assert.deepEqual(
    plain(util.buildArgs({
      input: "/tmp/in.pdf",
      output: "/tmp/out.pdf",
      targetLang: "zh-CN",
      pages: "1-",
    })),
    ["translate", "/tmp/in.pdf", "-o", "/tmp/out.pdf", "--target-lang", "zh-CN"],
  );

  assert.deepEqual(
    plain(util.buildArgs({
      input: "in.pdf",
      output: "out.pdf",
      configPath: "/tmp/config.toml",
      targetLang: "ja",
      pages: "2-5,8",
      mode: "overlay",
      bilingual: true,
      translateReferences: true,
      keepAnnotations: true,
      fake: true,
      extraArgs: "--limit 3",
    })),
    [
      "translate", "in.pdf", "-o", "out.pdf",
      "--config", "/tmp/config.toml",
      "--target-lang", "ja",
      "--pages", "2-5,8",
      "--overlay",
      "--bilingual",
      "--translate-references",
      "--keep-annotations",
      "--fake",
      "--limit", "3",
    ],
  );
});

test("output names are stable and safe", () => {
  assert.equal(util.outputFileName("ABCD1234", "zh-CN", "clean"), "ABCD1234.zh-CN.pdf");
  assert.equal(util.outputFileName("ABCD1234", "zh-CN", "overlay"), "ABCD1234.zh-CN.overlay.pdf");
  assert.equal(util.outputFileName("ABCD1234", "zh-CN", "clean", true), "ABCD1234.zh-CN.bilingual.pdf");
  assert.equal(util.outputFileName("ABCD1234", "zh-CN", "overlay", true), "ABCD1234.zh-CN.overlay.bilingual.pdf");
  assert.equal(util.outputFileName("../x", "zh/CN", "clean"), "___x.zh_CN.pdf");
});

test("parseProgressLine maps CLI stages", () => {
  assert.equal(util.parseProgressLine(""), null);
  assert.deepEqual(plain(util.parseProgressLine("\u001b[32mlayout: 12 pages, 300 lines, 42 paragraphs, 3 protected\u001b[0m")), {
    stage: "layout",
    percent: 3,
    detail: "12 pages, 42 paragraphs",
  });
  assert.deepEqual(plain(util.parseProgressLine("translated 10/40")), {
    stage: "translate",
    percent: 25,
    detail: "10/40",
    done: 10,
    total: 40,
  });
  assert.deepEqual(plain(util.parseProgressLine(" INFO translated 4/40")), {
    stage: "translate",
    percent: 13,
    detail: "4/40",
    done: 4,
    total: 40,
  });
  assert.equal(util.parseProgressLine("translated 1/0").percent, 5);
  assert.equal(util.parseProgressLine("translation: 40 candidates").stage, "translation");
  assert.equal(util.parseProgressLine("font: /tmp/font.ttf").stage, "typeset");
  assert.equal(util.parseProgressLine("typeset: 40 paragraphs").percent, 95);
  assert.equal(util.parseProgressLine("wrote /tmp/out.pdf").stage, "write");
  assert.equal(util.parseProgressLine("error: boom").stage, "error");
  assert.equal(util.parseProgressLine("some tracing line").stage, "log");
});

test("cache descriptors only match the same source and settings", () => {
  const base = {
    source: "/tmp/a.pdf",
    sourceMtime: 100,
    sourceSize: 200,
    binary: "/tmp/pdf-translate",
    binaryMtime: 300,
    pluginVersion: "0.3.0",
    configPath: "",
    targetLang: "zh-CN",
    pages: "1-",
    mode: "clean",
    bilingual: false,
    translateReferences: false,
    keepAnnotations: false,
    fake: false,
    extraArgs: "",
  };
  const current = plain(util.cacheState(base));
  assert.equal(util.cacheMatches(current, current), true);
  assert.equal(util.cacheMatches({ ...current, sourceMtime: 101 }, current), false);
  assert.equal(util.cacheMatches({ ...current, settings: { ...current.settings, targetLang: "ja" } }, current), false);
  assert.equal(util.cacheMatches({ ...current, attachmentID: 9 }, current), true);
});

test("lastError prefers the explicit error line", () => {
  assert.equal(util.lastError(["warning: x", "error: bad", "wrote nothing"]), "error: bad");
  assert.equal(util.lastError(["warning: x", "last line"]), "last line");
  assert.equal(util.lastError([]), "");
  assert.equal(util.lastError(["   ", ""]), "");
});

test("truncate keeps status lines short", () => {
  assert.equal(util.truncate("abc", 5), "abc");
  assert.equal(util.truncate("abcdef", 5), "abcd…");
});
