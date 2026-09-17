/**
 * DeskBase · Windows 构建脚本
 * ============================================================
 * 为什么需要这个脚本：
 *
 * 本机有三处非标准，直接用 cargo 会踩坑：
 *   1. Windows SDK 装在非标准位置 D:\Windows Kits\10，而 reg.exe 被安全策略拉黑，
 *      vcvars64.bat 查不到注册表里的 KitsRoot10，导致 LIB 里缺 SDK 的库
 *      → 链接报 link.exe exit code 1181
 *   2. Node 的 spawnSync 直接调 cmd.exe 时内层引号会被转义坏
 *      → 报「'\"D:\...\vcvars64.bat\"' 不是内部或外部命令」
 *   3. Rust 的 default 工具链是 GNU，项目要用 MSVC
 *
 * 用法：
 *   node scripts/build.cjs            构建 release
 *   node scripts/build.cjs --debug    构建 debug
 *   node scripts/build.cjs --test     跑测试（用 debug）
 *   node scripts/build.cjs --run      构建后启动
 *   node scripts/build.cjs --check    只做 cargo check（最快）
 */

const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const os = require('os');

// ---------------- 环境配置 ----------------
// 若你的安装位置不同，改这几行即可
const VC_VARS = 'D:\\VisualStudio\\VC\\Auxiliary\\Build\\vcvars64.bat';
const SDK_ROOT = 'D:\\Windows Kits\\10';
const SDK_VER = '10.0.26100.0';
const RUSTUP_HOME = process.env.RUSTUP_HOME || 'C:\\Users\\you\\.rustup';
const CARGO_HOME = process.env.CARGO_HOME || 'C:\\Users\\you\\.cargo';
const TOOLCHAIN = 'stable-x86_64-pc-windows-msvc';

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
const MODE = has('--test') ? 'test' : has('--check') ? 'check' : has('--debug') ? 'debug' : 'release';

// ---------------- 1. 捕获 MSVC 环境 ----------------
function msvcEnv() {
  if (!fs.existsSync(VC_VARS)) {
    die(
      `找不到 vcvars64.bat: ${VC_VARS}\n` +
        `  请安装 Visual Studio 或 Build Tools 的「使用 C++ 的桌面开发」工作负载，\n` +
        `  或修改本脚本顶部的 VC_VARS。`
    );
  }

  // 落一个 .bat 再执行 —— 避免 Node 与 cmd.exe 的引号转义冲突
  const bat = path.join(os.tmpdir(), 'deskbase-vcvars.bat');
  fs.writeFileSync(bat, `@echo off\r\ncall "${VC_VARS}"\r\nset\r\n`, 'ascii');

  const r = spawnSync('cmd.exe', ['/c', bat], { encoding: 'utf8', maxBuffer: 8 * 1024 * 1024 });
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
  const libs = [
    path.join(SDK_ROOT, 'Lib', SDK_VER, 'ucrt', 'x64'),
    path.join(SDK_ROOT, 'Lib', SDK_VER, 'um', 'x64'),
  ].filter((d) => fs.existsSync(d));
  const incs = ['ucrt', 'shared', 'um', 'winrt', 'cppwinrt']
    .map((s) => path.join(SDK_ROOT, 'Include', SDK_VER, s))
    .filter((d) => fs.existsSync(d));

  if (!libs.length) {
    log(`  ⚠ 未找到 Windows SDK 的 Lib 目录（${SDK_ROOT}\\Lib\\${SDK_VER}）`);
    log('    如果链接时报 LNK1181，请检查 SDK 安装位置并修改本脚本的 SDK_ROOT / SDK_VER');
  } else {
    env.LIB = [...libs, env.LIB || ''].filter(Boolean).join(';');
    env.INCLUDE = [...incs, env.INCLUDE || ''].filter(Boolean).join(';');
    const sdkBin = path.join(SDK_ROOT, 'bin', SDK_VER, 'x64');
    if (fs.existsSync(sdkBin)) env.PATH = sdkBin + ';' + (env.PATH || '');
    log(`  Windows SDK：LIB ${libs.length} 项、INCLUDE ${incs.length} 项`);
  }

  // ---------------- 3. 指定 MSVC 工具链 ----------------
  env.RUSTUP_HOME = RUSTUP_HOME;
  env.CARGO_HOME = CARGO_HOME;
  env.RUSTUP_TOOLCHAIN = TOOLCHAIN;
  env.RUSTC = path.join(RUSTUP_HOME, 'toolchains', TOOLCHAIN, 'bin', 'rustc.exe');
  delete env.RUSTFLAGS; // 之前为 GNU 路线加的 link-self-contained 必须清掉

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
    ? ['test', '--', '--nocapture']
    : MODE === 'check'
    ? ['check']
    : ['build', ...(MODE === 'release' ? ['--release'] : [])];

log('');
log(`=== cargo ${args.join(' ')} ===`);
const t0 = Date.now();
const r = spawnSync(cargo, args, {
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
      const child = spawnSync(exe, [], { stdio: 'inherit', env });
      process.exit(child.status || 0);
    }
  } else {
    log('⚠ 未找到产物：' + exe);
  }
}
