import { execFileSync } from 'node:child_process';
import { chmodSync, readFileSync, readdirSync, rmSync, unlinkSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { createVSIX } from '@vscode/vsce';
import { TARGETS, verifyBinary } from './platform.mjs';
import { verifyVsix } from './verify-vsix.mjs';

const target = process.argv[2];
const targetInfo = TARGETS[target];
if (!targetInfo) {
  throw new Error(`Usage: node scripts/package-target.mjs <${Object.keys(TARGETS).join('|')}>`);
}

const cwd = process.cwd();
const manifest = JSON.parse(readFileSync(resolve(cwd, 'package.json'), 'utf8'));
const cargo = readFileSync(resolve(cwd, '../../Cargo.toml'), 'utf8');
const cargoVersion = cargo.match(/^version = "([^"]+)"/m)?.[1];
if (manifest.version !== cargoVersion) {
  throw new Error(`Version mismatch: extension ${manifest.version}, Cargo ${cargoVersion ?? 'missing'}`);
}

const binaryPath = resolve(cwd, 'bin', target, targetInfo.binary);
const binary = readFileSync(binaryPath);
verifyBinary(binary, target);
if (targetInfo.format !== 'pe') { chmodSync(binaryPath, 0o755); }
const help = execFileSync(binaryPath, ['--help'], { encoding: 'utf8' });
if (!/^\s+--acp(?:\s|$)/m.test(help)) {
  throw new Error(`Bundled ${target} binary does not include the required ACP feature.`);
}

const npmCli = process.env.npm_execpath;
if (!npmCli) { throw new Error('Run through npm so npm_execpath is available'); }
execFileSync(process.execPath, [resolve(cwd, 'scripts', 'smoke-acp.mjs'), binaryPath], {
  cwd,
  stdio: 'inherit',
});
execFileSync(process.execPath, [npmCli, 'run', 'build'], { cwd, stdio: 'inherit' });

const packagePath = resolve(cwd, 'dist', `mini-agent-${manifest.version}-${target}.vsix`);
for (const entry of readdirSync(resolve(cwd, 'dist'))) {
  if (entry.endsWith('.vsix') || entry.endsWith('.cdx.json')) {
    rmSync(resolve(cwd, 'dist', entry), { force: true });
  }
}
process.env.SOURCE_DATE_EPOCH ??= '315532800';
const ignoreFile = resolve(cwd, `.vscodeignore.${target}`);
writeFileSync(ignoreFile, [
  '**',
  '!dist/',
  'dist/**',
  '!dist/extension.js',
  '!package.json',
  '!LICENSE',
  '!SOURCE.md',
  '!THIRD_PARTY_LICENSES.md',
  '!THIRD_PARTY_APACHE_LICENSE.txt',
  '!bin/',
  `!bin/${target}/`,
  `!bin/${target}/${targetInfo.binary}`,
  '',
].join('\n'), 'utf8');
try {
  await createVSIX({
    cwd,
    packagePath,
    target,
    dependencies: false,
    ignoreFile,
    ignoreOtherTargetFolders: true,
  });
} finally {
  unlinkSync(ignoreFile);
}
verifyVsix(packagePath, target);
console.log(packagePath);
