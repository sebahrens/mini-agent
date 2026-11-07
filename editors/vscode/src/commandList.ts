import type * as acp from '@agentclientprotocol/sdk';

/** Commands listed per `/commands` page. */
export const COMMANDS_PAGE_SIZE = 100;

/**
 * Render one page of the server's advertised commands. A list that does not fit
 * says how many are not shown and how to see them; commands the session could
 * not keep (more advertised than the extension stores) are reported separately
 * because `/run` cannot accept them.
 */
export function describeServerCommands(
  commands: readonly acp.AvailableCommand[],
  droppedCommands = 0,
  pageArgument = '',
): string {
  if (commands.length === 0) { return 'The Mini Agent server has not advertised any commands yet.'; }
  const pages = Math.ceil(commands.length / COMMANDS_PAGE_SIZE);
  const requested = Number.parseInt(pageArgument.trim(), 10);
  const page = Number.isFinite(requested) ? Math.min(Math.max(requested, 1), pages) : 1;
  const start = (page - 1) * COMMANDS_PAGE_SIZE;
  const shown = commands.slice(start, start + COMMANDS_PAGE_SIZE);
  const lines = shown.map(command => `- \`/${command.name.replace(/[^\p{L}\p{N}\p{M}_:.-]/gu, '?')}\`: ${command.description.replace(/[\r\n]+/gu, ' ').slice(0, 200)}`);
  const notes: string[] = [];
  if (commands.length > shown.length) {
    const hidden = commands.length - shown.length;
    notes.push(`Showing commands ${start + 1}-${start + shown.length} of ${commands.length}; ${hidden} more command${hidden === 1 ? '' : 's'} not shown on this page.${page < pages ? ` Use \`/commands ${page + 1}\` for the next page.` : ' Use `/commands 1` to go back to the first page.'}`);
  }
  if (droppedCommands > 0) {
    notes.push(`${droppedCommands} more command${droppedCommands === 1 ? '' : 's'} advertised by the server could not be kept (the extension stores at most ${commands.length}) and cannot be run with \`/run\`.`);
  }
  return `Server commands (run one with \`/run <command>\`):\n\n${lines.join('\n')}\n${notes.length ? `\n${notes.join('\n')}\n` : ''}`;
}
