'use strict';
// Invoked as: node fig_dump.js <build-dir> <out-dir>
// Requires each compiled .js spec, JSON-serialises it (replacing functions
// with the sentinel string "__FN__"), and writes one .json file per spec.
const fs = require('fs');
const path = require('path');

const buildDir = path.resolve(process.argv[2]);
const outDir = path.resolve(process.argv[3]);

fs.mkdirSync(outDir, { recursive: true });

let written = 0;
let skipped = 0;

for (const f of fs.readdirSync(buildDir).filter(n => n.endsWith('.js'))) {
  let specModule;
  try {
    specModule = require(path.join(buildDir, f));
  } catch (e) {
    skipped++;
    continue;
  }

  const spec = specModule.default || specModule;
  if (!spec || typeof spec !== 'object') {
    skipped++;
    continue;
  }

  // Replace function values with a sentinel so JSON.stringify keeps the key.
  let sanitized;
  try {
    sanitized = JSON.parse(JSON.stringify(spec, (_k, v) =>
      typeof v === 'function' ? '__FN__' : v
    ));
  } catch (e) {
    skipped++;
    continue;
  }

  const outFile = path.join(outDir, f.replace(/\.js$/, '.json'));
  fs.writeFileSync(outFile, JSON.stringify(sanitized));
  written++;
}

process.stderr.write('fig_dump: wrote ' + written + ' specs, skipped ' + skipped + '\n');
