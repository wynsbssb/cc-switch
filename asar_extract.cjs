const fs = require('fs');
const asarPath = process.argv[2];
const outPath = process.argv[3];
const target = process.argv[4];
const buf = fs.readFileSync(asarPath);
const jsonStart = 16;
const jsonLen = buf.readUInt32LE(12);
const header = JSON.parse(buf.slice(jsonStart, jsonStart + jsonLen).toString('utf8'));
const dataStart = (jsonStart + jsonLen + 3) & ~3;
function find(header, targetPath) {
  let cur = header.files;
  const parts = targetPath.split('/').filter(Boolean);
  for (let i=0;i<parts.length;i++) {
    const part = parts[i];
    if (!cur[part]) return null;
    if (i === parts.length-1) return cur[part];
    cur = cur[part].files;
  }
  return null;
}
const e = find(header, target);
if (!e) { console.error('not found', target); process.exit(1); }
const off = parseInt(e.offset, 10);
fs.writeFileSync(outPath, buf.slice(dataStart + off, dataStart + off + e.size));
console.log('wrote', outPath, e.size);
