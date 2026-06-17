import { chmodSync, existsSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

if (process.platform === 'win32') {
  process.exit(0);
}

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const binDir = path.join(__dirname, '..', 'bin');

if (!existsSync(binDir)) {
  process.exit(0);
}

let madeExecutable = 0;
try {
  for (const file of readdirSync(binDir)) {
    if (file.endsWith('.exe')) continue;
    const filePath = path.join(binDir, file);
    chmodSync(filePath, 0o755);
    madeExecutable++;
  }
} catch (err) {
  console.warn(`postinstall: failed to chmod binaries: ${err.message}`);
}

if (madeExecutable > 0) {
  console.log(`postinstall: made ${madeExecutable} binary(s) executable`);
}