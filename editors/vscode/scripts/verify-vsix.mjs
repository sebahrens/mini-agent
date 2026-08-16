import { readFileSync } from 'node:fs';
import { unzipSync } from 'fflate';
import { TARGETS } from './platform.mjs';

export function verifyVsix(vsixPath, target) {
  const targetInfo = TARGETS[target];
  if (!targetInfo) { throw new Error(`Unsupported VS Code target: ${target}`); }
  const archive = unzipSync(new Uint8Array(readFileSync(vsixPath)));
  const files = Object.keys(archive).filter(name => !name.endsWith('/')).sort();
  const expected = [
    '[Content_Types].xml',
    'extension.vsixmanifest',
    'extension/LICENSE.txt',
    'extension/SOURCE.md',
    'extension/THIRD_PARTY_LICENSES.md',
    'extension/THIRD_PARTY_APACHE_LICENSE.txt',
    `extension/bin/${target}/${targetInfo.binary}`,
    'extension/dist/extension.js',
    'extension/package.json',
  ].sort();
  if (JSON.stringify(files) !== JSON.stringify(expected)) {
    throw new Error(`Unexpected VSIX contents for ${target}:\n${files.join('\n')}`);
  }
  return files;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const [, , vsixPath, target] = process.argv;
  verifyVsix(vsixPath, target);
  console.log(`Verified ${vsixPath} for ${target}`);
}
