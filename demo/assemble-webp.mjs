#!/usr/bin/env node
// frames/frame-text-*.png + frames/frame-cursor-*.png  ->  docs/demo.webp
//
// vhs's PNG-sequence output writes two parallel streams: frame-text-* is
// the terminal content (no cursor), frame-cursor-* is a transparent
// overlay carrying only the cursor blink. We composite them with
// ffmpeg's overlay filter, then hand the composed PNGs to img2webp.
//
// Lossless is cheap here because byte-identical frames coalesce — vhs
// writes the same bytes for back-to-back unchanged frames (no codec
// quantization noise), so long Sleep stretches collapse to a single
// stored frame plus skip markers.
//
//   node demo/assemble-webp.mjs [frames/] [docs/demo.webp]
import { mkdirSync, readdirSync, rmSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";

const inDir = process.argv[2] ?? "frames";
const out = process.argv[3] ?? "docs/demo.webp";
const work = join(tmpdir(), `jview-webp-${process.pid}`);
mkdirSync(work, { recursive: true });

// Pair frames by sequence index via ffmpeg's two-input glob: input 0 is the
// terminal content, input 1 the transparent cursor overlay; overlay puts 1
// on top of 0. `-frame_pts 0` keeps the output PNG numbering 1..N regardless
// of source file names.
const ff = spawnSync(
  "ffmpeg",
  ["-y",
   "-pattern_type", "glob", "-i", `${inDir}/frame-text-*.png`,
   "-pattern_type", "glob", "-i", `${inDir}/frame-cursor-*.png`,
   "-filter_complex", "[0][1]overlay",
   join(work, "composed-%05d.png")],
  { stdio: "inherit" },
);
if (ff.status !== 0) process.exit(ff.status ?? 1);

// Tape Framerate is 24 → 1000/24 ≈ 42 ms per frame. Keep in sync with the
// `Set Framerate` line in demo/demo.tape.
const PER_FRAME_MS = 42;
const composed = readdirSync(work).filter((f) => f.endsWith(".png")).sort();
const args = ["-loop", "0", "-d", String(PER_FRAME_MS), "-lossless", "-m", "6"];
for (const f of composed) args.push(join(work, f));
args.push("-o", out);

const enc = spawnSync("img2webp", args, { stdio: "inherit" });
rmSync(work, { recursive: true, force: true });
if (enc.status !== 0) process.exit(enc.status ?? 1);
console.error(`wrote ${out}: ${composed.length} frames`);
