// The catalogues, checked without a browser.
//
// A missing entry is invisible at runtime by design: `t()` falls back to
// the English it was written in, which is what keeps the product usable
// with a gap. These stop that fallback hiding the gap for a release or
// two.
//
// Run: node packages/ui/src/i18n/check.mjs
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const SKIP = new Set(["index.ts", "locales.ts"]);
const CATALOGUES = readdirSync(HERE).filter((f) => f.endsWith(".ts") && !SKIP.has(f));

let failures = 0;
const check = (ok, what) => {
  console.log(`${ok ? "  ok  " : "  FAIL"}  ${what}`);
  if (!ok) failures += 1;
};

/** The object's own keys. Two leading spaces, so nothing quoted inside a
 *  doc comment is mistaken for an entry. */
const entriesOf = (file) => {
  const src = readFileSync(join(HERE, file), "utf8");
  return [...src.matchAll(/^ {2}"((?:[^"\\]|\\.)*)": "((?:[^"\\]|\\.)*)",$/gm)]
    .map(([, k, v]) => [k, v]);
};

console.log("locales");
const declared = [...readFileSync(join(HERE, "locales.ts"), "utf8")
  .matchAll(/code: "([^"]+)"/g)].map((m) => m[1]);
check(declared.length === 8, `eight languages declared (${declared.length})`);
for (const code of declared) {
  if (code === "en") continue;
  check(CATALOGUES.includes(`${code}.ts`), `${code} has a catalogue behind it`);
}

console.log("coverage");
// zh-Hans is the reference: English is implicit — it *is* the key set —
// so the first fully-written catalogue is what the rest must match.
const reference = new Map(entriesOf("zh-Hans.ts"));
check(reference.size > 60, `the reference catalogue is substantial (${reference.size})`);
for (const file of CATALOGUES) {
  if (file === "zh-Hans.ts") continue;
  const theirs = new Map(entriesOf(file));
  const missing = [...reference.keys()].filter((k) => !theirs.has(k));
  const extra = [...theirs.keys()].filter((k) => !reference.has(k));
  check(missing.length === 0, `${file} covers every string${missing.length ? ` — missing ${missing.length}, e.g. ${JSON.stringify(missing[0])}` : ""}`);
  check(extra.length === 0, `${file} has nothing the others lack${extra.length ? ` — ${JSON.stringify(extra[0])}` : ""}`);
}

console.log("placeholders");
// A dropped {site} is worse than an untranslated string: the sentence
// reads as finished and silently loses the thing it was about.
for (const file of CATALOGUES) {
  for (const [k, v] of entriesOf(file)) {
    const want = [...k.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort().join(",");
    const got = [...v.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort().join(",");
    if (want || got) check(want === got, `${file}: ${JSON.stringify(k.slice(0, 40))} keeps {${want}}`);
  }
}

console.log("nothing left in English");
// Proper nouns and model names are the same word everywhere; listing
// them is better than loosening the check.
const SAME_ON_PURPOSE = new Set(["Base", "Small", "Downloads", "Language"]);
for (const file of CATALOGUES) {
  const same = entriesOf(file).filter(([k, v]) => k === v && !SAME_ON_PURPOSE.has(k)).map(([k]) => k);
  check(same.length === 0, `${file}${same.length ? ` left ${same.length} in English, e.g. ${JSON.stringify(same[0])}` : " translated every string"}`);
}

console.log(failures === 0 ? "\nall good" : `\n${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
