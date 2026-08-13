const fs = require('fs');
const asarPath = process.argv[2];
const outDir = process.argv[3];
const buf = fs.readFileSync(asarPath);
const jsonStart = 16;
const jsonLen = buf.readUInt32LE(12); // 1565588
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
function readFile(targetPath) {
  const e = find(header, targetPath);
  if (!e) return null;
  const off = parseInt(e.offset, 10);
  return buf.slice(dataStart + off, dataStart + off + e.size);
}

const test = readFile('.vite/build/avatar-overlay-composition-surface-preload.js');
fs.mkdirSync(outDir, {recursive:true});
fs.writeFileSync(outDir + '/__test_head.txt', test.slice(0,200).toString('utf8'));

const results = [];
function walk(files, prefix) {
  for (const [name, v] of Object.entries(files)) {
    const path = prefix ? prefix + '/' + name : name;
    if (v.files) walk(v.files, path);
    else results.push({path, size: v.size, offset: parseInt(v.offset,10)});
  }
}
walk(header.files, '');
fs.writeFileSync(outDir + '/__file_list.json', JSON.stringify({dataStart, jsonLen, totalFiles: results.length, files: results}, null, 2));
console.log('dataStart', dataStart, 'jsonLen', jsonLen, 'totalFiles', results.length);
console.log('test head:', JSON.stringify(test.slice(0,80).toString('utf8')));
