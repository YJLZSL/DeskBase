/**
 * 应用图标构建
 * ============================================================
 * 从 app/ui/brand/icon.svg 出发，产出：
 *   app/ui/brand/icon.ico              多尺寸 Windows 图标（可执行文件/快捷方式/安装器）
 *   app/ui/brand/icon-<n>.png          各尺寸 PNG（关于页、README、托盘、Linux 便携包）
 *   app/ui/brand/icon-rgba-256.bin     256×256 原始 RGBA，给 Rust 侧做窗口/任务栏图标
 *
 * 光栅化交给应用自己（app/src/render.rs）—— 本机 Edge 无法以 headless 方式工作，
 * 又没有 ImageMagick / Python / sharp。详见 render.rs 顶部的说明。
 *
 * 用法：
 *   node tools/icon/build-icons.cjs             先构建应用，再生成图标
 *   node tools/icon/build-icons.cjs --no-build  不重新构建，直接用现有 exe
 *   node tools/icon/build-icons.cjs --check     只校验已入库的产物
 */

const fs = require('fs');
const path = require('path');
const zlib = require('zlib');
const os = require('os');
const { spawnSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..', '..');
const BRAND = path.join(ROOT, 'app', 'ui', 'brand');
const SVG_SRC = path.join(BRAND, 'icon.svg');
const SVG_SMALL_SRC = path.join(BRAND, 'icon-small.svg');
const EXE = path.join(ROOT, 'app', 'target', 'release', 'deskbase.exe');
const NODE = process.execPath;

/** ICO 里要装的尺寸。≤64 用 BMP/DIB，128/256 用 PNG。 */
const ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256];
/** 额外单独出的 PNG */
const PNG_SIZES = [16, 24, 32, 48, 64, 128, 256, 512];
/**
 * 小尺寸与大尺寸的分界。≤ 这个值用 `icon-small.svg`（单独绘制的一套几何，
 * 字形更大、圆角更小、底色更深），> 这个值用 `icon.svg`。
 * 两个文件都要改的话记得两套一起看 —— 见 app/src/render.rs 的 VARIANTS。
 */
const SMALL_MAX = 32;

// ============================================================
// 最小 PNG 解码器（8 位，colorType 2/6，非隔行）
// ============================================================
function crc32(buf) {
  if (!crc32.table) {
    const t = new Int32Array(256);
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      t[n] = c;
    }
    crc32.table = t;
  }
  let crc = -1;
  for (let i = 0; i < buf.length; i++) crc = crc32.table[(crc ^ buf[i]) & 0xff] ^ (crc >>> 8);
  return (crc ^ -1) >>> 0;
}

function decodePng(buf) {
  if (buf.readUInt32BE(0) !== 0x89504e47) throw new Error('不是 PNG');
  let p = 8, ihdr = null;
  const idat = [];
  while (p < buf.length) {
    const len = buf.readUInt32BE(p);
    const type = buf.slice(p + 4, p + 8).toString('latin1');
    const data = buf.slice(p + 8, p + 8 + len);
    if (type === 'IHDR') {
      ihdr = {
        width: data.readUInt32BE(0), height: data.readUInt32BE(4),
        depth: data[8], colorType: data[9], interlace: data[12],
      };
    } else if (type === 'IDAT') idat.push(data);
    else if (type === 'IEND') break;
    p += 12 + len;
  }
  if (!ihdr) throw new Error('PNG 缺少 IHDR');
  if (ihdr.depth !== 8) throw new Error('只支持 8 位色深，实际 ' + ihdr.depth);
  if (ihdr.interlace !== 0) throw new Error('不支持隔行 PNG');
  const channels = ihdr.colorType === 6 ? 4 : ihdr.colorType === 2 ? 3 : 0;
  if (!channels) throw new Error('只支持 colorType 2/6，实际 ' + ihdr.colorType);

  const raw = zlib.inflateSync(Buffer.concat(idat));
  const { width: w, height: h } = ihdr;
  const stride = w * channels;
  const out = Buffer.alloc(w * h * 4);
  const prior = Buffer.alloc(stride);
  const cur = Buffer.alloc(stride);
  let sp = 0;
  for (let y = 0; y < h; y++) {
    const ft = raw[sp++];
    raw.copy(cur, 0, sp, sp + stride);
    sp += stride;
    for (let i = 0; i < stride; i++) {
      const a = i >= channels ? cur[i - channels] : 0;
      const b = prior[i];
      const c = i >= channels ? prior[i - channels] : 0;
      let v = cur[i];
      if (ft === 1) v += a;
      else if (ft === 2) v += b;
      else if (ft === 3) v += (a + b) >> 1;
      else if (ft === 4) {
        const pp = a + b - c;
        const pa = Math.abs(pp - a), pb = Math.abs(pp - b), pc = Math.abs(pp - c);
        v += pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      cur[i] = v & 0xff;
    }
    for (let x = 0; x < w; x++) {
      const s = x * channels, d = (y * w + x) * 4;
      out[d] = cur[s];
      out[d + 1] = cur[s + 1];
      out[d + 2] = cur[s + 2];
      out[d + 3] = channels === 4 ? cur[s + 3] : 255;
    }
    cur.copy(prior);
  }
  return { width: w, height: h, rgba: out };
}

// ============================================================
// PNG 编码（8 位 RGBA，filter 0）—— 用于 ICO 的大尺寸条目
// ============================================================
function pngChunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, 'latin1'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

function encodePng(img) {
  const { width: w, height: h, rgba } = img;
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  const raw = Buffer.alloc(h * (1 + w * 4));
  for (let y = 0; y < h; y++) {
    raw[y * (1 + w * 4)] = 0;
    rgba.copy(raw, y * (1 + w * 4) + 1, y * w * 4, (y + 1) * w * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk('IHDR', ihdr),
    pngChunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
    pngChunk('IEND', Buffer.alloc(0)),
  ]);
}

// ============================================================
// ICO 打包
// ============================================================
function bmpEntry(rgba, size) {
  const header = Buffer.alloc(40);
  header.writeUInt32LE(40, 0);
  header.writeInt32LE(size, 4);
  header.writeInt32LE(size * 2, 8); // XOR + AND 两层
  header.writeUInt16LE(1, 12);
  header.writeUInt16LE(32, 14);
  const xor = Buffer.alloc(size * size * 4);
  for (let y = 0; y < size; y++) {
    const srcRow = size - 1 - y;
    for (let x = 0; x < size; x++) {
      const s = (srcRow * size + x) * 4, d = (y * size + x) * 4;
      xor[d] = rgba[s + 2];
      xor[d + 1] = rgba[s + 1];
      xor[d + 2] = rgba[s];
      xor[d + 3] = rgba[s + 3];
    }
  }
  const maskStride = Math.ceil(size / 32) * 4;
  return Buffer.concat([header, xor, Buffer.alloc(maskStride * size, 0)]);
}

function packIco(entries) {
  const dir = Buffer.alloc(6);
  dir.writeUInt16LE(0, 0);
  dir.writeUInt16LE(1, 2);
  dir.writeUInt16LE(entries.length, 4);
  let offset = 6 + entries.length * 16;
  const table = [];
  const blobs = [];
  for (const e of entries) {
    const b = Buffer.alloc(16);
    b.writeUInt8(e.size >= 256 ? 0 : e.size, 0);
    b.writeUInt8(e.size >= 256 ? 0 : e.size, 1);
    b.writeUInt16LE(1, 4);
    b.writeUInt16LE(32, 6);
    b.writeUInt32LE(e.data.length, 8);
    b.writeUInt32LE(offset, 12);
    table.push(b);
    blobs.push(e.data);
    offset += e.data.length;
  }
  return Buffer.concat([dir, ...table, ...blobs]);
}

// ============================================================
// 主流程
// ============================================================
function main() {
  const argv = process.argv.slice(2);
  const svg = fs.readFileSync(SVG_SRC, 'utf8');
  const svgSmall = fs.readFileSync(SVG_SMALL_SRC, 'utf8');
  console.log('源文件 app/ui/brand/icon.svg        ' + svg.length + ' 字节（≥' + (SMALL_MAX + 1) + 'px）');
  console.log('源文件 app/ui/brand/icon-small.svg  ' + svgSmall.length + ' 字节（≤' + SMALL_MAX + 'px）');

  if (argv.includes('--check')) {
    let bad = 0;
    for (const n of PNG_SIZES) {
      const f = path.join(BRAND, `icon-${n}.png`);
      if (!fs.existsSync(f)) { console.log('  ✗ 缺 icon-' + n + '.png'); bad++; continue; }
      const { width, height } = decodePng(fs.readFileSync(f));
      const ok = width === n && height === n;
      console.log(`  icon-${n}.png ${width}×${height} ${ok ? '✔' : '✗'}`);
      if (!ok) bad++;
    }
    const ico = path.join(BRAND, 'icon.ico');
    if (!fs.existsSync(ico)) { console.log('  ✗ 缺 icon.ico'); bad++; }
    else {
      const b = fs.readFileSync(ico);
      const cnt = b.readUInt16LE(4);
      const sizes = [...Array(cnt)].map((_, i) => b.readUInt8(6 + i * 16) || 256);
      console.log(`  icon.ico ${cnt} 个尺寸：${sizes.join('/')} ${cnt === ICO_SIZES.length ? '✔' : '✗'}`);
      if (cnt !== ICO_SIZES.length) bad++;
    }
    console.log(bad ? `✗ ${bad} 项未通过` : '✔ 产物齐全');
    process.exit(bad ? 1 : 0);
  }

  if (!argv.includes('--no-build')) {
    console.log('\n先构建应用（光栅化要用它）…');
    const b = spawnSync(NODE, [path.join(ROOT, 'scripts', 'build.cjs')], { windowsHide: true,
      encoding: 'utf8', timeout: 1800000, cwd: ROOT,
    });
    const tail = (b.stdout || '').trim().split('\n').slice(-3).join('\n');
    console.log(tail);
    if (b.status !== 0) { console.error('✗ 构建失败'); process.exit(1); }
  }
  if (!fs.existsSync(EXE)) {
    console.error('✗ 找不到 ' + EXE + '\n  先跑 node scripts/build.cjs');
    process.exit(1);
  }

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'deskbase-icon-'));
  console.log('\n光栅化（应用内 WebView2）→ ' + tmpDir);
  const r = spawnSync(EXE, [], { windowsHide: true,
    env: { ...process.env, DESKBASE_RENDER: tmpDir, DESKBASE_DATA_DIR: tmpDir },
    encoding: 'utf8', timeout: 180000,
  });
  const logFile = path.join(tmpDir, 'render.log');
  if (fs.existsSync(logFile)) {
    console.log(fs.readFileSync(logFile, 'utf8').trim());
  }
  if (r.status !== 0) {
    console.error('✗ 光栅化失败（退出码 ' + r.status + '）');
    console.error('  临时目录保留在 ' + tmpDir);
    process.exit(1);
  }

  // 收产物、校验、打包。每个尺寸选它该用的那套几何。
  let failed = 0;
  const rendered = new Map();
  for (const size of [...new Set([...ICO_SIZES, ...PNG_SIZES])].sort((a, b) => a - b)) {
    const prefix = size <= SMALL_MAX ? 'icon-small' : 'icon';
    const src = path.join(tmpDir, `${prefix}-${size}.png`);
    if (!fs.existsSync(src)) {
      console.log(`  ✗ ${size}px 没有产出（${prefix}）`);
      failed++;
      continue;
    }
    const img = decodePng(fs.readFileSync(src));
    if (img.width !== size || img.height !== size) {
      console.log(`  ✗ ${size}px 尺寸不对：${img.width}×${img.height}`);
      failed++;
      continue;
    }
    const a = (x, y) => img.rgba[(y * size + x) * 4 + 3];
    const corners = [a(0, 0), a(size - 1, 0), a(0, size - 1), a(size - 1, size - 1)];
    if (corners.some((v) => v > 8)) {
      console.log(`  ✗ ${size}px 四角不透明：${corners.join(',')}`);
      failed++;
      continue;
    }
    rendered.set(size, img);
    if (PNG_SIZES.includes(size)) {
      fs.writeFileSync(path.join(BRAND, `icon-${size}.png`), fs.readFileSync(src));
    }
    console.log(`  ${String(size).padStart(3)}px  ✔  ${prefix}`);
  }
  if (failed) {
    console.error(`\n✗ ${failed} 个尺寸有问题，未打包`);
    process.exit(1);
  }

  const entries = ICO_SIZES.map((size) => ({
    size,
    data: size <= 64 ? bmpEntry(rendered.get(size).rgba, size) : encodePng(rendered.get(size)),
  }));
  const ico = packIco(entries);
  fs.writeFileSync(path.join(BRAND, 'icon.ico'), ico);
  console.log('\n✔ icon.ico  ' + (ico.length / 1024).toFixed(1) + ' KB  (' +
    ICO_SIZES.map((s) => s + (s <= 64 ? 'B' : 'P')).join(' ') + ')');

  const rgbaSrc = path.join(tmpDir, 'icon-rgba-256.bin');
  if (fs.existsSync(rgbaSrc)) {
    fs.copyFileSync(rgbaSrc, path.join(BRAND, 'icon-rgba-256.bin'));
    console.log('✔ icon-rgba-256.bin  ' + (fs.statSync(rgbaSrc).size / 1024).toFixed(1) + ' KB  (256×256×4)');
  } else {
    console.log('⚠ 没有拿到 icon-rgba-256.bin（Rust 窗口图标会用不上）');
  }

  // 复核用contact sheet：把每个尺寸按 1:1 与 4 倍最近邻并排画在亮/暗两种底上。
  // 小尺寸的观感只能靠放大看，眼盯着 16px 原图是看不出问题的。
  const sheetPath = path.join(os.tmpdir(), 'deskbase-icon-review.png');
  fs.writeFileSync(sheetPath, contactSheet(rendered));
  console.log('✔ 复核图  ' + sheetPath);

  fs.rmSync(tmpDir, { recursive: true, force: true });
  console.log('\n完成。B = BMP/DIB 条目，P = PNG 条目。');
}

/** 把各尺寸拼成一张对照图：亮底/暗底 × 1:1/4× 四行。
 *  小尺寸的观感只能靠放大看 —— 盯着 16px 原图是看不出问题的。 */
function contactSheet(rendered) {
  const all = [...rendered.keys()].sort((a, b) => a - b);
  const small = all.filter((s) => s <= 48);
  const rows = [
    { bg: [252, 250, 245], sizes: all, mul: 1 },
    { bg: [26, 25, 23], sizes: all, mul: 1 },
    { bg: [252, 250, 245], sizes: small, mul: 4 },
    { bg: [26, 25, 23], sizes: small, mul: 4 },
  ];
  const PAD = 20;
  const rowH = rows.map((r) => Math.max(...r.sizes.map((s) => s * r.mul)) + PAD * 2);
  const rowW = rows.map((r) => r.sizes.reduce((a, s) => a + s * r.mul + PAD, PAD));
  const W = Math.max(...rowW);
  const H = rowH.reduce((a, b) => a + b, 0);
  const canvas = { width: W, height: H, rgba: Buffer.alloc(W * H * 4) };

  const fill = (x0, y0, w, h, r, g, b) => {
    for (let y = Math.max(0, y0); y < Math.min(H, y0 + h); y++)
      for (let x = Math.max(0, x0); x < Math.min(W, x0 + w); x++) {
        const d = (y * W + x) * 4;
        canvas.rgba[d] = r; canvas.rgba[d + 1] = g; canvas.rgba[d + 2] = b; canvas.rgba[d + 3] = 255;
      }
  };
  const blit = (img, x0, y0, mul) => {
    for (let y = 0; y < img.height; y++)
      for (let x = 0; x < img.width; x++) {
        const s = (y * img.width + x) * 4;
        const a = img.rgba[s + 3] / 255;
        for (let dy = 0; dy < mul; dy++)
          for (let dx = 0; dx < mul; dx++) {
            const px = x0 + x * mul + dx, py = y0 + y * mul + dy;
            if (px < 0 || py < 0 || px >= W || py >= H) continue;
            const d = (py * W + px) * 4;
            canvas.rgba[d] = Math.round(img.rgba[s] * a + canvas.rgba[d] * (1 - a));
            canvas.rgba[d + 1] = Math.round(img.rgba[s + 1] * a + canvas.rgba[d + 1] * (1 - a));
            canvas.rgba[d + 2] = Math.round(img.rgba[s + 2] * a + canvas.rgba[d + 2] * (1 - a));
            canvas.rgba[d + 3] = 255;
          }
      }
  };

  let y = 0;
  rows.forEach((row, ri) => {
    fill(0, y, W, rowH[ri], ...row.bg);
    let x = PAD;
    const maxH = Math.max(...row.sizes.map((s) => s * row.mul));
    for (const size of row.sizes) {
      const d = size * row.mul;
      blit(rendered.get(size), x, y + PAD + Math.round((maxH - d) / 2), row.mul);
      x += d + PAD;
    }
    y += rowH[ri];
  });
  return encodePng(canvas);
}

main();
