#!/usr/bin/env node
/**
 * DeskBase 零污染快照与对比工具（零第三方依赖）
 *
 * 用途：在「安装/运行前」与「操作后」各拍一张快照，逐项比对注册表、文件系统、
 *       计划任务、服务、自启项的差异。差异为 0 即证明「零污染 / 零残留」。
 *
 * 三个子命令：
 *   take   拍快照
 *   diff   对比两个快照（人类可读 + JSON）
 *   verify 判定是否零残留（带白名单容忍）
 *
 * 仅依赖系统自带 Node + reg.exe / schtasks.exe / sc.exe，无 npm 依赖。
 */
'use strict';

import fs from 'fs';
import path from 'path';
import { execFileSync } from 'child_process';

const TOOL_NAME = 'deskbase-snapshot';
const TOOL_VERSION = '1.0.0';
const IS_WIN = process.platform === 'win32';

// 默认注册表采集范围（会在 --help 中展示）
const DEFAULT_REG_KEYS = [
  'HKCU\\Software',
  'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall',
  'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run',
  'HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall',
  'HKLM\\SYSTEM\\CurrentControlSet\\Services',
];

// 默认自启项采集范围（Run 键本身，非递归）
const DEFAULT_STARTUP_KEYS = [
  'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run',
  'HKLM\\Software\\Microsoft\\Windows\\CurrentVersion\\Run',
];

// ---------------------------------------------------------------------------
// 通用：执行系统命令
// ---------------------------------------------------------------------------
function runCmd(file, args) {
  try {
    const out = execFileSync(file, args, {
      encoding: 'utf8',
      maxBuffer: 200 * 1024 * 1024,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return { ok: true, stdout: out || '', stderr: '' };
  } catch (e) {
    // reg/schtasks/sc 在「键不存在」或「无权限」时返回非 0，视为空结果并保留信息
    return {
      ok: false,
      stdout: (e.stdout || '').toString(),
      stderr: (e.stderr || '').toString() || e.message,
    };
  }
}

// ---------------------------------------------------------------------------
// 注册表采集与解析
// ---------------------------------------------------------------------------
function parseReg(text, map) {
  let cur = null;
  const lines = text.split(/\r?\n/);
  for (const raw of lines) {
    if (!raw.trim()) continue;
    // 键头：非缩进行，且形如 HKEY_...
    if (!/^\s/.test(raw) && /^HKEY_/.test(raw.trim())) {
      const key = raw.trim();
      if (!map[key]) map[key] = { values: {} };
      cur = key;
      continue;
    }
    if (!cur) continue;
    // 值行：name + REG_TYPE + data（data 可能含空格）
    let m = raw.match(/^\s+(.+?)\s+(REG_\w+)\s+([\s\S]*)$/);
    if (m) {
      const name = m[1].trim();
      const repr = m[2] + '=' + m[3].replace(/\s+$/, '');
      map[cur].values[name] = repr;
      continue;
    }
    // 只有 name + type、无 data 的情况
    m = raw.match(/^\s+(.+?)\s+(REG_\w+)\s*$/);
    if (m) {
      map[cur].values[m[1].trim()] = m[2] + '=';
    }
  }
}

function collectRegistry(keys) {
  const regMap = {};
  const errors = [];
  for (const key of keys) {
    const r = runCmd('reg.exe', ['query', key, '/s']);
    if (r.stdout) {
      parseReg(r.stdout, regMap);
    }
    if (!r.ok && r.stderr) {
      errors.push(`reg query "${key}" 失败: ${r.stderr.split('\n')[0]}`);
    }
  }
  return { data: regMap, errors };
}

function collectStartup(keys) {
  const result = {};
  const errors = [];
  for (const key of keys) {
    const r = runCmd('reg.exe', ['query', key]);
    const entries = {};
    if (r.stdout) {
      const tmp = {};
      parseReg(r.stdout, tmp);
      // 该键本身就是唯一的 key
      const k = Object.keys(tmp)[0];
      if (k) Object.assign(entries, tmp[k].values);
    }
    if (!r.ok && r.stderr) {
      errors.push(`reg query "${key}" 失败: ${r.stderr.split('\n')[0]}`);
    }
    result[key] = entries;
  }
  return { data: result, errors };
}

// ---------------------------------------------------------------------------
// 文件系统采集（递归、长路径、权限拒绝跳过）
// ---------------------------------------------------------------------------
function toLongPath(p) {
  let r = path.resolve(p);
  if (IS_WIN) {
    r = r.replace(/\//g, '\\');
    if (!r.startsWith('\\\\?\\')) r = '\\\\?\\' + r;
  }
  return r;
}

function logicalPath(longOrAbs) {
  let p = longOrAbs;
  if (IS_WIN) p = p.replace(/^\\\\?\\/, '');
  return path.resolve(p);
}

function collectDirs(dirs) {
  const files = {};
  const errors = [];
  for (const dir of dirs) {
    const root = toLongPath(dir);
    const rootLogical = logicalPath(root);
    const walk = (current) => {
      let entries;
      try {
        entries = fs.readdirSync(current, { withFileTypes: true });
      } catch (e) {
        errors.push(`${logicalPath(current)} :: ${e.message}`);
        return;
      }
      for (const ent of entries) {
        const full = current.endsWith('\\') ? current + ent.name : current + '\\' + ent.name;
        const logical = logicalPath(full);
        if (ent.isDirectory()) {
          walk(full);
        } else if (ent.isFile()) {
          try {
            const st = fs.statSync(full);
            files[logical] = { size: st.size, mtimeMs: st.mtimeMs };
          } catch (e) {
            errors.push(`${logical} :: ${e.message}`);
          }
        }
        // 符号链接 / 其他类型：跳过
      }
    };
    try {
      const st = fs.statSync(root);
      if (st.isDirectory()) walk(root);
      else if (st.isFile()) {
        files[rootLogical] = { size: st.size, mtimeMs: st.mtimeMs };
      }
    } catch (e) {
      errors.push(`${rootLogical} :: ${e.message}`);
    }
  }
  return { data: files, errors };
}

// ---------------------------------------------------------------------------
// 计划任务 / 服务
// ---------------------------------------------------------------------------
function collectTasks() {
  const tasks = {};
  const r = runCmd('schtasks.exe', ['/query', '/fo', 'LIST']);
  const errors = [];
  if (r.stdout) {
    const re = /TaskName:\s*(.+)/g;
    let m;
    while ((m = re.exec(r.stdout)) !== null) {
      tasks[m[1].trim()] = true;
    }
  }
  if (!r.ok && r.stderr) {
    errors.push(`schtasks 失败: ${r.stderr.split('\n')[0]}`);
  }
  return { data: tasks, errors };
}

function parseSc(text) {
  const map = {};
  let cur = null;
  for (const raw of text.split(/\r?\n/)) {
    const sn = raw.match(/^SERVICE_NAME:\s*(.+)$/);
    if (sn) {
      cur = sn[1].trim();
      map[cur] = { state: '', startType: '' };
      continue;
    }
    if (!cur) continue;
    const st = raw.match(/STATE\s*:\s*\d+\s+(\w+)/);
    if (st) map[cur].state = st[1];
    const stt = raw.match(/START_TYPE\s*:\s*\d+\s+(\w+)/);
    if (stt) map[cur].startType = stt[1];
  }
  return map;
}

function collectServices() {
  const r = runCmd('sc.exe', ['query', 'type=', 'service', 'state=', 'all']);
  const errors = [];
  let data = {};
  if (r.stdout) data = parseSc(r.stdout);
  if (!r.ok && r.stderr) errors.push(`sc query 失败: ${r.stderr.split('\n')[0]}`);
  return { data, errors };
}

// ---------------------------------------------------------------------------
// take
// ---------------------------------------------------------------------------
function cmdTake(argv) {
  let out = null, label = '', dirs = [], regKeys = null;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--out') out = argv[++i];
    else if (argv[i] === '--label') label = argv[++i];
    else if (argv[i] === '--dirs') dirs = argv[++i].split(',').map((s) => s.trim()).filter(Boolean);
    else if (argv[i] === '--reg-keys') regKeys = argv[++i].split(',').map((s) => s.trim()).filter(Boolean);
  }
  if (!out) {
    console.error('错误：take 需要 --out <快照文件路径>');
    process.exit(2);
  }
  if (dirs.length === 0) {
    console.error('提示：未指定 --dirs，将不采集任何文件系统。如需监控目录请加 --dirs "C:/a,C:/b"。');
  }
  const rk = regKeys || DEFAULT_REG_KEYS;

  console.error(`[take] 采集注册表（${rk.length} 个根键）...`);
  const reg = collectRegistry(rk);
  console.error(`[take] 采集自启项（${DEFAULT_STARTUP_KEYS.length} 个键）...`);
  const startup = collectStartup(DEFAULT_STARTUP_KEYS);
  console.error(`[take] 采集文件系统（${dirs.length} 个目录）...`);
  const fsd = collectDirs(dirs);
  console.error('[take] 采集计划任务...');
  const tasks = collectTasks();
  console.error('[take] 采集服务...');
  const svc = collectServices();

  const snapshot = {
    tool: TOOL_NAME,
    version: TOOL_VERSION,
    createdAt: new Date().toISOString(),
    label: label || '',
    scope: { regKeys: rk, dirs, startupKeys: DEFAULT_STARTUP_KEYS },
    registry: reg.data,
    startup: startup.data,
    files: fsd.data,
    tasks: tasks.data,
    services: svc.data,
    errors: [].concat(reg.errors, startup.errors, fsd.errors, tasks.errors, svc.errors),
  };
  fs.writeFileSync(out, JSON.stringify(snapshot, null, 2));
  console.error(`[take] 已写入快照：${out}`);
  console.error(`[take] 注册表键数=${Object.keys(reg.data).length} 文件数=${Object.keys(fsd.data).length} 任务数=${Object.keys(tasks.data).length} 服务数=${Object.keys(svc.data).length} 错误数=${snapshot.errors.length}`);
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------
function diffSnapshots(A, B) {
  const result = {
    summary: {},
    registry: { addedKeys: [], removedKeys: [], changedKeys: [] },
    files: { added: [], removed: [], changed: [] },
    tasks: { added: [], removed: [] },
    services: { added: [], removed: [], changed: [] },
    startup: { added: [], removed: [], changed: [] },
    errors: { a: A.errors || [], b: B.errors || [] },
  };

  // 注册表
  {
    const keysA = Object.keys(A.registry || {});
    const keysB = Object.keys(B.registry || {});
    const setA = new Set(keysA);
    const setB = new Set(keysB);
    for (const k of keysB) if (!setA.has(k)) result.registry.addedKeys.push(k);
    for (const k of keysA) if (!setB.has(k)) result.registry.removedKeys.push(k);
    for (const k of keysA) {
      if (!setB.has(k)) continue;
      const va = A.registry[k].values || {};
      const vb = B.registry[k].values || {};
      const namesA = Object.keys(va);
      const namesB = Object.keys(vb);
      const sA = new Set(namesA);
      const sB = new Set(namesB);
      const addedValues = [], removedValues = [], changedValues = [];
      for (const n of namesB) if (!sA.has(n)) addedValues.push(n);
      for (const n of namesA) if (!sB.has(n)) removedValues.push(n);
      for (const n of namesA) {
        if (sB.has(n) && va[n] !== vb[n]) {
          changedValues.push({ name: n, a: va[n], b: vb[n] });
        }
      }
      if (addedValues.length || removedValues.length || changedValues.length) {
        result.registry.changedKeys.push({ key: k, addedValues, removedValues, changedValues });
      }
    }
  }

  // 文件系统
  {
    const fa = A.files || {};
    const fb = B.files || {};
    const sa = new Set(Object.keys(fa));
    const sb = new Set(Object.keys(fb));
    for (const p of sb) if (!sa.has(p)) result.files.added.push(p);
    for (const p of sa) if (!sb.has(p)) result.files.removed.push(p);
    for (const p of sa) {
      if (!sb.has(p)) continue;
      const x = fa[p], y = fb[p];
      if (x.size !== y.size || x.mtimeMs !== y.mtimeMs) {
        result.files.changed.push({ path: p, aSize: x.size, bSize: y.size, aMtimeMs: x.mtimeMs, bMtimeMs: y.mtimeMs });
      }
    }
  }

  // 计划任务
  {
    const ta = A.tasks || {};
    const tb = B.tasks || {};
    const sa = new Set(Object.keys(ta));
    const sb = new Set(Object.keys(tb));
    for (const n of sb) if (!sa.has(n)) result.tasks.added.push(n);
    for (const n of sa) if (!sb.has(n)) result.tasks.removed.push(n);
  }

  // 服务
  {
    const sa = A.services || {};
    const sb = B.services || {};
    const ka = new Set(Object.keys(sa));
    const kb = new Set(Object.keys(sb));
    for (const n of kb) if (!ka.has(n)) result.services.added.push(n);
    for (const n of ka) if (!kb.has(n)) result.services.removed.push(n);
    for (const n of ka) {
      if (!kb.has(n)) continue;
      if (sa[n].state !== sb[n].state || sa[n].startType !== sb[n].startType) {
        result.services.changed.push({ name: n, aState: sa[n].state, bState: sb[n].state, aStartType: sa[n].startType, bStartType: sb[n].startType });
      }
    }
  }

  // 自启项
  {
    const sa = A.startup || {};
    const sb = B.startup || {};
    const locs = new Set([...Object.keys(sa), ...Object.keys(sb)]);
    for (const loc of locs) {
      const ea = (sa[loc] || {});
      const eb = (sb[loc] || {});
      const na = new Set(Object.keys(ea));
      const nb = new Set(Object.keys(eb));
      for (const n of nb) if (!na.has(n)) result.startup.added.push({ location: loc, name: n, value: eb[n] });
      for (const n of na) if (!nb.has(n)) result.startup.removed.push({ location: loc, name: n, value: ea[n] });
      for (const n of na) {
        if (nb.has(n) && ea[n] !== eb[n]) {
          result.startup.changed.push({ location: loc, name: n, a: ea[n], b: eb[n] });
        }
      }
    }
  }

  result.summary = {
    registry: { addedKeys: result.registry.addedKeys.length, removedKeys: result.registry.removedKeys.length, changedKeys: result.registry.changedKeys.length },
    files: { added: result.files.added.length, removed: result.files.removed.length, changed: result.files.changed.length },
    tasks: { added: result.tasks.added.length, removed: result.tasks.removed.length },
    services: { added: result.services.added.length, removed: result.services.removed.length, changed: result.services.changed.length },
    startup: { added: result.startup.added.length, removed: result.startup.removed.length, changed: result.startup.changed.length },
  };
  return result;
}

function formatDiff(d) {
  const lines = [];
  const S = d.summary;
  lines.push('=== 快照对比报告 ===');
  lines.push(`注册表: 新增键 ${S.registry.addedKeys} / 删除键 ${S.registry.removedKeys} / 修改键 ${S.registry.changedKeys}`);
  lines.push(`文件:   新增 ${S.files.added} / 删除 ${S.files.removed} / 修改 ${S.files.changed}`);
  lines.push(`任务:   新增 ${S.tasks.added} / 删除 ${S.tasks.removed}`);
  lines.push(`服务:   新增 ${S.services.added} / 删除 ${S.services.removed} / 修改 ${S.services.changed}`);
  lines.push(`自启:   新增 ${S.startup.added} / 删除 ${S.startup.removed} / 修改 ${S.startup.changed}`);
  lines.push('');

  if (d.registry.addedKeys.length) {
    lines.push(`[注册表·新增键] (${d.registry.addedKeys.length})`);
    d.registry.addedKeys.forEach((k) => lines.push('  + ' + k));
    lines.push('');
  }
  if (d.registry.removedKeys.length) {
    lines.push(`[注册表·删除键] (${d.registry.removedKeys.length})`);
    d.registry.removedKeys.forEach((k) => lines.push('  - ' + k));
    lines.push('');
  }
  if (d.registry.changedKeys.length) {
    lines.push(`[注册表·修改键] (${d.registry.changedKeys.length})`);
    d.registry.changedKeys.forEach((c) => {
      lines.push('  ~ ' + c.key);
      c.addedValues.forEach((n) => lines.push('      + 值: ' + n));
      c.removedValues.forEach((n) => lines.push('      - 值: ' + n));
      c.changedValues.forEach((v) => lines.push(`      ~ 值: ${v.name}\n          A: ${v.a}\n          B: ${v.b}`));
    });
    lines.push('');
  }

  if (d.files.added.length) {
    lines.push(`[文件·新增] (${d.files.added.length})`);
    d.files.added.forEach((p) => lines.push('  + ' + p));
    lines.push('');
  }
  if (d.files.removed.length) {
    lines.push(`[文件·删除] (${d.files.removed.length})`);
    d.files.removed.forEach((p) => lines.push('  - ' + p));
    lines.push('');
  }
  if (d.files.changed.length) {
    lines.push(`[文件·修改] (${d.files.changed.length})`);
    d.files.changed.forEach((f) => lines.push(`  ~ ${f.path}\n      size ${f.aSize} -> ${f.bSize}, mtime ${f.aMtimeMs} -> ${f.bMtimeMs}`));
    lines.push('');
  }

  if (d.tasks.added.length) {
    lines.push(`[任务·新增] (${d.tasks.added.length})`);
    d.tasks.added.forEach((n) => lines.push('  + ' + n));
    lines.push('');
  }
  if (d.tasks.removed.length) {
    lines.push(`[任务·删除] (${d.tasks.removed.length})`);
    d.tasks.removed.forEach((n) => lines.push('  - ' + n));
    lines.push('');
  }

  if (d.services.added.length) {
    lines.push(`[服务·新增] (${d.services.added.length})`);
    d.services.added.forEach((n) => lines.push('  + ' + n));
    lines.push('');
  }
  if (d.services.removed.length) {
    lines.push(`[服务·删除] (${d.services.removed.length})`);
    d.services.removed.forEach((n) => lines.push('  - ' + n));
    lines.push('');
  }
  if (d.services.changed.length) {
    lines.push(`[服务·修改] (${d.services.changed.length})`);
    d.services.changed.forEach((s) => lines.push(`  ~ ${s.name}\n      state ${s.aState} -> ${s.bState}, startType ${s.aStartType} -> ${s.bStartType}`));
    lines.push('');
  }

  if (d.startup.added.length) {
    lines.push(`[自启·新增] (${d.startup.added.length})`);
    d.startup.added.forEach((e) => lines.push(`  + [${e.location}] ${e.name} = ${e.value}`));
    lines.push('');
  }
  if (d.startup.removed.length) {
    lines.push(`[自启·删除] (${d.startup.removed.length})`);
    d.startup.removed.forEach((e) => lines.push(`  - [${e.location}] ${e.name} = ${e.value}`));
    lines.push('');
  }
  if (d.startup.changed.length) {
    lines.push(`[自启·修改] (${d.startup.changed.length})`);
    d.startup.changed.forEach((e) => lines.push(`  ~ [${e.location}] ${e.name}\n      A: ${e.a}\n      B: ${e.b}`));
    lines.push('');
  }

  // 采集错误信息
  const errA = d.errors.a || [];
  const errB = d.errors.b || [];
  if (errA.length || errB.length) {
    lines.push(`[采集错误] A=${errA.length} B=${errB.length}`);
    errA.forEach((e) => lines.push('  A! ' + e));
    errB.forEach((e) => lines.push('  B! ' + e));
    lines.push('');
  }
  return lines.join('\n');
}

function cmdDiff(argv) {
  let a = null, b = null, json = null;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--json') json = argv[++i];
    else if (!a) a = argv[i];
    else if (!b) b = argv[i];
  }
  if (!a || !b) {
    console.error('错误：diff 需要 <快照A> <快照B>');
    process.exit(2);
  }
  const A = JSON.parse(fs.readFileSync(a, 'utf8'));
  const B = JSON.parse(fs.readFileSync(b, 'utf8'));
  const d = diffSnapshots(A, B);
  const text = formatDiff(d);
  console.log(text);
  if (json) {
    fs.writeFileSync(json, JSON.stringify(d, null, 2));
    console.error(`[diff] 已写入 JSON：${json}`);
  }
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------
function loadAllowlist(file) {
  const patterns = [];
  if (!file) return patterns;
  const text = fs.readFileSync(file, 'utf8');
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    try {
      patterns.push(new RegExp(line));
    } catch (e) {
      console.error(`[verify] 白名单正则无效，已忽略: ${line} (${e.message})`);
    }
  }
  return patterns;
}

function matchable(item) {
  // 构造用于白名单匹配的字符串
  if (item.type === 'regAddKey') return item.key;
  if (item.type === 'regDelKey') return item.key;
  if (item.type === 'regAddVal') return `${item.key} :: ${item.name}`;
  if (item.type === 'regDelVal') return `${item.key} :: ${item.name}`;
  if (item.type === 'regChgVal') return `${item.key} :: ${item.name}`;
  if (item.type === 'fileAdd') return item.path;
  if (item.type === 'fileDel') return item.path;
  if (item.type === 'fileChg') return item.path;
  if (item.type === 'taskAdd') return item.name;
  if (item.type === 'taskDel') return item.name;
  if (item.type === 'svcAdd') return item.name;
  if (item.type === 'svcDel') return item.name;
  if (item.type === 'svcChg') return item.name;
  if (item.type === 'startAdd') return `${item.location} :: ${item.name}`;
  if (item.type === 'startDel') return `${item.location} :: ${item.name}`;
  if (item.type === 'startChg') return `${item.location} :: ${item.name}`;
  return '';
}

function cmdVerify(argv) {
  let a = null, b = null, allow = null, json = null;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--allow') allow = argv[++i];
    else if (argv[i] === '--json') json = argv[++i];
    else if (!a) a = argv[i];
    else if (!b) b = argv[i];
  }
  if (!a || !b) {
    console.error('错误：verify 需要 <快照A> <快照B>');
    process.exit(2);
  }
  const A = JSON.parse(fs.readFileSync(a, 'utf8'));
  const B = JSON.parse(fs.readFileSync(b, 'utf8'));
  const d = diffSnapshots(A, B);

  // 把 diff 拆成可匹配项
  const items = [];
  d.registry.addedKeys.forEach((k) => items.push({ type: 'regAddKey', key: k }));
  d.registry.removedKeys.forEach((k) => items.push({ type: 'regDelKey', key: k }));
  d.registry.changedKeys.forEach((c) => {
    c.addedValues.forEach((n) => items.push({ type: 'regAddVal', key: c.key, name: n }));
    c.removedValues.forEach((n) => items.push({ type: 'regDelVal', key: c.key, name: n }));
    c.changedValues.forEach((v) => items.push({ type: 'regChgVal', key: c.key, name: v.name }));
  });
  d.files.added.forEach((p) => items.push({ type: 'fileAdd', path: p }));
  d.files.removed.forEach((p) => items.push({ type: 'fileDel', path: p }));
  d.files.changed.forEach((f) => items.push({ type: 'fileChg', path: f.path }));
  d.tasks.added.forEach((n) => items.push({ type: 'taskAdd', name: n }));
  d.tasks.removed.forEach((n) => items.push({ type: 'taskDel', name: n }));
  d.services.added.forEach((n) => items.push({ type: 'svcAdd', name: n }));
  d.services.removed.forEach((n) => items.push({ type: 'svcDel', name: n }));
  d.services.changed.forEach((s) => items.push({ type: 'svcChg', name: s.name }));
  d.startup.added.forEach((e) => items.push({ type: 'startAdd', location: e.location, name: e.name }));
  d.startup.removed.forEach((e) => items.push({ type: 'startDel', location: e.location, name: e.name }));
  d.startup.changed.forEach((e) => items.push({ type: 'startChg', location: e.location, name: e.name }));

  const patterns = loadAllowlist(allow);
  const allowed = [];
  const real = [];
  for (const it of items) {
    const s = matchable(it);
    const hit = patterns.some((re) => re.test(s));
    if (hit) allowed.push({ ...it, match: s });
    else real.push({ ...it, match: s });
  }

  console.log('=== 零残留判定 ===');
  console.log(`总差异项: ${items.length}  可接受(白名单): ${allowed.length}  真实残留: ${real.length}`);
  console.log('');

  if (allowed.length) {
    console.log(`--- 本工具认为可接受的变化（白名单命中，不算残留）--- (${allowed.length})`);
    allowed.forEach((it) => console.log('  ~ ' + it.match));
    console.log('');
  }
  if (real.length) {
    console.log(`--- 真实残留（必须归零）--- (${real.length})`);
    real.forEach((it) => console.log('  ! ' + it.match));
    console.log('');
  }

  if (real.length === 0) {
    console.log('结论：未发现残留（差异全部可被白名单容忍，或根本没有差异）。');
    if (json) fs.writeFileSync(json, JSON.stringify({ clean: true, total: items.length, allowed: allowed.length, real: 0 }, null, 2));
    process.exit(0);
  } else {
    console.log('结论：发现真实残留，判定不通过。');
    if (json) fs.writeFileSync(json, JSON.stringify({ clean: false, total: items.length, allowed: allowed.length, real: real.length }, null, 2));
    process.exit(1);
  }
}

// ---------------------------------------------------------------------------
// help
// ---------------------------------------------------------------------------
function printHelp() {
  console.log(`DeskBase 零污染快照与对比工具 v${TOOL_VERSION}

用法:
  node snapshot.mjs take  --out <快照文件> [--label 标签] [--dirs "dir1,dir2"] [--reg-keys "k1,k2"]
  node snapshot.mjs diff  <快照A> <快照B> [--json <输出>]
  node snapshot.mjs verify <快照A> <快照B> [--allow <白名单文件>] [--json <输出>]
  node snapshot.mjs help

take 采集范围:
  注册表（默认，可用 --reg-keys 覆盖）:
    ${DEFAULT_REG_KEYS.map((k) => '    ' + k).join('\n    ')}
  自启项（固定）:
    ${DEFAULT_STARTUP_KEYS.map((k) => '    ' + k).join('\n    ')}
  文件系统:
    默认不扫全盘；必须用 --dirs 显式指定要监控的目录（逗号分隔）。
  计划任务: schtasks /query /fo LIST
  服务:     sc query type= service state= all

diff 输出: 分组缩进的人类可读文本；--json 可导出给 CI。
  差异分四类：新增 / 删除 / 修改，每类都给出精确路径。

verify 判定:
  差异为 0 或全被白名单容忍 -> 退出码 0，打印「未发现残留」
  存在真实残留 -> 退出码 1，逐条列出
  --allow 白名单为文本文件，每行一个正则表达式（# 开头为注释），
  命中正则的差异项视为「可接受变化」，其余为「真实残留」。

说明:
  - 零第三方依赖，仅需系统 Node + reg.exe/schtasks.exe/sc.exe。
  - 文件系统采集处理长路径（\\\\?\\ 前缀）与权限拒绝（跳过并记录，不崩溃）。
`);
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------
function main() {
  const args = process.argv.slice(2);
  const cmd = args[0];
  const rest = args.slice(1);
  switch (cmd) {
    case 'take':
      cmdTake(rest);
      break;
    case 'diff':
      cmdDiff(rest);
      break;
    case 'verify':
      cmdVerify(rest);
      break;
    case 'help':
    case '--help':
    case '-h':
    case undefined:
      printHelp();
      break;
    default:
      console.error(`未知子命令: ${cmd}`);
      printHelp();
      process.exit(2);
  }
}

main();
