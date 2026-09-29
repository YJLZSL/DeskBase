const fs = require('fs');

// ---------- 1) index.html：图标 + 导航项 + 视图 ----------
{
  const F = 'app/ui/index.html';
  let s = fs.readFileSync(F, 'utf8');

  // 图标：一张纸 + 几行字（单据的样子），风格与现有 20×20 图标一致
  const iconAnchor = '    <symbol id="i-gear" viewBox="0 0 20 20">';
  if (!s.includes(iconAnchor)) { console.error('!! 找不到图标锚点'); process.exit(1); }
  if (!s.includes('id="i-form"')) {
    s = s.split(iconAnchor).join(
      [
        '    <symbol id="i-form" viewBox="0 0 20 20">',
        '      <path d="M5 2.5h7.5L16 6v11.5H5z" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/>',
        '      <path d="M12.2 2.6V6H16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/>',
        '      <path d="M7.5 10h6M7.5 13h6M7.5 16h3.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>',
        '    </symbol>',
        '',
        iconAnchor,
      ].join('\n')
    );
    console.log('ok 图标 i-form');
  }

  // 导航项：放在「表格」后面 —— 单据是表格的出口，逻辑上挨着
  const navAnchor = '      <button class="nav-item" data-target="settings" data-label="设置">';
  if (!s.includes(navAnchor)) { console.error('!! 找不到导航锚点'); process.exit(1); }
  if (!s.includes('data-target="forms"')) {
    s = s.split(navAnchor).join(
      [
        '      <button class="nav-item" data-target="forms" data-label="单据">',
        '        <svg class="ico"><use href="#i-form"/></svg><span>单据</span>',
        '      </button>',
        navAnchor,
      ].join('\n')
    );
    console.log('ok 导航项「单据」');
  }

  // 视图
  const viewAnchor = '      <section class="view" data-view="settings">';
  if (!s.includes(viewAnchor)) { console.error('!! 找不到视图锚点'); process.exit(1); }
  if (!s.includes('data-view="forms"')) {
    const view = [
      '      <section class="view" data-view="forms">',
      '        <!-- 单据编辑器：摆好一张送货单/订货单/收据，然后打印出来。',
      '             它为的是 reference/31 §0.3 说的"最该补的出口" ——',
      '             只有表没有出口，就接不住"给同事用的小东西"那个位置。 -->',
      '        <div class="formswrap">',
      '          <aside class="forms-side">',
      '            <div class="forms-side-head">',
      '              <span class="forms-side-title">单据模板</span>',
      '              <button class="btn btn-ghost db-mini" id="forms-new" type="button"',
      '                      title="从内置的空白模板开始新建">新建空白</button>',
      '            </div>',
      '            <div class="forms-list" id="forms-list"></div>',
      '            <div class="forms-side-foot">',
      '              <button class="btn" id="forms-save" type="button" title="保存（Ctrl+S）">保存</button>',
      '              <button class="btn btn-primary" id="forms-print" type="button"',
      '                      title="生成打印视图并用浏览器打开（Ctrl+P）">打印</button>',
      '              <button class="btn btn-ghost" id="forms-del" type="button">删除</button>',
      '            </div>',
      '          </aside>',
      '          <div class="forms-main">',
      '            <div class="forms-canvas-wrap">',
      '              <div class="forms-sheet" id="forms-sheet"></div>',
      '            </div>',
      '            <aside class="forms-props" id="forms-props"></aside>',
      '          </div>',
      '        </div>',
      '      </section>',
      '',
      viewAnchor,
    ].join('\n');
    s = s.split(viewAnchor).join(view);
    console.log('ok 视图 forms');
  }

  // 脚本挂载
  const jsAnchor = '    <script src="db-onboard.js"></script>';
  if (s.includes(jsAnchor) && !s.includes('forms.js')) {
    s = s.split(jsAnchor).join(jsAnchor + '\n    <script src="forms.js"></script>');
    console.log('ok 挂载 forms.js');
  } else if (!s.includes('forms.js')) {
    console.error('!! 找不到脚本挂载点');
    process.exit(1);
  }

  // 样式挂载
  if (!s.includes('forms.css')) {
    const cssAnchor = '    <link rel="stylesheet" href="db-onboard.css" />';
    if (!s.includes(cssAnchor)) { console.error('!! 找不到样式挂载点'); process.exit(1); }
    s = s.split(cssAnchor).join(cssAnchor + '\n    <link rel="stylesheet" href="forms.css" />');
    console.log('ok 挂载 forms.css');
  }

  fs.writeFileSync(F, s, 'utf8');
}

// ---------- 2) assets.rs 登记 ----------
{
  const F = 'app/src/assets.rs';
  let s = fs.readFileSync(F, 'utf8');
  const anchor = '"db-onboard.js"';
  if (!s.includes(anchor)) { console.error('!! 找不到 assets 锚点'); process.exit(1); }
  // 看看这一行长什么样，照着加两条
  const line = s.split('\n').find((l) => l.includes(anchor));
  console.log('  参考行：' + line.trim().slice(0, 100));
  if (!s.includes('"forms.js"')) {
    const nl = line.replace('"db-onboard.js"', '"forms.js"');
    s = s.replace(line, line + '\n' + nl);
    console.log('ok 登记 forms.js');
  }
  if (!s.includes('"forms.css"')) {
    const cssLine = s.split('\n').find((l) => l.includes('"db-onboard.css"'));
    if (cssLine) {
      const n2 = cssLine.replace('"db-onboard.css"', '"forms.css"');
      s = s.replace(cssLine, cssLine + '\n' + n2);
      console.log('ok 登记 forms.css');
    } else {
      console.log('  ⚠ 没找到 db-onboard.css 行，检查样式是怎么登记的');
    }
  }
  fs.writeFileSync(F, s, 'utf8');
}
console.log('done');
