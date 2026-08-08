// Style-guide enforcement (CLAUDE.md invariant 8): components may not
// declare color literals — every color comes from the tokens in
// theme.css. This test IS the enforcement; if it hurts, change the tokens
// and docs/style-guide.md in the same PR.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { describe, expect, it } from "vitest";

const SRC = join(__dirname, "..");
const ALLOWED = new Set(["theme.css", "style-lint.test.ts"]);
const COLOR_LITERAL =
  /#[0-9a-fA-F]{3,8}\b|rgba?\(|hsla?\(|color-mix\(|\b(?:white|black|red|blue|green|orange|purple|yellow)\b(?=\s*[;,)])/;

function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return walk(path);
    return /\.(tsx?|css)$/.test(name) ? [path] : [];
  });
}

describe("style guide enforcement", () => {
  it("no color literals outside theme.css", () => {
    const offenders: string[] = [];
    for (const file of walk(SRC)) {
      const base = file.split("/").pop()!;
      if (ALLOWED.has(base)) continue;
      const lines = readFileSync(file, "utf8").split("\n");
      lines.forEach((line, i) => {
        if (COLOR_LITERAL.test(line)) {
          offenders.push(`${relative(SRC, file)}:${i + 1}: ${line.trim()}`);
        }
      });
    }
    expect(offenders, offenders.join("\n")).toEqual([]);
  });
});
