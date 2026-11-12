import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";

const manifest = JSON.parse(readFileSync(new URL("../manifest.json", import.meta.url), "utf8"));
const root = new URL("..", import.meta.url);

test("plugin version matches the Rust binary version", () => {
  const cargo = readFileSync(new URL("../../Cargo.toml", import.meta.url), "utf8");
  const packageSection = cargo.split(/^\[/m).find((section) => section.startsWith("package]"));
  const version = packageSection?.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  assert.ok(version, "Cargo.toml must declare a package version");
  assert.equal(manifest.version, version);
});

test("manifest has the fields Zotero 10 requires to install a plugin", () => {
  assert.equal(manifest.manifest_version, 2);
  assert.ok(manifest.name);
  assert.match(manifest.version, /^\d+(\.\d+){1,3}$/);
  assert.equal(manifest.applications.zotero.id, "pdf-translate@crl.local");
  // Zotero's ExtensionData rejects a plugin with no update_url as a parse
  // error, even when the plugin is installed from a local file.
  assert.match(manifest.applications.zotero.update_url, /^https:\/\//);
  assert.ok(manifest.applications.zotero.strict_min_version);
  assert.ok(manifest.applications.zotero.strict_max_version);
  assert.equal(manifest.applications.zotero.strict_min_version.includes("*"), false);
});

test("bootstrap and preference resources exist", () => {
  for (const file of ["bootstrap.js", "prefs.js", "content/preferences.xhtml", "content/preferences.js"]) {
    assert.ok(existsSync(new URL(file, root)), `${file} is missing`);
  }
});
