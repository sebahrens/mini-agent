import { describe, expect, it } from 'vitest';
// The production packaging scripts are plain ESM so CI can run them without transpilation.
// @ts-expect-error JavaScript packaging module intentionally has no declaration file.
import { inspectBinary, verifyBinary } from '../scripts/platform.mjs';

function elf(machine: number): Uint8Array {
  const bytes = new Uint8Array(64);
  bytes.set([0x7f, 0x45, 0x4c, 0x46, 2, 1]);
  new DataView(bytes.buffer).setUint16(18, machine, true);
  return bytes;
}

function macho(cpu: number): Uint8Array {
  const bytes = new Uint8Array(32);
  bytes.set([0xcf, 0xfa, 0xed, 0xfe]);
  new DataView(bytes.buffer).setUint32(4, cpu, true);
  return bytes;
}

function pe(machine: number): Uint8Array {
  const bytes = new Uint8Array(128);
  bytes.set([0x4d, 0x5a]);
  new DataView(bytes.buffer).setUint32(0x3c, 64, true);
  bytes.set([0x50, 0x45, 0, 0], 64);
  new DataView(bytes.buffer).setUint16(68, machine, true);
  return bytes;
}

describe('platform binary verification', () => {
  it('identifies supported native formats', () => {
    expect(inspectBinary(elf(62))).toEqual({ format: 'elf', arch: 'x64' });
    expect(inspectBinary(elf(183))).toEqual({ format: 'elf', arch: 'arm64' });
    expect(inspectBinary(macho(0x0100000c))).toEqual({ format: 'macho', arch: 'arm64' });
    expect(inspectBinary(pe(0x8664))).toEqual({ format: 'pe', arch: 'x64' });
  });

  it('rejects a wrong-architecture binary', () => {
    expect(() => verifyBinary(elf(183), 'linux-x64')).toThrow(/Wrong binary/);
  });
});
