const fs = require('fs');
const file = process.argv[2];
const text = fs.readFileSync(file, 'utf8');
const needles = process.argv.slice(3);
for (const needle of needles) {
  console.log('\n==================== NEEDLE:', needle, '====================');
  let idx = 0, count = 0;
  while (true) {
    const m = text.indexOf(needle, idx);
    if (m === -1) break;
    count++;
    if (count > 60) { console.log('... (truncated at 60)'); break; }
    const start = Math.max(0, m - 220);
    const snippet = text.slice(start, m + 320).replace(/\n/g, ' ');
    console.log('--- @' + m + ' ---');
    console.log(snippet);
    idx = m + needle.length;
  }
  if (count === 0) console.log('(no match)');
}
