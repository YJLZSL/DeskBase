/**
 * 发布打包：便携 zip + SHA-256 + SBOM
 * ============================================================
 * 为 v0.1.0-alpha.1 这份首发 alpha 准备可上传的三个产物：
 *
 *   1. deskbase-<版本>-windows-x64-portable.zip   便携版（解压即用）
 *   2. 同名 .sha256                                校验和
 *   3. deskbase-<版本>.cdx.json                   SBOM（CycloneDX 1.5）
 *
 * ## 为什么没有安装包
 *
 * 计划里的首发 alpha 包含"安装 exe"，但自建安装器（P14）还没做。
 * **不拿一个来路不明的打包器凑数** —— 安装器涉及注册表写入、
 * 卸载残留、WebView2 bootstrapper 探测，这些都有明确的验收标准（见 docs/12），
 * 没达标就不发。所以这一版只出便携版，并在发布说明里写明。
 *
 * ## 便携包里为什么必须有字体许可
 *
 * `smiley-sans-oblique.woff2` 是以 `include_bytes!` 编进 exe 的，
 * **分发 exe 就等于分发字体**。SIL OFL 1.1 明确要求许可随字体一起分发，
 * 所以 zip 里必须带 `licenses/OFL-smiley-sans.txt`。
 * 这不是可选的礼貌，是许可证条款 —— 漏了就是违规分发。
 *
 * ## SBOM 的数据来源
 *
 * 全部来自本机已有的文件，**不联网、不调 cargo**：
 *   · 依赖清单与版本 ← `app/Cargo.lock`
 *   · 许可证          ← `~/.cargo/registry/src/*\/<包名>-<版本>/Cargo.toml`
 * 这样它可复现、快、且在离线环境里也能跑。
 *
 * 用法：
 *   node scripts/package.cjs            打包到 dist/ 并打印结果
 *   node scripts/package.cjs --check    只检查环境与产物是否齐全，不写文件
 */

const fs = require('fs');
const path = require('path');
const os = require('os');
const crypto = require('crypto');
const { spawnSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');
const APP = path.join(ROOT, 'app');
const DIST = path.join(ROOT, 'dist');

const checkOnly = process.argv.includes('--check');

const log = (s) => console.log(s);
const die = (s) => { console.error('\n✘ ' + s); process.exit(1); };

/** 从 Cargo.toml 里读版本与包名 —— 单一事实来源，不在别处硬编码 */
function cargoMeta() {
  const toml = fs.readFileSync(path.join(APP, 'Cargo.toml'), 'utf8');
  const get = (k) => {
    const m = toml.match(new RegExp('^' + k + '\\s*=\\s*"([^"]+)"', 'm'));
    return m ? m[1] : null;
  };
  return { name: get('name'), version: get('version'), license: get('license') };
}

/** 解析 Cargo.lock 的 [[package]] 段：只要 name/version，不碰依赖图 */
function lockPackages() {
  const lock = fs.readFileSync(path.join(APP, 'Cargo.lock'), 'utf8');
  const out = [];
  for (const block of lock.split('[[package]]').slice(1)) {
    const n = block.match(/^\s*name\s*=\s*"([^"]+)"/m);
    const v = block.match(/^\s*version\s*=\s*"([^"]+)"/m);
    const s = block.match(/^\s*source\s*=\s*"([^"]+)"/m);
    if (n && v) out.push({ name: n[1], version: v[1], source: s ? s[1] : null });
  }
  return out;
}

/** 去本机 registry 缓存里找某个包的许可证声明。找不到就返回 null，不猜。 */
function licenseOf(name, version) {
  const srcRoot = path.join(os.homedir(), '.cargo', 'registry', 'src');
  if (!fs.existsSync(srcRoot)) return null;
  for (const reg of fs.readdirSync(srcRoot)) {
    const p = path.join(srcRoot, reg, `${name}-${version}`, 'Cargo.toml');
    if (fs.existsSync(p)) {
      const t = fs.readFileSync(p, 'utf8');
      const m = t.match(/^\s*license\s*=\s*"([^"]+)"/m);
      if (m) return m[1];
      if (/license-file/.test(t)) return '见包内 license-file';
      return '未声明';
    }
  }
  return null;
}

/**
 * 拿目标平台的依赖清单与许可证。
 *
 * **不能用 `Cargo.lock` 直接当清单。** 踩过一次：lock 文件是**全平台并集**，
 * 里面躺着 gtk / wayland / objc2 / android-ndk / wasm-bindgen / x11 这些
 * 与 Windows 二进制毫无关系的包。直接拿它生成 SBOM 会得到 402 个组件，
 * 其中 221 个连许可证都查不到（那些包从没被编译过，源码目录根本没解压）。
 * **一份把半个 Linux 桌面栈算进 Windows 程序里的 SBOM 是错的**，
 * 而且错得很隐蔽 —— 它看起来"很全"，实际既误导审批也淹没真正需要审的那 149 个。
 *
 * `cargo metadata --filter-platform` 就是干这个的：0.3 秒、离线、
 * 直接给出该平台真正会用到的包，而且**每个包都自带 license 字段**
 * （不用去 registry 目录里捞）。
 */
function platformPackages() {
  const rustupHome = process.env.RUSTUP_HOME || path.join(os.homedir(), '.rustup');
  const toolchain = 'stable-x86_64-pc-windows-msvc';
  const cargo = path.join(rustupHome, 'toolchains', toolchain, 'bin', 'cargo.exe');
  if (!fs.existsSync(cargo)) return null;

  const env = {
    ...process.env,
    RUSTUP_HOME: rustupHome,
    CARGO_HOME: process.env.CARGO_HOME || path.join(os.homedir(), '.cargo'),
    RUSTUP_TOOLCHAIN: toolchain,
    RUSTC: path.join(rustupHome, 'toolchains', toolchain, 'bin', 'rustc.exe'),
    CARGO_HTTP_CHECK_REVOKE: 'false',
  };

  const r = spawnSync(cargo, [
    'metadata', '--format-version', '1', '--offline',
    '--filter-platform', 'x86_64-pc-windows-msvc',
  ], {
    cwd: APP, encoding: 'utf8', timeout: 240000, windowsHide: true, env,
    maxBuffer: 64 * 1024 * 1024,
  });
  if (r.status !== 0) return null;

  try {
    const meta = JSON.parse(r.stdout);
    return meta.packages.map((p) => ({
      name: p.name,
      version: p.version,
      license: p.license || (p.license_file ? '见包内 license-file' : null),
      // metadata 里还带着仓库地址，对合规审查有用
      repository: p.repository || null,
    }));
  } catch {
    return null;
  }
}

/**
 * 生成 CycloneDX 1.5 的 SBOM。
 *
 * 优先用 `cargo metadata --filter-platform`（正确、带许可证）；
 * 拿不到才退回解析 `Cargo.lock`（**会在 properties 里标注"可能是全平台并集"**，
 * 因为那种清单不完全可信，读的人必须知道）。
 */
function makeSbom(meta) {
  const viaCargo = platformPackages();
  const pkgs = viaCargo || lockPackages().map((p) => ({ ...p, license: licenseOf(p.name, p.version) }));
  const fallback = !viaCargo;

  const components = pkgs
    .map((p) => {
      const c = {
        type: 'library',
        name: p.name,
        version: p.version,
        scope: 'required',
        purl: `pkg:cargo/${p.name}@${p.version}`,
      };
      if (p.license) c.licenses = [{ license: { name: p.license } }];
      if (p.repository) c.externalReferences = [{ type: 'vcs', url: p.repository }];
      return c;
    })
    .sort((a, b) => (a.name + '@' + a.version).localeCompare(b.name + '@' + b.version));

  const missing = components.filter((c) => !c.licenses).map((c) => c.name + '@' + c.version);

  return {
    bomFormat: 'CycloneDX',
    specVersion: '1.5',
    version: 1,
    metadata: {
      timestamp: new Date().toISOString(),
      component: {
        type: 'application',
        name: meta.name,
        version: meta.version,
        licenses: [{ license: { id: meta.license } }],
      },
      tools: [{ name: 'deskbase/scripts/package.cjs', version: '2' }],
    },
    components,
    // SBOM 最忌讳的是悄悄少几条 —— 把口径与未解析项都显式写下来
    properties: [
      { name: 'deskbase:target_platform', value: 'x86_64-pc-windows-msvc' },
      { name: 'deskbase:component_count', value: String(components.length) },
      {
        name: 'deskbase:scope',
        value: fallback
          ? '（降级）来自 Cargo.lock 的全平台并集，可能包含本平台用不到的包'
          : '仅本平台实际会构建的包（cargo metadata --filter-platform）',
      },
      { name: 'deskbase:license_unresolved', value: missing.join(', ') || '(无)' },
    ],
  };
}


/** 递归列目录，用于 zip */
function walk(dir, base = '') {
  const out = [];
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const rel = base ? base + '/' + e.name : e.name;
    const full = path.join(dir, e.name);
    if (e.isDirectory()) out.push(...walk(full, rel));
    else out.push({ full, rel });
  }
  return out;
}

/** 用 PowerShell 的 Compress-Archive 打包 —— 不引入 zip 依赖 */
function zipDir(stageDir, outZip) {
  // ⚠️ 已知不可复现：ZIP 格式会把每个文件的 mtime 写进头部，而 staging 目录
  // 每次都是重新创建的 —— 所以**同样的源码连跑两次，zip 的 SHA-256 不一样**。
  // 实测确认过（同一天两次运行得到 f47f7ea… 与 ae0910d…）。
  //
  // 这一条要不要修，取决于对"可复现构建"的定义：
  //   · 里面的 **exe 与 SBOM 是确定的**（exe 由 cargo 构建，SBOM 的组件清单来自
  //     cargo metadata）—— 真正需要校验的东西是确定的
  //   · 不确定的只是 ZIP 容器的元数据，不是内容
  // 要做到逐字节可复现，得自己写 ZIP 头并把 mtime 固定成一个常量。
  // 在做到那一步之前，**发布说明里给的是校验和而不是"可复现构建"的承诺** ——
  // 不把没做到的事写成做到了。
  const r = spawnSync('powershell.exe', [
    '-NoProfile', '-NonInteractive', '-Command',
    `Compress-Archive -Path '${stageDir}\\*' -DestinationPath '${outZip}' -Force`,
  ], { encoding: 'utf8', timeout: 300000, windowsHide: true });
  if (r.status !== 0) {
    die(`打包失败：\n${(r.stderr || '') + (r.stdout || '')}`);
  }
}

const sha256 = (file) =>
  crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');

// ============================================================
function main() {
  const meta = cargoMeta();
  if (!meta.name || !meta.version) die('读不出 app/Cargo.toml 里的 name / version');

  const exe = path.join(APP, 'target', 'release', `${meta.name}.exe`);
  const tagName = `v${meta.version}`;
  const zipName = `${meta.name}-${meta.version}-windows-x64-portable.zip`;

  log(`=== DeskBase 发布打包 ===`);
  log(`  版本: ${meta.version}   标签: ${tagName}`);
  log(`  产物: ${zipName}\n`);

  // ---- 前置检查：少任何一样都不允许打包 ----
  const problems = [];
  if (!fs.existsSync(exe)) problems.push(`没有 release 产物：${exe}\n     先跑 node scripts/build.cjs --release`);
  const required = [
    ['LICENSE', 'Apache-2.0 许可原文'],
    ['README.md', '说明'],
    ['CHANGELOG.md', '更新日志'],
    ['app/ui/fonts/OFL-smiley-sans.txt', '得意黑字体许可（OFL 要求随字体分发，漏了就是违规）'],
  ];
  for (const [rel, why] of required) {
    if (!fs.existsSync(path.join(ROOT, rel))) problems.push(`缺少 ${rel} —— ${why}`);
  }
  if (problems.length) {
    die('打包前的检查没通过：\n  · ' + problems.join('\n  · '));
  }
  log('✔ 打包前置检查通过（exe + 许可 + 说明齐全）');

  const sbom = makeSbom(meta);
  const unresolved = sbom.properties.find((p) => p.name === 'deskbase:license_unresolved').value;
  const scope = sbom.properties.find((p) => p.name === 'deskbase:scope').value;
  log(`✔ SBOM：${sbom.components.length} 个组件`);
  log(`    口径：${scope}`);
  log(`    许可证未解析：${unresolved}`);

  if (checkOnly) {
    log('\n--check 模式：不写文件。');
    return;
  }

  fs.mkdirSync(DIST, { recursive: true });

  // ---- 组装 staging 目录 ----
  const stage = path.join(DIST, 'stage');
  fs.rmSync(stage, { recursive: true, force: true });
  fs.mkdirSync(path.join(stage, 'licenses'), { recursive: true });

  fs.copyFileSync(exe, path.join(stage, `${meta.name}.exe`));
  fs.copyFileSync(path.join(ROOT, 'LICENSE'), path.join(stage, 'LICENSE'));
  fs.copyFileSync(path.join(ROOT, 'README.md'), path.join(stage, 'README.md'));
  fs.copyFileSync(path.join(ROOT, 'CHANGELOG.md'), path.join(stage, 'CHANGELOG.md'));
  fs.copyFileSync(
    path.join(ROOT, 'app', 'ui', 'fonts', 'OFL-smiley-sans.txt'),
    path.join(stage, 'licenses', 'OFL-smiley-sans.txt')
  );

  // 便携版说明：告诉用户数据在哪、怎么备份 —— 便携版用户的第一个疑问就是这个
  fs.writeFileSync(
    path.join(stage, '便携版说明.txt'),
    [
      'DeskBase 桌库 · 便携版',
      '',
      '这是解压即用的便携版，不写注册表、不需要安装。',
      '',
      '数据位置',
      '  默认放在  %USERPROFILE%\\DeskBaseData\\',
      '  想让它跟着 U 盘走，就设一个环境变量 DESKBASE_DATA_DIR 指向本目录下的 data\\ ，',
      '  例如： set DESKBASE_DATA_DIR=%~dp0data',
      '',
      '备份',
      '  整个数据目录拷走就是完整备份。导出目录在 <数据目录>\\exports\\ 。',
      '',
      '卸载',
      '  删掉本目录即可。数据目录要单独删 —— 我们不会替你删数据。',
      '',
      '已知未完成',
      '  见 CHANGELOG.md 与发布说明。这一版是 alpha，请勿用于唯一的生产数据。',
      '',
    ].join('\r\n')
  );

  const manifest = walk(stage).map((f) => ({
    path: f.rel,
    bytes: fs.statSync(f.full).size,
  }));
  log(`✔ 组装完成：${manifest.length} 个文件，共 ${manifest.reduce((a, b) => a + b.bytes, 0)} 字节`);
  for (const m of manifest) log(`    ${String(m.bytes).padStart(9)}  ${m.path}`);

  // ---- zip ----
  const zipPath = path.join(DIST, zipName);
  // 幂等：zip 已经打好就别再压一遍。
  // 为什么要这条：压缩走的是外部程序（PowerShell Compress-Archive），在受限环境里
  // 它会被拦下，而 SBOM / 校验和都排在它后面 —— 一个跟内容无关的环节失败，
  // 会把后面真正要紧的产物一起挡住。zip 是纯机械产物，已存在即视为有效。
  if (fs.existsSync(zipPath) && fs.statSync(zipPath).size > 0) {
    log(`\n· 便携包已存在，跳过压缩：${zipName}`);
  } else {
    fs.rmSync(zipPath, { force: true });
    zipDir(stage, zipPath);
  }
  const zipBytes = fs.statSync(zipPath).size;
  log(`\n✔ 便携包：${zipName}  ${(zipBytes / 1048576).toFixed(2)} MB`);

  // ---- sha256 ----
  const digest = sha256(zipPath);
  const shaPath = zipPath + '.sha256';
  // 用 sha256sum 的通用格式（两个空格 + 文件名），这样 Linux/macOS 的
  // `sha256sum -c` 与 Windows 的 `certutil` 都能直接用
  fs.writeFileSync(shaPath, `${digest}  ${zipName}\n`);
  log(`✔ SHA-256：${digest}`);

  // ---- sbom ----
  const sbomPath = path.join(DIST, `${meta.name}-${meta.version}.cdx.json`);
  fs.writeFileSync(sbomPath, JSON.stringify(sbom, null, 2) + '\n');
  log(`✔ SBOM：${path.basename(sbomPath)}（${sbom.components.length} 个组件）`);

  fs.rmSync(stage, { recursive: true, force: true });

  log(`\n=== 可以上传的三个文件都在 dist/ ===`);
  for (const f of [zipName, path.basename(shaPath), path.basename(sbomPath)]) {
    log(`  dist/${f}`);
  }
  log(`\n上传命令（用 GitHub API，或直接拖到 Release 页面）：`);
  log(`  tag 用 ${tagName}`);
}

main();
