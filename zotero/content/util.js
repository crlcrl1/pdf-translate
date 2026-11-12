/* Pure helpers shared by the plugin and its Node tests. No Zotero globals. */
var PdfTranslateUtil = {
  /**
   * Split a command-line fragment into arguments. Supports single and double
   * quotes and backslash escapes, so paths with spaces can be passed through
   * the "extra arguments" preference.
   */
  parseExtraArgs(text) {
    let args = [];
    let current = "";
    let quote = "";
    let input = String(text || "");
    for (let i = 0; i < input.length; i++) {
      let ch = input[i];
      if (ch === "\\" && quote !== "'") {
        let next = input[i + 1];
        // Escape only whitespace and quotes, so Windows paths keep their
        // backslashes. Users can still quote paths that contain spaces.
        if (next !== undefined && (/\s/.test(next) || next === '"' || next === "'")) {
          current += next;
          i++;
          continue;
        }
        current += ch;
        continue;
      }
      if (quote) {
        if (ch === quote) {
          quote = "";
        }
        else {
          current += ch;
        }
        continue;
      }
      if (ch === '"' || ch === "'") {
        quote = ch;
        continue;
      }
      if (/\s/.test(ch)) {
        if (current) {
          args.push(current);
          current = "";
        }
        continue;
      }
      current += ch;
    }
    if (quote) {
      throw new Error("unterminated quote in extra arguments");
    }
    if (current) {
      args.push(current);
    }
    return args;
  },

  /** Build the argv passed to `pdf-translate` after the executable name. */
  buildArgs(options) {
    let args = ["translate", options.input, "-o", options.output];
    if (options.configPath) {
      args.push("--config", options.configPath);
    }
    if (options.targetLang) {
      args.push("--target-lang", options.targetLang);
    }
    if (options.pages && options.pages !== "1-") {
      args.push("--pages", options.pages);
    }
    if (options.mode === "overlay") {
      args.push("--overlay");
    }
    if (options.bilingual) {
      args.push("--bilingual");
    }
    if (options.translateReferences) {
      args.push("--translate-references");
    }
    if (options.keepAnnotations) {
      args.push("--keep-annotations");
    }
    if (options.fake) {
      args.push("--fake");
    }
    args.push(...this.parseExtraArgs(options.extraArgs));
    return args;
  },

  /** Sanitize a language code for use in a file name. */
  safeLang(lang) {
    let safe = String(lang || "zh-CN").replace(/[^A-Za-z0-9_-]/g, "_");
    return safe || "zh-CN";
  },

  /**
   * Output file name for an attachment. The attachment key keeps concurrent
   * jobs apart; the mode suffix keeps clean and overlay outputs separate.
   */
  outputFileName(key, lang, mode, bilingual) {
    let stem = String(key || "attachment").replace(/[^A-Za-z0-9_-]/g, "_");
    let suffixes = [];
    if (mode === "overlay") {
      suffixes.push("overlay");
    }
    if (bilingual) {
      suffixes.push("bilingual");
    }
    let suffix = suffixes.length ? `.${suffixes.join(".")}` : "";
    return `${stem}.${this.safeLang(lang)}${suffix}.pdf`;
  },

  /**
   * Convert one stderr line from pdf-translate into progress state.
   * Returns null for empty lines, otherwise { stage, percent, detail }.
   */
  parseProgressLine(raw) {
    let line = String(raw || "")
      .replace(/\x1b\[[0-9;]*m/g, "")
      .replace(/\r/g, "")
      .trim();
    if (!line) {
      return null;
    }

    let match = line.match(/^layout:\s+(\d+) pages?,.*?(\d+) paragraphs?/);
    if (match) {
      return { stage: "layout", percent: 3, detail: `${match[1]} pages, ${match[2]} paragraphs` };
    }

    match = line.match(/(?:^|\s)translated (\d+)\/(\d+)$/);
    if (match) {
      let done = Number(match[1]);
      let total = Number(match[2]);
      let fraction = total > 0 ? done / total : 0;
      return {
        stage: "translate",
        percent: Math.min(88, 5 + Math.round(80 * fraction)),
        detail: `${done}/${total}`,
        done,
        total,
      };
    }

    if (line.startsWith("translation:")) {
      return { stage: "translation", percent: 89, detail: line };
    }
    if (line.startsWith("font:")) {
      return { stage: "typeset", percent: 91, detail: line };
    }
    if (line.startsWith("typeset:")) {
      return { stage: "typeset", percent: 95, detail: line };
    }
    if (line.startsWith("wrote ")) {
      return { stage: "write", percent: 98, detail: line };
    }
    if (line.startsWith("error:")) {
      return { stage: "error", percent: null, detail: line };
    }
    return { stage: "log", percent: null, detail: line };
  },

  /** Build the cache descriptor for a translated PDF. */
  cacheState(fields) {
    return {
      source: fields.source,
      sourceMtime: fields.sourceMtime,
      sourceSize: fields.sourceSize,
      binary: fields.binary,
      binaryMtime: fields.binaryMtime,
      pluginVersion: fields.pluginVersion,
      settings: {
        configPath: fields.configPath || "",
        targetLang: fields.targetLang,
        pages: fields.pages,
        mode: fields.mode,
        bilingual: !!fields.bilingual,
        translateReferences: !!fields.translateReferences,
        keepAnnotations: !!fields.keepAnnotations,
        fake: !!fields.fake,
        extraArgs: fields.extraArgs || "",
      },
    };
  },

  /** True when a cached output was built from the same source and settings. */
  cacheMatches(cached, current) {
    return !!cached
      && !!current
      && cached.source === current.source
      && cached.sourceMtime === current.sourceMtime
      && cached.sourceSize === current.sourceSize
      && cached.binary === current.binary
      && cached.binaryMtime === current.binaryMtime
      && cached.pluginVersion === current.pluginVersion
      && JSON.stringify(cached.settings) === JSON.stringify(current.settings);
  },

  /** The most useful line from a failed run. */
  lastError(log) {
    let lines = (log || []).filter((line) => line.trim());
    for (let i = lines.length - 1; i >= 0; i--) {
      if (lines[i].trim().startsWith("error:")) {
        return lines[i].trim();
      }
    }
    return lines.length ? lines[lines.length - 1].trim() : "";
  },

  /** Shorten long status lines for the progress window. */
  truncate(text, max = 120) {
    let value = String(text || "");
    return value.length > max ? `${value.slice(0, max - 1)}…` : value;
  },
};
