import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { TARGETS } from './platform.mjs';

const npmCli = process.env.npm_execpath;
if (!npmCli) {
  throw new Error('npm_execpath is required; run this script through npm run sbom');
}

const output = resolve('dist/mini-agent-vscode.cdx.json');
mkdirSync(dirname(output), { recursive: true });
const rawSbom = execFileSync(process.execPath, [npmCli, 'sbom', '--sbom-format', 'cyclonedx', '--omit=dev'], {
  encoding: 'utf8',
});
const manifest = JSON.parse(readFileSync(resolve('package.json'), 'utf8'));
const sbom = JSON.parse(rawSbom);
const requestedTarget = process.argv[2] ?? process.env.VSCODE_TARGET;
const candidates = requestedTarget
  ? [requestedTarget]
  : Object.keys(TARGETS).filter(target => (
    existsSync(resolve('bin', target, TARGETS[target].binary))
  ));
if (candidates.length !== 1 || !TARGETS[candidates[0]]) {
  throw new Error('Pass exactly one VS Code target when generating a platform SBOM.');
}
const target = candidates[0];
const binary = readFileSync(resolve('bin', target, TARGETS[target].binary));
const binaryRef = `mini-agent-native@${manifest.version}#${target}`;
const nativeComponent = {
  'bom-ref': binaryRef,
  type: 'application',
  name: 'mini-agent-native',
  version: manifest.version,
  hashes: [{ alg: 'SHA-256', content: createHash('sha256').update(binary).digest('hex') }],
  licenses: [{ license: { id: manifest.license } }],
  properties: [{ name: 'vscode:target', value: target }],
  externalReferences: [{ type: 'vcs', url: manifest.repository.url }],
};
delete sbom.serialNumber;
sbom.metadata.timestamp = new Date(315_532_800_000).toISOString();
sbom.metadata.component.name = manifest.name;
sbom.metadata.component.type = 'application';
sbom.components.push(nativeComponent);
const rootDependency = sbom.dependencies.find(dependency => (
  dependency.ref === sbom.metadata.component['bom-ref']
));
if (!rootDependency) { throw new Error('npm SBOM did not include the root component dependency.'); }
rootDependency.dependsOn.push(binaryRef);
writeFileSync(output, `${JSON.stringify(sbom, null, 2)}\n`, 'utf8');
console.log(output);
