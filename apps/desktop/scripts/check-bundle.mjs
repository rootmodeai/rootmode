// Refuse to ship a bundle an older WebKit cannot parse.
//
// A syntax the engine does not know is not a runtime error somewhere in the
// app; it is the whole bundle thrown away before the first line runs, and
// a window that stays blank with nothing left to say why. The build target
// keeps our own syntax in bounds, but a regex feature cannot be
// downlevelled, and one arrived through a dependency once. This is the
// list of things that did, or plausibly could.
//
// Bar: Safari 15 / macOS 12 Monterey, the oldest engine the app has been
// seen on. Raise it deliberately, not by accident.

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const dir = new URL("../dist/assets", import.meta.url).pathname;

const forbidden = [
  // Regex lookbehind — Safari 16.4.
  { name: "regex lookbehind (?<= / (?<!", re: /\(\?<[=!]/g },
  // Regex `v` flag — Safari 17.
  { name: "regex v flag", re: /\/[gimsuy]*v[gimsuy]*\)|\/[gimsuy]*v[gimsuy]*[,;\s]/g },
  // Class static blocks — Safari 16.4.
  { name: "class static block", re: /\bstatic\s*\{/g },
];

let bad = 0;
for (const file of readdirSync(dir).filter((f) => f.endsWith(".js"))) {
  const text = readFileSync(join(dir, file), "utf8");
  for (const { name, re } of forbidden) {
    const hits = text.match(re);
    if (!hits) continue;
    // The v-flag pattern is loose; confirm a hit is really a regex flag
    // list and not a URL or a division.
    const real = name.startsWith("regex v")
      ? hits.filter((h) => /^\/[gimsuy]*v[gimsuy]*/.test(h) && !/\/\//.test(h))
      : hits;
    if (real.length === 0) continue;
    bad += real.length;
    const at = text.search(re);
    console.error(`check-bundle: ${file}: ${name} ×${real.length} — first at ${JSON.stringify(text.slice(Math.max(0, at - 60), at + 60))}`);
  }
}

if (bad > 0) {
  console.error("check-bundle: the bundle contains syntax older WebKit cannot parse; the window would be blank there.");
  process.exit(1);
}
console.log("check-bundle: ok");
