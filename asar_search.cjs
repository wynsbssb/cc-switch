const fs = require('fs');
const asarPath = process.argv[2];
const buf = fs.readFileSync(asarPath);
const jsonStart = 16;
const jsonLen = buf.readUInt32LE(12);
const header = JSON.parse(buf.slice(jsonStart, jsonStart + jsonLen).toString('utf8'));
const dataStart = (jsonStart + jsonLen + 3) & ~3;

function walk(files, prefix, out) {
  for (const [name, v] of Object.entries(files)) {
    const path = prefix ? prefix + '/' + name : name;
    if (v.files) walk(v.files, path, out);
    else out.push({path, size: v.size, offset: parseInt(v.offset,10)});
  }
}
const files = [];
walk(header.files, '', files);

const needles = process.argv.slice(3);
const needleSet = needles.map((n, i) => ({re: new RegExp(n, 'g'), name: n}));

let matches = [];
for (const f of files) {
  if (!/\.(js|mjs|cjs|html|json)$/i.test(f.path)) continue;
  const text = buf.slice(dataStart + f.offset, dataStart + f.offset + f.size).toString('utf8');
  for (const n of needleSet) {
    // reset lastIndex
    n.re.lastIndex = 0;
    let m;
    while ((m = n.re.exec(text)) !== null) {
      const start = Math.max(0, m.index - 80);
      const snippet = text.slice(start, m.index + 120).replace(/\s+/g, ' ');
      matches.push({path: f.path, needle: n.name, index: m.index, snippet});
      if (matches.length >= 300) break;
    }
    if (matches.length >= 300) break;
  }
  if (matches.length >= 300) break;
}
for (const m of matches) {
  console.log('\n### ' + m.path + ' [' + m.needle + ' @ ' + m.index + ']');
  console.log(m.snippet);
}
console.log('\nTOTAL matches shown:', matches.length);
