import type * as acp from '@agentclientprotocol/sdk';

/** mini-agent advertises its persona (prompt plus `[prompt_to_model]` route) under this config id. */
export const PERSONA_CONFIG_ID = 'mode';

/** The model selector (`[quick_models]`) and the reasoning-effort selector. */
export const MODEL_CONFIG_ID = 'model';
export const EFFORT_CONFIG_ID = 'effort';

const MAX_PERSONA_CHOICES = 64;

export interface PersonaChoice {
  readonly value: string;
  readonly name: string;
  readonly description?: string;
}

export interface PersonaSelector {
  readonly current: string;
  readonly choices: readonly PersonaChoice[];
}

/** The session's select option `configId` (the persona by default), or undefined when the agent advertises none. */
export function personaSelector(
  options: readonly acp.SessionConfigOption[] | null | undefined,
  configId: string = PERSONA_CONFIG_ID,
): PersonaSelector | undefined {
  const option = options?.find(candidate => candidate.id === configId);
  if (!option || option.type !== 'select') { return undefined; }
  const flat = option.options.flatMap(entry => ('group' in entry ? entry.options : [entry]));
  const choices = flat.slice(0, MAX_PERSONA_CHOICES).map(choice => ({
    value: choice.value,
    name: choice.name,
    ...(choice.description ? { description: choice.description } : {}),
  }));
  return choices.length > 0 ? { current: option.currentValue, choices } : undefined;
}
