import assert from 'node:assert/strict';
import { test } from 'node:test';
import { keyEvents, quitEvents } from './chat-ui-keys.mjs';

test('Darwin key adapter preserves proven native events exactly', () => {
  assert.deepEqual(keyEvents('a,KeyA,4', 'darwin'), [
    {type:'keyDown',key:'a',code:'KeyA',modifiers:4},
    {type:'keyUp',key:'a',code:'KeyA',modifiers:4},
  ]);
  assert.deepEqual(quitEvents('darwin'), [{type:'keyDown',key:'q',code:'KeyQ',modifiers:4,windowsVirtualKeyCode:81}]);
});

test('Linux and Windows use Ctrl and normal final-window close', () => {
  for (const platform of ['linux','win32']) {
    assert.equal(keyEvents('a,KeyA,4',platform)[0].modifiers,2);
    assert.equal(keyEvents('Enter,Enter',platform)[0].windowsVirtualKeyCode,13);
    assert.deepEqual(quitEvents(platform),[
      {type:'keyDown',key:'w',code:'KeyW',modifiers:10,windowsVirtualKeyCode:87},
      {type:'keyUp',key:'w',code:'KeyW',modifiers:10},
    ]);
  }
});
