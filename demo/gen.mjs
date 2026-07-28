#!/usr/bin/env node
// Generate a large, realistically-nested JSON file for the jsonview demo GIF.
// Deterministic (fixed PRNG seed) so the recording is reproducible.
//
//   node demo/gen.mjs demo/big.json 1      # ~1 GB  (the "opens instantly" shot)
//   node demo/gen.mjs demo/big.json 0.02   # ~20 MB (quick local check)
import { createWriteStream } from "node:fs";

const out = process.argv[2] ?? "demo/big.json";
const targetBytes = Math.round(parseFloat(process.argv[3] ?? "1") * 1024 ** 3);

// mulberry32 — tiny deterministic PRNG, no deps.
let s = 0x9e3779b9;
const rnd = () => {
  s = (s + 0x6d2b79f5) | 0;
  let t = Math.imul(s ^ (s >>> 15), 1 | s);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const pick = (a) => a[Math.floor(rnd() * a.length)];
const int = (n) => Math.floor(rnd() * n);

const FIRST = ["Ada", "Alan", "Grace", "Linus", "Margaret", "Ken", "Barbara", "Dennis", "Radia", "Edsger"];
const LAST = ["Lovelace", "Turing", "Hopper", "Torvalds", "Hamilton", "Thompson", "Liskov", "Ritchie", "Perlman", "Dijkstra"];
const CITY = ["Berlin", "Tokyo", "Nairobi", "Lima", "Oslo", "Hanoi", "Cairo", "Quito", "Perth", "Riga"];
const TAGS = ["rust", "mmap", "json", "tui", "cli", "perf", "lazy", "stream", "ratatui", "unicode"];
const WORDS = ["fast", "native", "zero", "copy", "byte", "range", "scan", "lazy", "viewer", "terminal", "huge", "instant"];

const sentence = () => Array.from({ length: 6 + int(8) }, () => pick(WORDS)).join(" ");

const record = (id) => ({
  id,
  login: pick(WORDS) + int(100000),
  name: `${pick(FIRST)} ${pick(LAST)}`,
  email: `${pick(WORDS)}${id}@example.com`,
  active: rnd() < 0.7,
  address: {
    street: `${int(9999)} ${pick(WORDS)} St`,
    city: pick(CITY),
    zip: String(10000 + int(89999)),
    geo: { lat: +(rnd() * 180 - 90).toFixed(5), lng: +(rnd() * 360 - 180).toFixed(5) },
  },
  tags: Array.from({ length: 1 + int(4) }, () => pick(TAGS)),
  stats: { followers: int(50000), repos: int(500), score: +(rnd() * 100).toFixed(3) },
  bio: sentence(),
});

const ws = createWriteStream(out);
let bytes = 0;
const write = (str) =>
  new Promise((res) => {
    bytes += Buffer.byteLength(str);
    ws.write(str) ? res() : ws.once("drain", res);
  });

let n = 0;
await write('{"users":[');
while (bytes < targetBytes) {
  let chunk = "";
  for (let i = 0; i < 500; i++) chunk += (n === 0 ? "" : ",") + JSON.stringify(record(n++));
  await write(chunk);
}
await write(`],"meta":{"count":${n},"generated_at":"2026-06-15T00:00:00Z"}}\n`);
await new Promise((r) => ws.end(r));
console.error(`wrote ${out}: ${n} users, ${(bytes / 1024 ** 3).toFixed(2)} GiB`);
