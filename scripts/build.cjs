/**
 * DeskBase · Windows 构建脚本
 * ============================================================
 * 为什么需要这个脚本：
 *
 * 本机有三处非标准，直接用 cargo 会踩坑：
 *   1. Windows SDK 装在非标准位置（D:\ 或 E:\Windows Kits\10 之类），而 reg.exe
 *      可能被安全策略拉黑，vcvars64.bat 查不到注册表里的 KitsRoot10，
 *      导致 LIB 里缺 SDK 的库 → 链接报
 *      `LNK1181: cannot open input file 'kernel32.lib'`
 *      （这个报错离原因很远。脚本用"扫盘符"代替查注册表来兜住它）
 *   2. Node 的 spawnSync 直接调 cmd.exe 时内层引号会被转义坏
 *      → 报「'\"D:\...\vcvars64.bat\"' 不是内部或外部命令」
 *   3. Rust 的 default 工具链是 GNU，项目要用 MSVC
 *
 * 用法：
 *   node scripts/build.cjs            构建 release
 *   node scripts/build.cjs --debug    构建 debug
 *   node scripts/build.cjs --test     跑测试（用 debug）
 *   node scripts/build.cjs --test --ignored
 *                                     只跑**需要网络**的测试（updater 的两个 #[ignore]：
 *                                     真取 GitHub 清单 + 真下载几 MB 并替换一次）。
 *                                     为什么必须有这个口子：本机直接用 cargo 会踩
 *                                     工具链的坑（见文件头第 1–3 条），而设计上
 *                                     "要联网的测试用 #[ignore] 手动跑" —— 两条
 *                                     加起来等于"这两个测试在这台机器上没法跑"。
 *                                     2026-09-19 加：**自动更新的最终验证只能靠它们**。
 *   node scripts/build.cjs --run      构建后启动
 *   node scripts/build.cjs --check    只做 cargo check（最快）
 *   node scripts/build.cjs --smoke    构建后跑界面烟测（真实点击，见 ui-smoke.cjs）
 *   node scripts/build.cjs --e2e      构建后跑端到端验收（真实导入一条链路，见 tests/e2e-import.cjs）
 */

const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const os = require('os');

// ---------------- 环境配置（自动探测，可用环境变量覆盖）----------------
//
// 这几项**刻意不写死路径**。写死之后这个脚本只能在一台机器上跑 ——
// 别人 clone 下来第一步就卡住，而"能不能构建"是公开仓库最基本的要求。
// 每一项都按「环境变量 → 常见安装位置 → 报错并给出可操作的提示」三级回退。
const TOOLCHAIN = 'stable-x86_64-pc-windows-msvc';

/** 在候选列表里找第一个存在的路径 */
function firstExisting(cands) {
  for (const c of cands) if (c && fs.existsSync(c)) return c;
  return null;
}

/** 找 vcvars64.bat：先问 vswhere（VS 官方的定位工具），再扫常见目录 */
function findVcVars() {
  if (process.env.DESKBASE_VCVARS) return process.env.DESKBASE_VCVARS;

  const vswhere = path.join(
    process.env['ProgramFiles(x86)'] || 'C:\\Program Files (x86)',
    'Microsoft Visual Studio', 'Installer', 'vswhere.exe'
  );
  if (fs.existsSync(vswhere)) {
    const r = spawnSync(vswhere, [
      '-latest', '-products', '*',
      '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
      '-property', 'installationPath',
    ], { encoding: 'utf8', timeout: 30000, windowsHide: true });
    const base = (r.stdout || '').trim().split('\n')[0].trim();
    if (base) {
      const p = path.join(base, 'VC', 'Auxiliary', 'Build', 'vcvars64.bat');
      if (fs.existsSync(p)) return p;
    }
  }

  const roots = [
    'C:\\Program Files\\Microsoft Visual Studio',
    'C:\\Program Files (x86)\\Microsoft Visual Studio',
    'D:\\Program Files\\Microsoft Visual Studio',
    'D:\\Microsoft Visual Studio',
  ];
  const editions = ['2022', '2026', '2019', 'Community', 'Professional', 'Enterprise', 'BuildTools'];
  const cands = [];
  for (const r of roots) {
    for (const e of editions) {
      cands.push(path.join(r, e, 'VC', 'Auxiliary', 'Build', 'vcvars64.bat'));
      cands.push(path.join(r, e, 'BuildTools', 'VC', 'Auxiliary', 'Build', 'vcvars64.bat'));
    }
  }
  return firstExisting(cands);
}

/** 找 Windows SDK 根目录 */
function findSdkRoot() {
  if (process.env.DESKBASE_SDK_ROOT) return process.env.DESKBASE_SDK_ROOT;

  // 为什么要把所有盘符都扫一遍，而不是只列几个常见位置：
  //   SDK 允许装在任意盘（本机就在 `E:\Windows Kits\10`），而**自动发现它的
  //   唯一可靠途径是注册表** —— vcvars64.bat 正是靠 `reg.exe` 查 KitsRoot10。
  //   一旦 reg.exe 不可用（受限环境、组策略、或本机那种程序黑名单），
  //   vcvars 就静默地不设 LIB，于是链接阶段报 `LNK1181: cannot open input
  //   file 'kernel32.lib'` —— 报错离原因很远，极难定位。
  //   扫盘符的成本是 26 次 existsSync，可以忽略；换来的是"装在哪儿都能构建"。
  const roots = [
    path.join(process.env.ProgramFiles || 'C:\\Program Files', 'Windows Kits', '10'),
    'C:\\Program Files (x86)\\Windows Kits\\10',
  ];
  for (let c = 'A'.charCodeAt(0); c <= 'Z'.charCodeAt(0); c++) {
    roots.push(`${String.fromCharCode(c)}:\\Windows Kits\\10`);
  }
  return firstExisting(roots);
}

/** SDK 版本目录：取 Lib 下版本号最大的那个，不写死 */
function findSdkVer(sdkRoot) {
  if (process.env.DESKBASE_SDK_VER) return process.env.DESKBASE_SDK_VER;
  if (!sdkRoot) return null;
  const lib = path.join(sdkRoot, 'Lib');
  if (!fs.existsSync(lib)) return null;
  const vers = fs.readdirSync(lib)
    .filter((d) => /^\d+\.\d+\.\d+\.\d+$/.test(d))
    .sort((a, b) => {
      const pa = a.split('.').map(Number), pb = b.split('.').map(Number);
      for (let i = 0; i < 4; i++) if (pa[i] !== pb[i]) return pb[i] - pa[i];
      return 0;
    });
  return vers[0] || null;
}

const VC_VARS = findVcVars();
const SDK_ROOT = findSdkRoot();
const SDK_VER = findSdkVer(SDK_ROOT);
// rustup 默认装在 %USERPROFILE%\.rustup —— 用 os.homedir() 而不是写死用户名
const RUSTUP_HOME = process.env.RUSTUP_HOME || path.join(os.homedir(), '.rustup');
const CARGO_HOME = process.env.CARGO_HOME || path.join(os.homedir(), '.cargo');

const ROOT = path.resolve(__dirname, '..');
const APP_DIR = path.join(ROOT, 'app');

const log = (s) => console.log(s);
const die = (s) => {
  console.error('✗ ' + s);
  process.exit(1);
};

// ---------------- 参数 ----------------
const argv = process.argv.slice(2);
const has = (f) => argv.includes(f);
const STRICT = has('--strict');
const MODE = has('--fix') ? 'fix' : has('--test') ? 'test' : has('--check') ? 'check' : has('--debug') ? 'debug' : 'release';

// ---------------- 1. 捕获 MSVC 环境 ----------------
function msvcEnv() {
  if (!VC_VARS) {
    die(
      `找不到 vcvars64.bat（Visual Studio 的 C++ 编译环境）。\n\n` +
        `  需要装 Visual Studio 2022（或更新）或独立的 Build Tools，并勾选\n` +
        `  「使用 C++ 的桌面开发」工作负载 —— 只要这一个，不用装 IDE。\n` +
        `    https://visualstudio.microsoft.com/downloads/  →  Build Tools for Visual Studio\n\n` +
        `  已经装了但还是找不到？用环境变量直接指定：\n` +
        `    set DESKBASE_VCVARS=D:\\你的路径\\VC\\Auxiliary\\Build\\vcvars64.bat`
    );
  }

  // 落一个 .bat 再执行 —— 避免 Node 与 cmd.exe 的引号转义冲突
  const bat = path.join(os.tmpdir(), 'deskbase-vcvars.bat');
  fs.writeFileSync(bat, `@echo off\r\ncall "${VC_VARS}"\r\nset\r\n`, 'ascii');

  const r = spawnSync('cmd.exe', ['/c', bat], { windowsHide: true, encoding: 'utf8', maxBuffer: 8 * 1024 * 1024 });
  if (!(r.stdout || '').includes('=')) {
    die('vcvars64.bat 执行失败：' + JSON.stringify((r.stderr || '').slice(0, 300)));
  }

  const env = { ...process.env };
  let n = 0;
  for (const line of r.stdout.split(/\r?\n/)) {
    const i = line.indexOf('=');
    if (i <= 0) continue;
    const k = line.slice(0, i);
    if (
      /^(PATH|LIB|INCLUDE|VCINSTALLDIR|VCToolsInstallDir|WindowsSdkDir|WindowsSDKVersion|UniversalCRTSdkDir|UCRTVersion|VSCMD_ARG_.*)$/i.test(
        k
      )
    ) {
      env[k] = line.slice(i + 1);
      n++;
    }
  }
  log(`  MSVC 环境：捕获 ${n} 个变量`);

  // ---------------- 2. 补 Windows SDK ----------------
  // SDK 通常由 vcvars64.bat 自己配好；这里补一遍是为了兜住"装了多个 SDK 版本、
  // 环境变量指向的不是我们要的那个"这种情况。探测不到就跳过，不当作致命错误。
  if (!SDK_ROOT || !SDK_VER) {
    log('  Windows SDK：未探测到独立安装，沿用 vcvars64.bat 配置的环境');
    log('    （若链接时报 LNK1181，用 DESKBASE_SDK_ROOT 与 DESKBASE_SDK_VER 指定）');
  } else {
    const libs = [
      path.join(SDK_ROOT, 'Lib', SDK_VER, 'ucrt', 'x64'),
      path.join(SDK_ROOT, 'Lib', SDK_VER, 'um', 'x64'),
    ].filter((d) => fs.existsSync(d));
    const incs = ['ucrt', 'shared', 'um', 'winrt', 'cppwinrt']
      .map((s) => path.join(SDK_ROOT, 'Include', SDK_VER, s))
      .filter((d) => fs.existsSync(d));

    if (!libs.length) {
      log(`  ⚠ 未找到 Windows SDK 的 Lib 目录（${SDK_ROOT}\\Lib\\${SDK_VER}）`);
      log('    如果链接时报 LNK1181，请检查 SDK 安装位置');
    } else {
      env.LIB = [...libs, env.LIB || ''].filter(Boolean).join(';');
      env.INCLUDE = [...incs, env.INCLUDE || ''].filter(Boolean).join(';');
      const sdkBin = path.join(SDK_ROOT, 'bin', SDK_VER, 'x64');
      if (fs.existsSync(sdkBin)) env.PATH = sdkBin + ';' + (env.PATH || '');
      log(`  Windows SDK：${SDK_VER}，LIB ${libs.length} 项、INCLUDE ${incs.length} 项`);
    }
  }

  // ---------------- 3. 指定 MSVC 工具链 ----------------
  env.RUSTUP_HOME = RUSTUP_HOME;
  env.CARGO_HOME = CARGO_HOME;
  env.RUSTUP_TOOLCHAIN = TOOLCHAIN;
  env.RUSTC = path.join(RUSTUP_HOME, 'toolchains', TOOLCHAIN, 'bin', 'rustc.exe');
  // --strict：复现 CI 的零警告硬标准（CI 用 RUSTFLAGS=-D warnings）。
  // 为什么要能在本地跑：警告只有变成硬失败才守得住，而本地不验的话，
  // "推上去才发现 CI 红了"会反复发生 —— 一次编译几分钟，来回很贵。
  if (STRICT) {
    env.RUSTFLAGS = '-D warnings';
  } else {
    delete env.RUSTFLAGS; // 之前为 GNU 路线加的 link-self-contained 必须清掉
  }

  // ---------------- 4. 关掉 TLS 吊销检查（本机第四处非标准环境）----------------
  // 本机访问 crates.io 时 schannel 报 CRYPT_E_REVOCATION_OFFLINE：
  //   SSL connect error ... 由于吊销服务器已脱机，吊销功能无法检查吊销。
  // 这是 schannel 的**吊销列表（CRL）服务器连不上**，不是证书本身有问题 ——
  // 只要吊销服务器不可达，任何 HTTPS 请求都会被拒，于是 cargo 一个包都下不来。
  // 注意它**只影响首次拉取新依赖**：已有依赖全在本机 registry 缓存里，
  // 所以之前一直没暴露出来，是加 image 时才发现。
  //
  // 关掉的是"吊销状态检查"，不是证书链校验 —— 证书仍然要能链到受信任根。
  // 取舍：CRL 检查能防的是"证书在签发后被吊销"这一种情况，而那要求攻击者
  // 同时拿到 crates.io 的有效私钥并让官方去吊销；在没有可用 CRL 服务器的
  // 网络里，继续强制检查只会让构建完全无法进行。这个开关只在本脚本内生效，
  // 不改用户的全局 cargo 配置。
  env.CARGO_HTTP_CHECK_REVOKE = 'false';

  // 剔除 GNU 路线的残留，避免干扰
  env.PATH = (env.PATH || '')
    .split(';')
    .filter((p) => p && !/WinGet\\Packages\\BrechtSanders/i.test(p) && !/rustup.*self-contained/i.test(p))
    .join(';');

  return env;
}

// ---------------- 主流程 ----------------
log('=== DeskBase 构建 ===');
log(`  模式: ${MODE}`);
log(`  工程: ${APP_DIR}`);

if (!fs.existsSync(path.join(APP_DIR, 'Cargo.toml'))) {
  die(`找不到 app/Cargo.toml，工程目录不对：${APP_DIR}`);
}

const env = msvcEnv();
const cargo = path.join(RUSTUP_HOME, 'toolchains', TOOLCHAIN, 'bin', 'cargo.exe');
if (!fs.existsSync(cargo)) {
  die(`找不到 MSVC 工具链的 cargo：${cargo}\n  请先运行：rustup toolchain install ${TOOLCHAIN}`);
}

const args =
  MODE === 'test'
    ? ['test', '--', ...(has('--ignored') ? ['--ignored'] : []), '--nocapture']
    : MODE === 'fix'
    ? ['fix', '--bin', 'deskbase', '--allow-dirty', '--allow-staged']
    : MODE === 'check'
    ? ['check']
    : ['build', ...(MODE === 'release' ? ['--release'] : [])];

log('');
log(`=== cargo ${args.join(' ')} ===`);
const t0 = Date.now();
const r = spawnSync(cargo, args, { windowsHide: true,
  cwd: APP_DIR,
  encoding: 'utf8',
  env,
  maxBuffer: 64 * 1024 * 1024,
  timeout: 1800000,
});
const secs = ((Date.now() - t0) / 1000).toFixed(1);
const out = (r.stdout || '') + (r.stderr || '');
log(out.trimEnd());
log('');
log(`耗时 ${secs}s   退出码 ${r.status}`);

if (r.status !== 0) {
  const errs = out.split('\n').filter((l) => /^error/i.test(l));
  if (errs.length) {
    log('');
    log('错误摘要：');
    errs.slice(0, 12).forEach((e) => log('  ' + e.slice(0, 240)));
  }
  process.exit(r.status || 1);
}

// ---------------- 产物 ----------------
if (MODE !== 'test' && MODE !== 'check') {
  const profile = MODE === 'release' ? 'release' : 'debug';
  const exe = path.join(APP_DIR, 'target', profile, 'deskbase.exe');
  log('');
  if (fs.existsSync(exe)) {
    const st = fs.statSync(exe);
    log(`✔ 产物: ${exe}`);
    log(`  大小: ${(st.size / 1048576).toFixed(3)} MB (${st.size} 字节)`);
    if (has('--run')) {
      log('');
      log('=== 启动 ===');
      const child = spawnSync(exe, [], { windowsHide: true, stdio: 'inherit', env });
      process.exit(child.status || 0);
    }
    if (has('--smoke')) {
      log('');
      log('=== 界面烟测（真实点击） ===');
      const r2 = spawnSync(process.execPath, [path.join(__dirname, 'ui-smoke.cjs'), exe], { windowsHide: true,
        stdio: 'inherit',
        env,
      });
      process.exit(r2.status || 0);
    }
    if (has('--e2e')) {
      log('');
      log('=== 端到端验收（真实导入一条链路） ===');
      const r3 = spawnSync(process.execPath, [path.join(ROOT, 'tests', 'e2e-import.cjs'), exe], { windowsHide: true,
        stdio: 'inherit',
        env,
      });
      process.exit(r3.status || 0);
    }
  } else {
    log('⚠ 未找到产物：' + exe);
  }
}
