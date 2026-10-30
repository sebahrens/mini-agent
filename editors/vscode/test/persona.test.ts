import { describe, expect, it, vi } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';
import { PERSONA_CONFIG_ID, personaSelector } from '../src/persona';
import { SessionUpdateRouter } from '../src/protocolSession';

function personaOption(current: string, values: string[]): acp.SessionConfigOption {
  return {
    id: PERSONA_CONFIG_ID,
    name: 'Persona',
    type: 'select',
    currentValue: current,
    options: values.map(value => ({ value, name: `Name ${value}`, description: `${value} route` })),
  };
}

describe('persona selector', () => {
  it('reads the mode select option and flattens groups', () => {
    expect(personaSelector([personaOption('general', ['general', 'software-reviewer'])])).toEqual({
      current: 'general',
      choices: [
        { value: 'general', name: 'Name general', description: 'general route' },
        { value: 'software-reviewer', name: 'Name software-reviewer', description: 'software-reviewer route' },
      ],
    });
    const grouped: acp.SessionConfigOption = {
      id: PERSONA_CONFIG_ID, name: 'Persona', type: 'select', currentValue: 'b',
      options: [{ group: 'g', name: 'Group', options: [{ value: 'a', name: 'A' }, { value: 'b', name: 'B' }] }],
    };
    expect(personaSelector([grouped])?.choices.map(choice => choice.value)).toEqual(['a', 'b']);
  });

  it('offers nothing without a select persona option and bounds the choices', () => {
    expect(personaSelector(undefined)).toBeUndefined();
    expect(personaSelector([])).toBeUndefined();
    expect(personaSelector([{ id: PERSONA_CONFIG_ID, name: 'Persona', type: 'boolean', currentValue: true }])).toBeUndefined();
    expect(personaSelector([{ ...personaOption('x', ['x']), id: 'other' }])).toBeUndefined();
    const many = Array.from({ length: 100 }, (_, index) => `p${index}`);
    expect(personaSelector([personaOption('p0', many)])?.choices).toHaveLength(64);
  });

  it('tracks advertised options and selects a persona over the public RPC', async () => {
    const router = new SessionUpdateRouter();
    router.begin();
    const request = vi.fn(async () => ({ configOptions: [personaOption('software-reviewer', ['general', 'software-reviewer'])] }));
    const client = { request, notify: vi.fn(async () => undefined) } as unknown as acp.ClientContext;
    const session = router.finish(client, 'live', [personaOption('general', ['general', 'software-reviewer'])]);
    expect(personaSelector(session.configOptions())?.current).toBe('general');

    await session.setConfigOption(PERSONA_CONFIG_ID, 'software-reviewer');
    expect(request).toHaveBeenCalledWith('session/set_config_option',
      { sessionId: 'live', configId: PERSONA_CONFIG_ID, value: 'software-reviewer' }, expect.anything());
    expect(personaSelector(session.configOptions())?.current).toBe('software-reviewer');

    router.receive({ sessionId: 'live', update: { sessionUpdate: 'config_option_update', configOptions: [personaOption('general', ['general'])] } });
    expect(personaSelector(session.configOptions())?.current).toBe('general');
    router.receive({ sessionId: 'foreign', update: { sessionUpdate: 'config_option_update', configOptions: [] } });
    expect(personaSelector(session.configOptions())?.current).toBe('general');
  });

  it('refuses a persona switch while a prompt is active', async () => {
    const router = new SessionUpdateRouter();
    router.begin();
    let release!: (value: acp.PromptResponse) => void;
    const pending = new Promise<acp.PromptResponse>(resolve => { release = resolve; });
    const request = vi.fn((method: string) => (method === 'session/prompt' ? pending : Promise.resolve({ configOptions: [] })));
    const session = router.finish({ request, notify: vi.fn(async () => undefined) } as unknown as acp.ClientContext, 'live');
    const turn = session.prompt('busy', vi.fn(), new AbortController().signal);
    await expect(session.setConfigOption(PERSONA_CONFIG_ID, 'general')).rejects.toThrow('busy');
    release({ stopReason: 'end_turn' });
    await turn;
    expect(request.mock.calls.some(call => call[0] === 'session/set_config_option')).toBe(false);
  });
});
