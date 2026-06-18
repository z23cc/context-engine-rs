// Generates a 1024x1024 RGBA app icon with zero dependencies, then `tauri icon`
// expands it into the full platform set. Run via `bun run icon`.
import { writeFileSync } from "node:fs";
import { deflateSync } from "node:zlib";

const SIZE = 1024;

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

// A simple "node" mark: teal ring + light center dot on a dark slate field.
const BG = [11, 18, 32, 255];
const RING = [45, 212, 191, 255];
const DOT = [125, 211, 252, 255];
const R_OUTER = 360;
const R_INNER = 250;
const R_DOT = 120;

function colorAt(dist) {
  if (dist <= R_DOT) return DOT;
  if (dist >= R_INNER && dist <= R_OUTER) return RING;
  return BG;
}

const raw = Buffer.alloc((SIZE * 4 + 1) * SIZE);
let p = 0;
const center = SIZE / 2;
for (let y = 0; y < SIZE; y++) {
  raw[p++] = 0; // PNG filter type: none
  for (let x = 0; x < SIZE; x++) {
    const dx = x - center + 0.5;
    const dy = y - center + 0.5;
    const color = colorAt(Math.sqrt(dx * dx + dy * dy));
    raw[p] = color[0];
    raw[p + 1] = color[1];
    raw[p + 2] = color[2];
    raw[p + 3] = color[3];
    p += 4;
  }
}

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(SIZE, 0);
ihdr.writeUInt32BE(SIZE, 4);
ihdr[8] = 8; // bit depth
ihdr[9] = 6; // color type: RGBA
// compression, filter, interlace already 0

const png = Buffer.concat([
  Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
  chunk("IHDR", ihdr),
  chunk("IDAT", deflateSync(raw, { level: 9 })),
  chunk("IEND", Buffer.alloc(0)),
]);

const out = new URL("../app-icon.png", import.meta.url);
writeFileSync(out, png);
console.log(`wrote ${out.pathname} (${png.length} bytes)`);
