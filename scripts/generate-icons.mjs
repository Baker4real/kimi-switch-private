#!/usr/bin/env node
// 变形 K 图标生成器（issue #4；2026-09 高级感重设计：方向 D「宣纸墨印」）。
//
// 设计目标：Windows 任务栏可见的圆角底变形 K，小尺寸下仍是实心块且 K 可辨认；
// 米白纸感底（微渐变）+ 墨绿实心 K + 印刷双线框（>=64px 帧），K 缩小至 84% 留白；
// 图形为自绘几何（竖干胶囊 + 双斜臂胶囊的并集），不复制官方 Kimi 商标路径。
//
// 实现说明：零运行时依赖（仅用 Node 内置 zlib 做 DEFLATE），手写 PNG 编码器与
// 多尺寸 ICO 组装器；渲染采用超采样盒式降采样得到平滑抗锯齿边缘。
// 全流程无随机数、无时间戳，重复运行输出逐字节一致（确定性产物）。

import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const OUT_DIR = path.join(ROOT, 'src-tauri', 'icons');

// ---------------------------------------------------------------------------
// 配色：「宣纸墨印」——米白纸感底 + 墨绿实心 K，与 src-ui/styles.css 纸感色系同源。
// 高级感来自纸墨反差与印刷双线框，不依赖投影/柔光。
const COL_PAPER_TOP = [0xf8, 0xf4, 0xe9]; // 纸面亮部（左上）
const COL_PAPER_BOT = [0xe9, 0xe1, 0xcf]; // 纸面暗部（右下）
const COL_INK = [0x1f, 0x3d, 0x33]; // 墨绿（K 字形与双线框共用）

// 精饰参数：双线框只在 >=64px 的帧启用，小尺寸只留纸底 + 墨 K 保证剪影可读。
const FINISH = {
  glyphScale: 0.84, // K 形整体缩小比例（美术馆式留白）
  ringInsets: [0.03, 0.052], // 双线框各自距底板边缘的内缩量（归一化）
  ringHalfW: 0.0032, // 单根线框半宽
  ringAlpha: 0.5, // 线框不透明度
};

// ---------------------------------------------------------------------------
// 几何参数（归一化到单位画布 [0,1]×[0,1]，变形 K 的全部自绘形状）：
//   底板：圆角矩形；大尺寸留边多些显圆润，小于 64px 时近满幅铺满保证“实心底”观感。
//   K 形：左竖干为圆角竖条；右上臂 / 右下臂是两条端点圆头线段（胶囊），
//         与竖干右缘中点相接形成连续剪影——这是本图标“变形”风格化的来源。
const GEO = {
  padBig: 0.05,
  radBig: 0.205,
  padSm: 0.008,
  radSm: 0.155,

  stemCx: 0.372,
  stemHalfW: 0.0825,
  stemCy: 0.5,
  stemHalfH: 0.29,
  stemCorner: 0.056,

  joinX: 0.435,
  joinY: 0.5,
  armUpX: 0.725,
  armUpY: 0.255,
  armDnX: 0.725,
  armDnY: 0.745,
  armHalfW: 0.0815,
};

// 圆角矩形 SDF（返回值 < 0 表示点在形状内部）。
function sdfRoundRect(px, py, cx, cy, hw, hh, r) {
  const qx = Math.abs(px - cx) - (hw - r);
  const qy = Math.abs(py - cy) - (hh - r);
  const ox = Math.max(qx, 0);
  const oy = Math.max(qy, 0);
  return Math.hypot(ox, oy) + Math.min(Math.max(qx, qy), 0) - r;
}

// 圆头线段（胶囊）SDF。
function sdfSegment(px, py, ax, ay, bx, by, r) {
  const dax = bx - ax;
  const day = by - ay;
  const l2 = dax * dax + day * day;
  let t = ((px - ax) * dax + (py - ay) * day) / l2;
  t = t < 0 ? 0 : t > 1 ? 1 : t;
  const dx = px - (ax + dax * t);
  const dy = py - (ay + day * t);
  return Math.hypot(dx, dy) - r;
}

// 字形 SDF 距离（< 0 表示在 K 内部）；s 为整体缩放（<1 缩小留白），距离值随缩放等比换算。
function glyphDist(nx, ny, s = 1) {
  const g = GEO;
  const x = (nx - 0.5) / s + 0.5;
  const y = (ny - 0.5) / s + 0.5;
  const stem = sdfRoundRect(
    x, y,
    g.stemCx, g.stemCy, g.stemHalfW, g.stemHalfH, g.stemCorner,
  );
  const armUp = sdfSegment(x, y, g.joinX, g.joinY, g.armUpX, g.armUpY, g.armHalfW);
  const armDn = sdfSegment(x, y, g.joinX, g.joinY, g.armDnX, g.armDnY, g.armHalfW);
  return Math.min(stem, armUp, armDn) * s;
}

// ---------------------------------------------------------------------------
// 逐采样点颜色：纸面微渐变 + 印刷双线框 + 墨绿 K 分层叠加，返回 RGBA。
const mix = (a, b, t) => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
const clamp01 = (v) => (v < 0 ? 0 : v > 1 ? 1 : v);

function sampleColor(nx, ny, size) {
  const g = GEO;
  const smallMode = size < 64;
  const pad = smallMode ? g.padSm : g.padBig;
  const rad = smallMode ? g.radSm : g.radBig;
  const dBg = sdfRoundRect(nx, ny, 0.5, 0.5, 0.5 - pad, 0.5 - pad, rad);
  if (dBg > 0) return [0, 0, 0, 0];

  // 纸面底板：左上亮 -> 右下暗的微弱斜向渐变。
  let col = mix(COL_PAPER_TOP, COL_PAPER_BOT, clamp01(0.25 * nx + 0.75 * ny));

  // 印刷双线框：贴近底板边缘的两根低透明度墨绿细线（书装框感）。
  if (!smallMode) {
    for (const inset of FINISH.ringInsets) {
      const ra = FINISH.ringAlpha * clamp01(1 - Math.abs(dBg + inset + FINISH.ringHalfW) / FINISH.ringHalfW);
      if (ra > 0) col = mix(col, COL_INK, ra);
    }
  }

  // K 形：实心墨绿，缩小后盖在最上层。
  if (glyphDist(nx, ny, FINISH.glyphScale) <= 0) {
    col = COL_INK;
  }
  return [col[0], col[1], col[2], 255];
}

// ---------------------------------------------------------------------------
// 渲染：以 k 倍超采样逐块累计预乘颜色，再盒式降采样回目标尺寸。
const TARGET_SAMPLES = 1600; // 每轴目标采样数上限（控制耗时与平滑度的平衡）

function render(size) {
  const k = Math.max(1, Math.min(Math.ceil(TARGET_SAMPLES / size), 256));
  const M = size * k;
  const inv = 1 / (k * k);

  const out = new Uint8Array(size * size * 4);
  for (let by = 0; by < size; by++) {
    for (let bx = 0; bx < size; bx++) {
      let sa = 0, sr = 0, sg = 0, sb = 0;
      for (let sy = 0; sy < k; sy++) {
        const ny = (by * k + sy + 0.5) / M;
        for (let sx = 0; sx < k; sx++) {
          const nx = (bx * k + sx + 0.5) / M;
          const c = sampleColor(nx, ny, size);
          // 预乘累计，避免直线平均 RGBA 在边缘产生色晕。
          sr += c[0] * c[3]; sg += c[1] * c[3]; sb += c[2] * c[3]; sa += c[3];
        }
      }
      const o = (by * size + bx) * 4;
      if (sa > 0) {
        out[o] = Math.round(sr / sa);
        out[o + 1] = Math.round(sg / sa);
        out[o + 2] = Math.round(sb / sa);
      }
      out[o + 3] = Math.round(sa * inv);
    }
  }
  return out;
}

// ---------------------------------------------------------------------------
// PNG 编码器（RGBA 8-bit，filter 0 行过滤 + zlib DEFLATE）。

let CRC_TABLE = null;
function crc32(buf) {
  if (!CRC_TABLE) {
    CRC_TABLE = new Int32Array(256);
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let i = 0; i < 8; i++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      CRC_TABLE[n] = c;
    }
  }
  let c = -1;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}

function pngChunk(type, data) {
  const out = Buffer.alloc(12 + data.length);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, 'ascii');
  data.copy(out, 8);
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length);
  return out;
}

function encodePng(w, h, pixels) {
  // 每行前置 filter 字节 0（None）。
  const raw = Buffer.alloc(h * (w * 4 + 1));
  for (let y = 0; y < h; y++) {
    raw[y * (w * 4 + 1)] = 0;
    Buffer.from(pixels.buffer, y * w * 4, w * 4).copy(raw, y * (w * 4 + 1) + 1);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // color type RGBA
  const idat = zlib.deflateSync(raw, { level: 9 });
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk('IHDR', ihdr),
    pngChunk('IDAT', idat),
    pngChunk('IEND', Buffer.alloc(0)),
  ]);
}

// ---------------------------------------------------------------------------
// ICO 组装器：ICONDIR + 目录项 + 各帧数据。
// 兼容性策略：64px 及以下用未压缩 32bpp BMP（BITMAPINFOHEADER，底行在上的 BGRA +
// 全零 AND 掩码），128/256 用内嵌 PNG，兼顾老式解析路径与文件体积。

function bmpFrameFromRgba(size, pixels) {
  const andRow = ((size + 31) >> 5) << 2;
  const xorBytes = size * size * 4;
  const andBytes = andRow * size;
  const dib = Buffer.alloc(40 + xorBytes + andBytes);
  dib.writeUInt32LE(40, 0); // BITMAPINFOHEADER
  dib.writeUInt32LE(size, 4);
  dib.writeUInt32LE(size * 2, 8); // XOR + AND 两倍高
  dib.writeUInt16LE(1, 12); // planes
  dib.writeUInt16LE(32, 14); // bpp
  dib.writeUInt32LE(xorBytes + andBytes, 20); // sizeImage
  // 像素自底向上写入 BGRA。
  let o = 40;
  for (let y = size - 1; y >= 0; y--) {
    for (let x = 0; x < size; x++) {
      const i = (y * size + x) * 4;
      dib[o++] = pixels[i + 2];
      dib[o++] = pixels[i + 1];
      dib[o++] = pixels[i];
      dib[o++] = pixels[i + 3];
    }
  }
  // AND 掩码保持全零：有逐像素 alpha 时由 alpha 决定透明度。
  return dib;
}

function buildIco(frames) {
  const count = frames.length;
  const parts = [];
  const dir = Buffer.alloc(6);
  dir.writeUInt16LE(0, 0);
  dir.writeUInt16LE(1, 2); // type: icon
  dir.writeUInt16LE(count, 4);
  parts.push(dir);

  let offset = 6 + count * 16;
  const entries = [];
  for (const f of frames) {
    const e = Buffer.alloc(16);
    const dim = f.size >= 256 ? 0 : f.size;
    e[0] = dim;
    e[1] = dim;
    e.writeUInt16LE(1, 4); // planes
    e.writeUInt16LE(32, 6); // bpp
    e.writeUInt32LE(f.data.length, 8);
    e.writeUInt32LE(offset, 12);
    entries.push(e);
    parts.push(f.data);
    offset += f.data.length;
  }
  parts.splice(1, 0, ...entries);
  return Buffer.concat(parts);
}

// ---------------------------------------------------------------------------
// 输出清单：文件名 -> 目标边长（覆盖仓库现有的全部图标文件并补齐缺口）。
const OUTPUTS = [
  ['32x32.png', 32],
  ['128x128.png', 128],
  ['128x128@2x.png', 256],
  ['256x256.png', 256],
  ['icon.png', 1024], // 主图（master）
];

const ICO_FRAMES = [
  { size: 16, bmp: true },
  { size: 24, bmp: true },
  { size: 32, bmp: true },
  { size: 48, bmp: true },
  { size: 64, bmp: true },
  { size: 128, bmp: false },
  { size: 256, bmp: false },
];

fs.mkdirSync(OUT_DIR, { recursive: true });

for (const [name, size] of OUTPUTS) {
  const buf = encodePng(size, size, render(size));
  fs.writeFileSync(path.join(OUT_DIR, name), buf);
  console.log(`written ${name.padEnd(16)} ${size}x${size} ${buf.length}B`);
}

{
  const frames = [];
  for (const f of ICO_FRAMES) {
    const pixels = render(f.size);
    frames.push({ size: f.size, data: f.bmp ? bmpFrameFromRgba(f.size, pixels) : encodePng(f.size, f.size, pixels) });
  }
  const buf = buildIco(frames);
  fs.writeFileSync(path.join(OUT_DIR, 'icon.ico'), buf);
  console.log(`written ${'icon.ico'.padEnd(16)} ${frames.map((f) => `${f.size}px`).join('/')} ${buf.length}B`);
}

// ---------------------------------------------------------------------------
// 自检：重新读取产物，校验 PNG 尺寸合法、ICO 目录含 16x16 且数据完整。
function fail(msg) {
  console.error(`VERIFY FAIL: ${msg}`);
  process.exit(1);
}

for (const [name, size] of OUTPUTS) {
  const b = fs.readFileSync(path.join(OUT_DIR, name));
  if (b.readUInt32BE(0) !== 0x89504e47) fail(`${name} 不是 PNG`);
  const w = b.readUInt32BE(16);
  const h = b.readUInt32BE(20);
  if (w !== size || h !== size) fail(`${name} 尺寸 ${w}x${h} != 期望 ${size}`);
}

{
  const b = fs.readFileSync(path.join(OUT_DIR, 'icon.ico'));
  if (b.readUInt16LE(0) !== 0 || b.readUInt16LE(2) !== 1) fail('icon.ico 头非法');
  const n = b.readUInt16LE(4);
  if (n !== ICO_FRAMES.length) fail(`icon.ico 帧数 ${n} != 期望 ${ICO_FRAMES.length}`);
  for (let i = 0; i < n; i++) {
    const o = 6 + i * 16;
    const fw = b[o] === 0 ? 256 : b[o];
    const fh = b[o + 1] === 0 ? 256 : b[o + 1];
    const len = b.readUInt32LE(o + 8);
    const off = b.readUInt32LE(o + 12);
    if (fw !== fh) fail(`icon.ico 第 ${i} 帧非正方形 ${fw}x${fh}`);
    if (off + len > b.length) fail(`icon.ico 第 ${i} 帧 (${fw}px) 数据越界`);
    if (len === 0) fail(`icon.ico 第 ${i} 帧 (${fw}px) 为空`);
  }
  const has16 = [...Array(n)].some((_, i) => (b[6 + i * 16] === 16));
  if (!has16) fail('icon.ico 缺少 16x16 帧');
  console.log(`verified icon.ico: ${n} 帧 (${ICO_FRAMES.map((f) => f.size).join(',')}), 含 16x16`);
}

console.log('VERIFY PASS: PNG 尺寸合法, icon.ico 目录项完整');
