export const TARGETS = Object.freeze({
  'win32-x64': { format: 'pe', arch: 'x64', binary: 'mini-agent.exe' },
  'darwin-x64': { format: 'macho', arch: 'x64', binary: 'mini-agent' },
  'darwin-arm64': { format: 'macho', arch: 'arm64', binary: 'mini-agent' },
  'linux-x64': { format: 'elf', arch: 'x64', binary: 'mini-agent' },
  'linux-arm64': { format: 'elf', arch: 'arm64', binary: 'mini-agent' },
});

export function inspectBinary(bytes) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (bytes.length >= 20 && bytes[0] === 0x7f && bytes[1] === 0x45 && bytes[2] === 0x4c && bytes[3] === 0x46) {
    const littleEndian = bytes[5] === 1;
    const machine = view.getUint16(18, littleEndian);
    return { format: 'elf', arch: machine === 62 ? 'x64' : machine === 183 ? 'arm64' : `machine-${machine}` };
  }
  if (bytes.length >= 8 && bytes[0] === 0xcf && bytes[1] === 0xfa && bytes[2] === 0xed && bytes[3] === 0xfe) {
    const cpu = view.getUint32(4, true);
    return { format: 'macho', arch: cpu === 0x01000007 ? 'x64' : cpu === 0x0100000c ? 'arm64' : `cpu-${cpu}` };
  }
  if (bytes.length >= 64 && bytes[0] === 0x4d && bytes[1] === 0x5a) {
    const peOffset = view.getUint32(0x3c, true);
    if (peOffset + 6 <= bytes.length
      && bytes[peOffset] === 0x50 && bytes[peOffset + 1] === 0x45
      && bytes[peOffset + 2] === 0 && bytes[peOffset + 3] === 0) {
      const machine = view.getUint16(peOffset + 4, true);
      return { format: 'pe', arch: machine === 0x8664 ? 'x64' : machine === 0xaa64 ? 'arm64' : `machine-${machine}` };
    }
  }
  return { format: 'unknown', arch: 'unknown' };
}

export function verifyBinary(bytes, target) {
  const expected = TARGETS[target];
  if (!expected) { throw new Error(`Unsupported VS Code target: ${target}`); }
  const actual = inspectBinary(bytes);
  if (actual.format !== expected.format || actual.arch !== expected.arch) {
    throw new Error(`Wrong binary for ${target}: expected ${expected.format}/${expected.arch}, got ${actual.format}/${actual.arch}`);
  }
}
