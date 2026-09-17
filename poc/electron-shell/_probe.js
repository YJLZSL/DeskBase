const fs = require('fs');
const out = [];
out.push('versions.electron: ' + (process.versions.electron || 'UNDEFINED'));
out.push('versions.node: ' + process.versions.node);
try {
  const e = require('electron');
  out.push('typeof require(electron): ' + typeof e);
  out.push('is_string: ' + (typeof e === 'string'));
  if (typeof e === 'object') {
    out.push('keys: ' + Object.keys(e).join(','));
  }
} catch (err) {
  out.push('require(electron) threw: ' + err.message);
}
fs.writeFileSync('C:/Users/you/AppData/Local/Temp/poc_probe.log', out.join('\n') + '\n');
process.exit(0);
