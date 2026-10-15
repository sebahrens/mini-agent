// The native UI fixture uses supported VS Code keyboard actions, not session exports.
export function keyEvents(argument, platform = process.platform) {
  const [key, code, rawModifiers = '0'] = argument.split(',');
  const modifiers = platform !== 'darwin' && Number(rawModifiers) === 4 ? 2 : Number(rawModifiers);
  const enter = key === 'Enter' ? { windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13 } : {};
  return [
    { type: 'keyDown', key, code, modifiers, ...enter },
    { type: 'keyUp', key, code, modifiers },
  ];
}

export function quitEvents(platform = process.platform) {
  // Closing the final native window quits VS Code on Linux/Windows; macOS
  // keeps windowless applications alive, so preserve the proven Cmd-Q path.
  return platform === 'darwin'
    ? [{ type: 'keyDown', key: 'q', code: 'KeyQ', modifiers: 4, windowsVirtualKeyCode: 81 }]
    : [
      { type: 'keyDown', key: 'w', code: 'KeyW', modifiers: 10, windowsVirtualKeyCode: 87 },
      { type: 'keyUp', key: 'w', code: 'KeyW', modifiers: 10 },
    ];
}
