#!/usr/bin/env node
// P0-11 脏数据语料库生成器
//
// 用途：按 docs/19-test-strategy.md 第 4.8 节列出的 10 类脏数据，生成一套
//       可被 CSV / JSON / Excel 导入流程直接使用的语料，并产出 manifest.json
//       供后续测试断言「每个文件应当被如何处理」。
//
// 约束：
//   - 不依赖任何第三方包，仅用 Node 内置模块。
//   - 可重复运行：输出目录先清空再重建。
//   - 运行：`node generate-corpus.mjs [--out <dir>]`
//     --out 缺省为脚本同级的 output/ 目录；CI / 临时验证可传入系统 Temp 路径。
//
// 判据词汇表（与 README.md、13-crash-test-and-corpus.md 保持一致）：
//   accept                  : 应被正确接受并完整保留（含全部字节 / 字符）。
//   reject                  : 应被明确拒绝并给出清晰错误提示，不得静默吞掉。
//   escape                  : 注入内容应作为普通文本安全保存 / 展示，绝不被执行。
//   accept-strict-text      : 接受但统一按文本存储，不擅自推断为危险类型，不崩溃。
//   reject-or-define        : 格式异常，要么明确拒绝并标出错误行，要么按预设规则处理。
//   accept-with-defined-null: 接受，但 NULL 与空串语义必须由导入器清晰定义并一致处理。
//   accept-or-encoding-error: 编码相关，识别成功则正确接受；识别失败给明确编码错误，
//                             绝不允许把 GBK 字节当 UTF-8 静默解码成乱码并入库。

import {
  writeFileSync,
  mkdirSync,
  rmSync,
  statSync,
  existsSync,
} from 'node:fs';
import { join, dirname, isAbsolute } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));

// Windows 扩展长度路径前缀：当文件名很长（如 255 字符）时，完整路径会超过
// MAX_PATH(260)，必须用 \\?\ 前缀才能创建 / 写入。统一在真正调用 fs 时套用。
function ep(p) {
  if (process.platform === 'win32' && isAbsolute(p) && !p.startsWith('\\\\?\\')) {
    // \\?\ 扩展长度前缀只认反斜杠，传入的前斜杠必须先归一化
    return '\\\\?\\' + p.replace(/\//g, '\\');
  }
  return p;
}

// ---- 解析 --out 参数 -------------------------------------------------------
const argv = process.argv.slice(2);
function getArg(name, fallback) {
  const i = argv.indexOf(name);
  if (i >= 0 && argv[i + 1]) return argv[i + 1];
  return fallback;
}
const OUT = getArg('--out', join(__dirname, 'output'));

// ---- 输出目录先清空再重建 --------------------------------------------------
if (existsSync(OUT)) rmSync(ep(OUT), { recursive: true, force: true });
mkdirSync(ep(OUT), { recursive: true });

const manifest = [];
let fileCount = 0;
let totalBytes = 0;

// 写入一个文件并记录大小
function write(relPath, data, encoding = 'utf8') {
  const full = join(OUT, relPath);
  mkdirSync(ep(dirname(full)), { recursive: true });
  writeFileSync(ep(full), data, encoding);
  const size = statSync(ep(full)).size;
  fileCount += 1;
  totalBytes += size;
  return size;
}

// 登记一条 manifest 记录
function record(entry) {
  manifest.push(entry);
}

// 把一组二维表同时写成 CSV 与 JSON（供两种导入路径复用）
// rows: 字符串数组的数组；headers: 列名数组
function table(relBase, headers, rows, { expected, note }) {
  const csvLines = [headers.join(',')];
  for (const r of rows) {
    csvLines.push(
      r
        .map((c) => {
          const s = String(c);
          // CSV 字段转义：含逗号 / 引号 / 换行时用双引号包裹并翻倍内部引号
          if (/[",\r\n]/.test(s)) return '"' + s.replace(/"/g, '""') + '"';
          return s;
        })
        .join(',')
    );
  }
  const csv = csvLines.join('\r\n') + '\r\n';
  write(relBase + '.csv', csv);

  const json = JSON.stringify(
    rows.map((r) => {
      const o = {};
      headers.forEach((h, i) => (o[h] = r[i]));
      return o;
    }),
    null,
    2
  );
  write(relBase + '.json', json);

  record({
    category: headers.join('/'),
    files: [relBase + '.csv', relBase + '.json'],
    format: ['csv', 'json'],
    expected,
    note,
  });
}

// ===========================================================================
// 类别 1：超长文本（单字段 1 MB 与 100 MB 两档）
// ===========================================================================
function makeLongText(mb) {
  const target = mb * 1024 * 1024;
  // 用可重复的混合文本填充，避免被压缩算法抹平（更接近真实长文本）
  const unit =
    '桌库本地优先办公工具箱长文本内容压力测试DeskBase long text stress 测试一二三四五 ';
  let s = '';
  while (s.length < target) s += unit;
  return s.slice(0, target);
}
for (const mb of [1, 100]) {
  const text = makeLongText(mb);
  table(
    `long_text/long_text_${mb}mb`,
    ['content'],
    [[text]],
    {
      expected: 'accept',
      note: `单字段约 ${mb} MB 纯文本，导入后须完整保留；搜索 / 打开须可用且不崩溃，超出合理上限时给出明确提示。`,
    }
  );
}

// ===========================================================================
// 类别 2：超长文件名（255 字符、含中文与 emoji）
// ===========================================================================
{
  const seg = '表格字段名超长测试案例'; // 11 个 UTF-16 单元
  let name = '';
  while (name.length < 249) name += seg;
  name = name.slice(0, 249) + '😀'; // 249 + emoji(2 单元) = 251
  const fname = name + '.csv'; // 251 + 4(.csv) = 255，单文件名组件上限 255
  const ok = fname.length === 255 && name.length === 251;
  const dir = join(OUT, 'long_filename');
  mkdirSync(ep(dir), { recursive: true });
  const full = join(dir, fname);
  writeFileSync(ep(full), 'name,notes\r\nlongname_test,文件名超长的导入文件，用于验证导入器对长文件名的处理\r\n');
  const size = statSync(ep(full)).size;
  fileCount += 1;
  totalBytes += size;
  record({
    category: 'long_filename',
    files: ['long_filename/' + fname],
    format: ['csv'],
    expected: 'accept',
    note: `文件名（含 .csv 扩展名）共 ${fname.length} UTF-16 单元（预期 255，校验${ok ? '通过' : '失败'}），其中主体 ${name.length} 单元含中文与 emoji。导入器须能接受并正确显示该文件名，不得截断或拒绝无理由。`,
  });
}

// ===========================================================================
// 类别 3：特殊字符（\0、控制字符、RTL 阿拉伯语、组合 emoji）
// ===========================================================================
{
  // 3.1 含 NUL 空字节（以原始字节写入，确保 \0 真的存在）
  const nulBuf = Buffer.from('name,notes\r\nfoo,\x00bar 中间有空字节\r\n', 'latin1');
  write('special/chars_nul.csv', nulBuf);
  record({
    category: 'special/nul',
    files: ['special/chars_nul.csv'],
    format: ['csv'],
    expected: 'accept-or-encoding-error',
    note: '字段内含 \\0 空字节。导入器须正确处理（截断 / 保留 / 报错均可，但须一致且明确），不得因此崩溃或越界读取。',
  });

  // 3.2 控制字符（C0/C1 区：\x01 \x02 \x1f \x7f 等）
  const ctrlBuf = Buffer.from(
    'name,notes\r\nctrl,\x01\x02\x1f\x7f 控制字符串\r\n',
    'latin1'
  );
  write('special/chars_control.csv', ctrlBuf);
  record({
    category: 'special/control',
    files: ['special/chars_control.csv'],
    format: ['csv'],
    expected: 'accept-or-encoding-error',
    note: '字段内含 C0/C1 控制字符。导入器须安全处理，不得触发终端/解析器的特殊行为或崩溃。',
  });

  // 3.3 RTL 阿拉伯语 + 强制 RTL 标记
  table('special/chars_rtl_arabic', ['text', 'note'], [
    ['السلام عليكم ورحمة الله', '阿拉伯语 RTL 文本'],
    ['\u202E' + '强制从右向左显示' + '\u202C', '含 U+202E 强制 RTL 标记'],
  ], {
    expected: 'accept',
    note: '阿拉伯语等 RTL 文本须按原样存储与展示，不得因方向控制符导致数据错乱或丢失。',
  });

  // 3.4 组合 emoji（ZWJ 序列）
  table('special/chars_combining_emoji', ['text', 'note'], [
    ['\u{1F468}\u200D\u{1F469}\u200D\u{1F467}', '家庭组合 emoji（ZWJ 序列）'],
    ['\u{1F3F4}\u200D\u2620\uFE0F', '海盗旗组合 emoji（ZWJ + 变体选择符）'],
    ['\u{1F44B}\u{1F3FB}', '肤色修饰组合 emoji'],
  ], {
    expected: 'accept',
    note: '组合 emoji 由代理对 + ZWJ/变体选择符组成，须作为整体完整保留，不得被拆成无意义码元。',
  });
}

// ===========================================================================
// 类别 4：生僻字（CJK 扩展 B/C/D 区、异体字）
// ===========================================================================
{
  const extB = '\u{20000}\u{20001}\u{2001F}\u{20BB7}'; // 𠀀 𠀁 𠀟 𬼷
  const extC = '\u{2A700}\u{2A71F}\u{2B740}\u{2B81D}'; // 𪜀 𪜟 𫝀 𫠝
  const extD = '\u{2B740}\u{2B7FF}\u{2B820}';
  const variant = '福飯﨑晴'; // CJK 兼容表意字（异体）：福/飯/崎/暑 的异体
  table('rare/cjk_ext_b', ['char', 'codepoint', 'note'], [
    [extB[0], 'U+20000', 'CJK 扩展 B'],
    [extB[1], 'U+20001', 'CJK 扩展 B'],
    [extB[2], 'U+2001F', 'CJK 扩展 B'],
    [extB[3], 'U+20BB7', 'CJK 扩展 B'],
  ], { expected: 'accept', note: 'CJK 扩展 B 区（ astral plane ）字符须完整存储，不得被替换成 U+FFFD。' });

  table('rare/cjk_ext_c', ['char', 'codepoint', 'note'], [
    [extC[0], 'U+2A700', 'CJK 扩展 C'],
    [extC[1], 'U+2A71F', 'CJK 扩展 C'],
    [extC[2], 'U+2B740', 'CJK 扩展 C'],
    [extC[3], 'U+2B81D', 'CJK 扩展 C'],
  ], { expected: 'accept', note: 'CJK 扩展 C 区字符须完整存储。' });

  table('rare/cjk_ext_d', ['char', 'codepoint', 'note'], [
    [extD[0], 'U+2B740', 'CJK 扩展 D'],
    [extD[1], 'U+2B7FF', 'CJK 扩展 D'],
    [extD[2], 'U+2B820', 'CJK 扩展 D'],
  ], { expected: 'accept', note: 'CJK 扩展 D 区字符须完整存储。' });

  table('rare/variant_chars', ['char', 'codepoint', 'note'], [
    [variant[0], 'U+FA1B', '异体字（福的兼容表意字）'],
    [variant[1], 'U+FA2A', '异体字（飯的兼容表意字）'],
    [variant[2], 'U+FA11', '异体字（崎的兼容表意字）'],
    [variant[3], 'U+FA12', '异体字（暑的兼容表意字）'],
  ], { expected: 'accept', note: '异体字 / 兼容表意字须按原字存储，不得被「自动正规化」成主字形而丢失用户意图。' });
}

// ===========================================================================
// 类别 5：注入串（SQL / XSS / 路径穿越 / 命令注入）
// ===========================================================================
{
  const payloads = [
    ["' OR 1=1 --", 'sql', 'SQL 注入：须作为普通文本存储，查询时参数化，绝不允许拼接执行。'],
    ['<script>alert(1)</script>', 'xss', 'XSS：须安全转义后存储 / 展示，绝不在渲染时执行脚本。'],
    ['../../etc/passwd', 'path', '路径穿越：须作为普通文本，不得被解析为文件系统路径去读取。'],
    ['$(rm -rf /)', 'command', '命令注入：须作为普通文本，绝不被 shell 执行。'],
    ['=HYPERLINK("http://evil")', 'csv-formula', 'CSV 公式注入：须作为文本，表格软件不得自动当作公式求值。'],
  ];
  for (const [p, kind, n] of payloads) {
    table(`injection/${kind}`, ['payload', 'kind'], [[p, kind]], {
      expected: 'escape',
      note: n,
    });
  }
}

// ===========================================================================
// 类别 6：格式异常（CSV 引号不闭合、JSON 语法错误、类型混列、空行空列、
//            多表头、合并单元格）
// ===========================================================================
{
  // 6.1 CSV 引号不闭合
  const unclosed = 'name,note\r\n"这个引号没有闭合\r\n';
  write('malformed/csv_unclosed_quote.csv', unclosed);
  record({
    category: 'malformed/csv_unclosed_quote',
    files: ['malformed/csv_unclosed_quote.csv'],
    format: ['csv'],
    expected: 'reject-or-define',
    note: '引号不闭合。导入器须明确报错并定位行号，或按定义好的恢复规则处理；不得静默丢弃后续字段导致数据错位。',
  });

  // 6.2 JSON 语法错误
  const badJson = '{\n  "name": "x",\n  "note": \n}\n';
  write('malformed/json_syntax_error.json', badJson);
  record({
    category: 'malformed/json_syntax_error',
    files: ['malformed/json_syntax_error.json'],
    format: ['json'],
    expected: 'reject',
    note: 'JSON 语法错误（note 值为空）。导入器须给出清晰的解析错误，不得静默生成空对象。',
  });

  // 6.3 类型混列
  table('malformed/mixed_type_column', ['id', 'value'], [
    ['1', 'hello'],
    ['2', '3.14'],
    ['3', 'true'],
    ['4', '2026-09-17'],
  ], {
    expected: 'accept-strict-text',
    note: '同一列混有字符串 / 数字 / 布尔 / 日期。导入器须不崩溃，统一按文本或明确推断类型，不得因类型推断失败丢行。',
  });

  // 6.4 空行空列
  table('malformed/empty_rows_cols', ['a', 'b', 'c'], [
    ['1', '', '3'],
    ['', '', ''],
    ['4', '', '6'],
  ], {
    expected: 'accept',
    note: '含空单元格与全空行。导入器须正确处理：空单元格映射为 NULL/空串（依语义），全空行可跳过或保留（须一致）。',
  });

  // 6.5 多表头（模拟 Excel 多行表头）
  write(
    'malformed/multi_header.csv',
    '年份,2024,2025\r\n指标,第一季度,第一季度\r\n营收,100,120\r\n成本,60,70\r\n'
  );
  record({
    category: 'malformed/multi_header',
    files: ['malformed/multi_header.csv'],
    format: ['csv'],
    expected: 'reject-or-define',
    note: '两行表头（多表头）。导入器须检测到非单行表头并提示用户选择表头行，或明确拒绝；不得把第二行当数据。',
  });

  // 6.6 合并单元格（无第三方库时以 CSV 近似表示：合并区首格有值、其余空）
  write(
    'malformed/merged_cells.csv',
    '区域,,,\r\n华北,销量,利润,\r\n,100,20,\r\n华南,销量,利润,\r\n,90,15,\r\n'
  );
  record({
    category: 'malformed/merged_cells',
    files: ['malformed/merged_cells.csv'],
    format: ['csv'],
    expected: 'reject-or-define',
    note: '模拟 Excel 合并单元格：A1 跨三列、A2 跨两列。真实 .xlsx 合并单元格需 xlsx 导入器还原；此处以 CSV 近似。导入器须把合并区还原或明确提示，不得把空单元格当 0/缺失值静默入库。',
  });
}

// ===========================================================================
// 类别 7：编码混用（UTF-8 / GBK / UTF-16 混在同一批文件）
// ===========================================================================
{
  // 7.1 UTF-8
  write('encoding/utf8_sample.csv', 'name,note\r\n张三,这是 UTF-8 编码的中文\r\n');
  record({
    category: 'encoding/utf8',
    files: ['encoding/utf8_sample.csv'],
    format: ['csv'],
    expected: 'accept',
    note: '标准 UTF-8（含 BOM 可选）。导入器应默认识别并接受。',
  });

  // 7.2 GBK（手工构造字节，不依赖第三方库）
  // 字节序列意图："中国中文DeskBase"
  //   中 D6D0  国 B9FA  中 D6D0  文 CEC4  DeskBase = ASCII
  const gbkBytes = Buffer.from([
    0xd6, 0xd0, 0xb9, 0xfa, 0xd6, 0xd0, 0xce, 0xc4,
    0x44, 0x65, 0x73, 0x6b, 0x42, 0x61, 0x73, 0x65,
  ]);
  write('encoding/gbk_sample.csv', gbkBytes);
  record({
    category: 'encoding/gbk',
    files: ['encoding/gbk_sample.csv'],
    format: ['raw'],
    expected: 'accept-or-encoding-error',
    note: '原始 GBK 字节（十六进制：D6D0 B9FA D6D0 CEC4 4465736B42617365），意图文本「中国中文DeskBase」。导入器须识别 GBK 并正确解码；若识别失败，必须给出明确编码错误，绝不允许把 GBK 字节当 UTF-8 静默解码成乱码并当作真实数据入库。',
  });

  // 7.3 UTF-16LE（含 BOM）
  const utf16 = Buffer.concat([
    Buffer.from([0xff, 0xfe]), // BOM
    Buffer.from('name,note\r\n李四,这是 UTF-16 编码的中文\r\n', 'utf16le'),
  ]);
  write('encoding/utf16_sample.csv', utf16);
  record({
    category: 'encoding/utf16',
    files: ['encoding/utf16_sample.csv'],
    format: ['raw'],
    expected: 'accept',
    note: 'UTF-16LE 含 BOM（FF FE）。导入器须依据 BOM 正确解码。',
  });
}

// ===========================================================================
// 类别 8：空值（全空行、全空列、NULL 与空串混用）
// ===========================================================================
{
  table('nulls/null_vs_empty', ['a', 'b', 'note'], [
    ['NULL', '', 'a 列为字面 NULL 标记，b 列为空串'],
    ['', 'text', 'a 列为空串，b 列为文本'],
    ['\\N', 'val', 'a 列为 \\N（PostgreSQL 风格 NULL 标记）'],
    ['0', 'val', 'a 列为数字 0，须区别于空'],
  ], {
    expected: 'accept-with-defined-null',
    note: 'NULL 与空串混用。导入器须清晰定义并一致处理：字面 NULL/\\N 视为 NULL，空单元格视为空串或 NULL（须文档化），数字 0 不得被当作空。',
  });

  // 全空行 / 全空列单独成文件
  write('nulls/empty_rows.csv', 'a,b,c\r\n1,2,3\r\n\r\n\r\n4,5,6\r\n');
  write('nulls/empty_cols.csv', 'a,b,c\r\n1,,3\r\n2,,4\r\n3,,5\r\n');
  record({
    category: 'nulls/empty_rows',
    files: ['nulls/empty_rows.csv'],
    format: ['csv'],
    expected: 'accept',
    note: '全空行。导入器须一致处理（默认跳过或保留为全 NULL 行），不得崩溃。',
  });
  record({
    category: 'nulls/empty_cols',
    files: ['nulls/empty_cols.csv'],
    format: ['csv'],
    expected: 'accept',
    note: '全空列。导入器须保留空列（或明确提示），不得把后续列错位。',
  });
}

// ===========================================================================
// 类别 9：Unicode 边界（U+FFFF、代理对、零宽字符）
// ===========================================================================
{
  table('unicode/uFFFF', ['char', 'codepoint', 'note'], [
    ['\uFFFF', 'U+FFFF', '非字符（noncharacter），部分系统拒绝存储'],
  ], {
    expected: 'accept-or-encoding-error',
    note: 'U+FFFF 是非字符码点。导入器须明确处理（接受或拒绝），不得因码点非法导致崩溃或数据错位。',
  });

  table('unicode/surrogate_pair', ['char', 'codepoint', 'note'], [
    ['\u{1F600}', 'U+1F600', '有效代理对（笑脸 emoji）'],
    ['\u{1F984}', 'U+1F984', '有效代理对（独角兽）'],
  ], {
    expected: 'accept',
    note: '有效代理对须作为整体存储，不得被拆成独立的孤代理（U+D800–U+DFFF 单独出现应报错）。',
  });

  table('unicode/zero_width', ['char', 'codepoint', 'note'], [
    ['\u200B', 'U+200B', '零宽空格 ZWSP'],
    ['\uFEFF', 'U+FEFF', '零宽不换行空格（BOM 兼）'],
    ['\u2060', 'U+2060', '词连接符 WORD JOINER'],
    ['\u00AD', 'U+00AD', '软连字符'],
  ], {
    expected: 'accept',
    note: '零宽字符须按原样保留；在显示 / 搜索时应意识到其存在，不得因不可见而丢失或引发长度计算错误。',
  });
}

// ===========================================================================
// 类别 10：数字边界（极大整数、极小负数、NaN、Infinity、科学计数法）
// ===========================================================================
{
  table('numbers/numeric_bounds', ['value', 'kind', 'note'], [
    ['999999999999999999999999', 'huge-int', '24 位整数，超出 64 位有符号范围'],
    ['-999999999999999999999999', 'tiny-neg', '极小负数，超出 64 位有符号范围'],
    ['NaN', 'nan', '非数字'],
    ['Infinity', 'inf', '正无穷'],
    ['-Infinity', 'neg-inf', '负无穷'],
    ['1e308', 'sci', '科学计数法（极大）'],
    ['1e-320', 'sci-small', '科学计数法（极小）'],
    ['1.7976931348623157e+308', 'max-float', '逼近 IEEE754 双精度上限'],
  ], {
    expected: 'accept-strict-text',
    note: '数字边界值。整数列须拒绝超出范围的值并明确报错；文本列须原样接受；NaN/Infinity 不得被静默写入数值列（否则破坏聚合与排序）。',
  });
}

// ---- 写出 manifest ---------------------------------------------------------
const manifestObj = {
  generatedAt: new Date().toISOString(),
  source: 'docs/19-test-strategy.md §4.8',
  node: process.version,
  expectedVocabulary: {
    accept: '应被正确接受并完整保留（含全部字节 / 字符）。',
    reject: '应被明确拒绝并给出清晰错误提示，不得静默吞掉。',
    escape: '注入内容应作为普通文本安全保存 / 展示，绝不被执行。',
    'accept-strict-text': '接受但统一按文本存储，不擅自推断为危险类型，不崩溃。',
    'reject-or-define': '格式异常，要么明确拒绝并标出错误行，要么按预设规则处理。',
    'accept-with-defined-null': '接受，但 NULL 与空串语义必须由导入器清晰定义并一致处理。',
    'accept-or-encoding-error': '编码相关，识别成功则正确接受；识别失败给明确编码错误，绝不允许把 GBK 字节当 UTF-8 静默解码成乱码并入库。',
  },
  categories: manifest,
  summary: {
    fileCount,
    totalBytes,
  },
};
write('manifest.json', JSON.stringify(manifestObj, null, 2));

// ---- 控制台摘要 ------------------------------------------------------------
console.log('脏数据语料生成完成');
console.log(`  输出目录 : ${OUT}`);
console.log(`  文件数量 : ${fileCount}`);
console.log(`  总大小   : ${(totalBytes / 1024 / 1024).toFixed(2)} MB (${totalBytes} 字节)`);
console.log(`  类别数量 : ${manifest.length}`);
