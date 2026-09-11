#!/usr/bin/env node
// 变形 K 图标生成器（issue #4；2026-09-11 重设计：与标题栏徽标同源的蓝底白 K）。
//
// 设计目标：任务栏图标与应用内标题栏徽标（src-ui/index.html `.window-app-mark`，
// issue #132）视觉一致——蓝色纵向渐变圆角方块 + 白色描边 K + 右上角四角星芒；
// 全部几何 1:1 归一化自徽标 SVG 的 40×40 viewBox，小尺寸（<32px）帧省略星芒并
// 加粗描边保证 16px 任务栏可读；图形为自绘几何，不复制官方 Kimi 商标路径。
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
// 配色：与标题栏徽标同源（src-ui/styles.css 的 .window-app-mark 系列变量）——
// 蓝色纵向渐变底板 + 纯白 K 与星芒。
const COL_BLUE_TOP = [0x60, 0xa5, 0xfa]; // 渐变顶部（--blue-fill）
const COL_BLUE_BOT = [0x3b, 0x82, 0xf6]; // 渐变底部（--monitor-dot）
const COL_WHITE = [0xff, 0xff, 0xff]; // K 描边与星芒（--text-inverse）

// ---------------------------------------------------------------------------
// 几何参数（归一化自徽标 SVG 的 40×40 viewBox，坐标 = SVG 值 / 40）：
//   底板：满幅圆角矩形（rx=12 -> 半径 0.3）；
//   K 形：徽标描边折线 M13 11 V29 M25.5 11 L13.9 20.6 L25.5 29，拆成三条圆头
//         线段（胶囊）取并集，圆头端点天然等价于 round linecap/linejoin；
//   星芒：右上四角星为四条二次贝塞尔围成的填充形，展平为多边形做填充判定。
const GEO = {
  plateRadius: 0.3, // 12 / 40

  stemAX: 0.325, // 竖干 (13,11) -> (13,29)
  stemAY: 0.275,
  stemBX: 0.325,
  stemBY: 0.725,
  joinX: 0.3475, // 上下臂公共拐点 (13.9,20.6)
  joinY: 0.515,
  armUpX: 0.6375, // 上臂另一端 (25.5,11)
  armUpY: 0.275,
  armDnX: 0.6375, // 下臂另一端 (25.5,29)
  armDnY: 0.725,

  strokeHalfBig: 0.0425, // 徽标 stroke-width 3.4 的半宽（3.4/2/40）
  strokeHalfSm: 0.0625, // <32px 帧加粗到 2.5 等效宽度，保证 16px 可辨认
};

// 星芒轮廓：四条二次贝塞尔（[起点, 控制点, 终点]），归一化坐标。
const SPARK_CURVES = [
  [[0.78, 0.1425], [0.7995, 0.2205], [0.8775, 0.24]],
  [[0.8775, 0.24], [0.7995, 0.2595], [0.78, 0.3375]],
  [[0.78, 0.3375], [0.7605, 0.2595], [0.6825, 0.24]],
  [[0.6825, 0.24], [0.7605, 0.2205], [0.78, 0.1425]],
];
const SPARK_FLATTEN = 8; // 每条贝塞尔细分的直线段数（超采样下足够平滑）

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

// K 形描边距离（< 0 表示在描边内）；strokeHalf 为归一化描边半宽。
function glyphDist(nx, ny, strokeHalf) {
  const g = GEO;
  const stem = sdfSegment(nx, ny, g.stemAX, g.stemAY, g.stemBX, g.stemBY, strokeHalf);
  const armUp = sdfSegment(nx, ny, g.joinX, g.joinY, g.armUpX, g.armUpY, strokeHalf);
  const armDn = sdfSegment(nx, ny, g.joinX, g.joinY, g.armDnX, g.armDnY, strokeHalf);
  return Math.min(stem, armUp, armDn);
}

// 星芒多边形：四条二次贝塞尔各自展平成 SPARK_FLATTEN 段直线。
const SPARK_POLY = (() => {
  const pts = [];
  for (const [[x0, y0], [cx, cy], [x1, y1]] of SPARK_CURVES) {
    for (let i = 0; i < SPARK_FLATTEN; i++) {
      const t = i / SPARK_FLATTEN;
      const u = 1 - t;
      pts.push([u * u * x0 + 2 * u * t * cx + t * t * x1, u * u * y0 + 2 * u * t * cy + t * t * y1]);
    }
  }
  return pts;
})();

// 偶奇规则点在多边形内判定（星芒填充用）。
function pointInPolygon(px, py, poly) {
  let inside = false;
  for (let i = 0, j = poly.length - 1; i < poly.length; j = i++) {
    const [xi, yi] = poly[i];
    const [xj, yj] = poly[j];
    if (yi > py !== yj > py && px < ((xj - xi) * (py - yi)) / (yj - yi) + xi) inside = !inside;
  }
  return inside;
}

// ---------------------------------------------------------------------------
// 逐采样点颜色：蓝色纵向渐变底板 + 白色描边 K + 星芒分层叠加，返回 RGBA。
const mix = (a, b, t) => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];

function sampleColor(nx, ny, size) {
  const g = GEO;
  const smallMode = size < 32; // 小尺寸帧省略星芒、加粗描边
  const dBg = sdfRoundRect(nx, ny, 0.5, 0.5, 0.5, 0.5, g.plateRadius);
  if (dBg > 0) return [0, 0, 0, 0];

  // 底板：自上而下的蓝色纵向渐变（与徽标 linearGradient x1=0,y1=0 -> x2=0,y2=1 同向）。
  let col = mix(COL_BLUE_TOP, COL_BLUE_BOT, ny);

  // 白色层：K 描边 + 星芒（小尺寸帧只留加粗 K）。
  if (glyphDist(nx, ny, smallMode ? g.strokeHalfSm : g.strokeHalfBig) <= 0) {
    col = COL_WHITE;
  } else if (!smallMode && pointInPolygon(nx, ny, SPARK_POLY)) {
    col = COL_WHITE;
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
