// Generates the extension's PNG icons from pure pixel math.
//
// No image library and no design tool: the mark is a rounded square in the
// OpenApps download-blue with a white download arrow, which is simple enough to
// rasterise directly. Node's zlib supplies the only compression needed, so this
// adds no dependency to package.json.
import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const outDir = join(dirname(dirname(fileURLToPath(import.meta.url))), "public", "icons");
mkdirSync(outDir, { recursive: true });

const BRAND = [0x15, 0xb9, 0xeb];
const WHITE = [0xff, 0xff, 0xff];

function crc32(buf) {
  let c = ~0;
  for (const byte of buf) {
    c ^= byte;
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return ~c >>> 0;
}

function chunk(type, data) {
  const typeAndData = Buffer.concat([Buffer.from(type, "latin1"), data]);
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(typeAndData));
  return Buffer.concat([len, typeAndData, crc]);
}

function encodePng(size, pixels) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // colour type: RGBA
  // Each scanline is prefixed with its filter type; 0 means "none", which keeps
  // this encoder trivial at a negligible size cost for icons this small.
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    pixels.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

/** Coverage of a pixel by the rounded square, used for cheap antialiasing. */
function squareCoverage(x, y, size, radius) {
  const inset = 0.5;
  const min = inset;
  const max = size - inset;
  const cx = Math.min(Math.max(x + 0.5, min + radius), max - radius);
  const cy = Math.min(Math.max(y + 0.5, min + radius), max - radius);
  const dx = x + 0.5 - cx;
  const dy = y + 0.5 - cy;
  const dist = Math.hypot(dx, dy);
  if (dist <= radius - 0.5) return 1;
  if (dist >= radius + 0.5) return 0;
  return radius + 0.5 - dist;
}

/**
 * The download glyph: a vertical stem, a chevron head, and a base line.
 *
 * At 16px a 2px-stroke glyph all but disappears, so the proportions scale with
 * the canvas rather than being drawn once and downsampled.
 */
function glyphAlpha(x, y, size) {
  const u = (v) => v * size;
  const px = x + 0.5;
  const py = y + 0.5;

  const stemHalf = Math.max(size * 0.055, 0.9);
  const inStem = Math.abs(px - u(0.5)) <= stemHalf && py >= u(0.24) && py <= u(0.56);

  // Chevron: two diagonals meeting at the arrow tip.
  const tipY = u(0.66);
  const armSpan = u(0.19);
  const armHalf = Math.max(size * 0.055, 0.9);
  const dxArm = Math.abs(px - u(0.5));
  const expectedY = tipY - (armSpan - dxArm);
  const inChevron =
    dxArm <= armSpan && Math.abs(py - expectedY) <= armHalf * Math.SQRT2 && py <= tipY;

  const baseHalf = Math.max(size * 0.05, 0.8);
  const inBase = Math.abs(py - u(0.79)) <= baseHalf && Math.abs(px - u(0.5)) <= u(0.23);

  return inStem || inChevron || inBase ? 1 : 0;
}

function renderIcon(size) {
  const pixels = Buffer.alloc(size * size * 4);
  const radius = size * 0.22;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const i = (y * size + x) * 4;
      const cover = squareCoverage(x, y, size, radius);
      const glyph = glyphAlpha(x, y, size);
      const rgb = glyph ? WHITE : BRAND;
      pixels[i] = rgb[0];
      pixels[i + 1] = rgb[1];
      pixels[i + 2] = rgb[2];
      pixels[i + 3] = Math.round(cover * 255);
    }
  }
  return encodePng(size, pixels);
}

for (const size of [16, 48, 128]) {
  const file = join(outDir, `icon${size}.png`);
  writeFileSync(file, renderIcon(size));
  console.log(`wrote ${file}`);
}
