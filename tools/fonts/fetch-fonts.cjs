/**
 * 拉取并校验 UI 内嵌字体
 * ============================================================
 * 为什么要有这个脚本：
 *   字体是要随安装包分发的二进制文件，来源必须可追溯、可复现。
 *   这里记录官方 release 地址与 SHA-256，任何人（或 CI）都能重新拉一遍并核对
 *   仓库里的那份字节是否一致 —— 这就是「可审计」在字体上的落地。
 *
 * 用法：
 *   node tools/fonts/fetch-fonts.cjs          校验仓库里的字体（不联网）
 *   node tools/fonts/fetch-fonts.cjs --fetch  先下载再校验（会联网）
 *
 * 注意：本机没有 unzip / 7z，所以脚本自带最小 ZIP 解析（只用 store / deflate）。
 */

const fs = require('fs');
const path = require('path');
const zlib = require('zlib');
const https = require('https');
const crypto = require('crypto');

const ROOT = path.resolve(__dirname, '..', '..');
const DEST_DIR = path.join(ROOT, 'app', 'ui', 'fonts');
const CACHE = path.join(require('os').tmpdir(), 'deskbase-fonts');

/** 已入库字体的登记表。加字体时在这里加一行。 */
const FONTS = [
  {
    // 得意黑（Smiley Sans）—— SIL OFL 1.1，未修改再分发
    name: '得意黑 Smiley Sans',
    release: 'https://github.com/atelier-anchor/smiley-sans/releases/download/v2.0.1/smiley-sans-v2.0.1.zip',
    releaseSha256: '299c0be6c960ae37361762eca76f7d0cd516615435bb96c0d4b98a1e70178a07',
    // 包内的 WOFF2（TTF 版衍生），体积最小
    entry: 'SmileySans-Oblique.ttf.woff2',
    dest: 'smiley-sans-oblique.woff2',
    sha256: '731f22973349404b15a88a99ef3b5dd4104c0965c23b7e485c1f11e84fea99e2',
    license: {
      from: 'https://api.github.com/repos/atelier-anchor/smiley-sans/contents/LICENSE?ref=v2.0.1',
      dest: 'OFL-smiley-sans.txt',
      sha256: null, // 许可文本不锁定哈希：上游更新版权年份不影响入库副本
    },
    note: '保留字体名 Smiley Sans（OFL 保留字体名为 Smiley / 得意黑，未修改再分发可直接使用原名称）',
  },
];

function download(url, dest) {
  return new Promise((resolve, reject) => {
    fs.mkdirSync(path.dirname(dest), { recursive: true });
    const f = fs.createWriteStream(dest);
    const go = (u, depth) => {
      if (depth > 6) return reject(new Error('重定向过多'));
      https
        .get(u, { headers: { 'User-Agent': 'deskbase-build' } }, (r) => {
          if ([301, 302, 303, 307, 308].includes(r.statusCode)) {
            r.resume();
            return go(r.headers.location, depth + 1);
          }
          if (r.statusCode !== 200) {
            r.resume();
            return reject(new Error('HTTP ' + r.statusCode + ' ' + u));
          }
          r.pipe(f);
          f.on('finish', () => f.close(() => resolve()));
        })
        .on('error', reject);
    };
    go(url, 0);
  });
}

function getText(url) {
  return new Promise((resolve, reject) => {
    const go = (u, depth) => {
      if (depth > 6) return reject(new Error('重定向过多'));
      https
        .get(u, { headers: { 'User-Agent': 'deskbase-build', Accept: 'application/vnd.github+json' } }, (r) => {
          if ([301, 302, 303, 307, 308].includes(r.statusCode)) {
            r.resume();
            return go(r.headers.location, depth + 1);
          }
          if (r.statusCode !== 200) {
            r.resume();
            return reject(new Error('HTTP ' + r.statusCode + ' ' + u));
          }
          let s = '';
          r.setEncoding('utf8');
          r.on('data', (c) => (s += c));
          r.on('end', () => resolve(s));
        })
        .on('error', reject);
    };
    go(url, 0);
  });
}

/** 极简 ZIP 读取 */
function unzip(buf) {
  let eocd = -1;
  for (let i = buf.length - 22; i >= 0 && i > buf.length - 66000; i--) {
    if (buf.readUInt32LE(i) === 0x06054b50) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) throw new Error('不是有效的 zip：找不到 EOCD');
  const count = buf.readUInt16LE(eocd + 10);
  let p = buf.readUInt32LE(eocd + 16);
  const out = [];
  for (let i = 0; i < count; i++) {
    if (buf.readUInt32LE(p) !== 0x02014b50) throw new Error('中央目录项签名不对');
    const method = buf.readUInt16LE(p + 10);
    const compSize = buf.readUInt32LE(p + 20);
    const nameLen = buf.readUInt16LE(p + 28);
    const extraLen = buf.readUInt16LE(p + 30);
    const cmtLen = buf.readUInt16LE(p + 32);
    const localOff = buf.readUInt32LE(p + 42);
    const name = buf.slice(p + 46, p + 46 + nameLen).toString('utf8');
    const lNameLen = buf.readUInt16LE(localOff + 26);
    const lExtraLen = buf.readUInt16LE(localOff + 28);
    const start = localOff + 30 + lNameLen + lExtraLen;
    const raw = buf.slice(start, start + compSize);
    out.push({ name, data: method === 0 ? raw : zlib.inflateRawSync(raw) });
    p += 46 + nameLen + extraLen + cmtLen;
  }
  return out;
}

const sha256 = (b) => crypto.createHash('sha256').update(b).digest('hex');

(async () => {
  const doFetch = process.argv.includes('--fetch');
  let failed = 0;

  for (const f of FONTS) {
    console.log('\n=== ' + f.name + ' ===');

    if (doFetch) {
      const zip = path.join(CACHE, path.basename(new URL(f.release).pathname));
      if (!fs.existsSync(zip)) {
        console.log('  下载 ' + f.release);
        await download(f.release, zip);
      }
      const buf = fs.readFileSync(zip);
      if (f.releaseSha256 && sha256(buf) !== f.releaseSha256) {
        console.log('  ✗ release 包哈希不匹配，拒绝提取');
        failed++;
        continue;
      }
      const entry = unzip(buf).find((e) => e.name === f.entry);
      if (!entry) {
        console.log('  ✗ 包内找不到 ' + f.entry);
        failed++;
        continue;
      }
      fs.mkdirSync(DEST_DIR, { recursive: true });
      fs.writeFileSync(path.join(DEST_DIR, f.dest), entry.data);
      console.log('  ✔ 提取 ' + f.dest);

      const licPath = path.join(DEST_DIR, f.license.dest);
      const lic = await getText(f.license.from);
      if (/^\{/.test(lic.trim())) {
        // GitHub contents API 返回 base64
        const j = JSON.parse(lic);
        fs.writeFileSync(licPath, Buffer.from(j.content, 'base64'));
      } else {
        fs.writeFileSync(licPath, lic, 'utf8');
      }
      console.log('  ✔ 提取 ' + f.license.dest);
    }

    // 校验仓库里现有的文件
    const fontPath = path.join(DEST_DIR, f.dest);
    if (!fs.existsSync(fontPath)) {
      console.log('  ✗ 仓库里没有 ' + f.dest + '，用 --fetch 生成');
      failed++;
      continue;
    }
    const bytes = fs.readFileSync(fontPath);
    const got = sha256(bytes);
    const magic = bytes.slice(0, 4).toString('latin1');
    const okMagic = magic === 'wOF2';
    const okHash = !f.sha256 || got === f.sha256;
    console.log('  ' + f.dest);
    console.log('    大小   ' + bytes.length + ' 字节 (' + (bytes.length / 1048576).toFixed(3) + ' MB)');
    console.log('    魔数   ' + magic + (okMagic ? '  ✔ 是合法 WOFF2' : '  ✗ 不是 WOFF2'));
    console.log('    sha256 ' + got + (okHash ? '  ✔' : '  ✗ 期望 ' + f.sha256));
    if (!okMagic || !okHash) failed++;

    const licPath = path.join(DEST_DIR, f.license.dest);
    console.log(
      '    ' + f.license.dest + '  ' + (fs.existsSync(licPath) ? '✔ 在库（OFL 要求随字体分发）' : '✗ 缺失')
    );
    if (!fs.existsSync(licPath)) failed++;
  }

  console.log('\n' + (failed ? '✗ ' + failed + ' 项未通过' : '✔ 全部通过'));
  process.exit(failed ? 1 : 0);
})().catch((e) => {
  console.error('失败：' + e.message);
  process.exit(1);
});
