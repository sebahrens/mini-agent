import * as vscode from 'vscode';
import * as cp from 'child_process';
import { log } from './log';

// ACP framing: newline-delimited JSON over stdout.
// Stderr is diagnostic-only and routed to the output channel.

type SessionState = 'stopped' | 'starting' | 'running' | 'stopping';

export class AgentSession {
  private proc: cp.ChildProcess | undefined;
  private state: SessionState = 'stopped';
  private readonly statusBar: vscode.StatusBarItem;

  constructor(
    private readonly executablePath: string,
    private readonly folder: vscode.WorkspaceFolder,
    private readonly context: vscode.ExtensionContext,
  ) {
    this.statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    this.statusBar.command = 'mini-agent.stop';
    this.context.subscriptions.push(this.statusBar);
  }

  async start(): Promise<void> {
    if (this.state !== 'stopped') { return; }

    this.setState('starting');

    // Verify the executable before spawning.
    if (!await this.verify()) {
      this.setState('stopped');
      return;
    }

    const args = ['--acp'];
    const cwd = this.folder.uri.fsPath;

    log.info(`Spawning mini-agent: ${this.executablePath} ${args.join(' ')} (cwd: ${cwd})`);

    this.proc = cp.spawn(this.executablePath, args, {
      shell: false,
      cwd,
      stdio: ['pipe', 'pipe', 'pipe'],
    });

    this.proc.on('error', err => this.onError(err));
    this.proc.on('exit', (code, signal) => this.onExit(code, signal));

    this.proc.stderr?.on('data', (chunk: Buffer) => {
      for (const line of chunk.toString().split('\n').filter(Boolean)) {
        log.debug(`[stderr] ${line}`);
      }
    });

    this.proc.stdout?.on('data', (chunk: Buffer) => {
      this.onStdout(chunk);
    });

    this.setState('running');
  }

  async stop(): Promise<void> {
    if (this.state === 'stopped' || this.state === 'stopping') { return; }
    this.setState('stopping');

    const proc = this.proc;
    if (!proc || proc.exitCode !== null) {
      this.setState('stopped');
      return;
    }

    await new Promise<void>(resolve => {
      const timer = setTimeout(() => {
        log.warn('mini-agent did not exit in time; sending SIGKILL');
        proc.kill('SIGKILL');
      }, 5000);

      proc.once('exit', () => {
        clearTimeout(timer);
        resolve();
      });

      proc.kill('SIGTERM');
    });

    this.proc = undefined;
    this.setState('stopped');
  }

  private onStdout(chunk: Buffer): void {
    // ACP frames are newline-delimited JSON. Buffer and split by newline.
    for (const line of chunk.toString().split('\n').filter(Boolean)) {
      log.trace(`[stdout] ${line}`);
      // TODO(ny65.7): parse ACP frames and forward to Chat Participant
    }
  }

  private onError(err: Error): void {
    log.error(`mini-agent process error: ${err.message}`);
    void vscode.window.showErrorMessage(`Mini Agent process error: ${err.message}`);
    this.proc = undefined;
    this.setState('stopped');
  }

  private onExit(code: number | null, signal: NodeJS.Signals | null): void {
    if (this.state !== 'stopping') {
      log.warn(`mini-agent exited unexpectedly: code=${code ?? 'null'} signal=${signal ?? 'none'}`);
      void vscode.window.showWarningMessage(
        `Mini Agent exited unexpectedly (code ${code ?? signal}). Use "Mini Agent: Restart Session" to recover.`,
        'Restart',
      ).then(choice => {
        if (choice === 'Restart') {
          void vscode.commands.executeCommand('mini-agent.restart');
        }
      });
    } else {
      log.info(`mini-agent exited cleanly: code=${code ?? 'null'} signal=${signal ?? 'none'}`);
    }
    this.proc = undefined;
    this.setState('stopped');
  }

  private async verify(): Promise<boolean> {
    return new Promise(resolve => {
      const probe = cp.spawn(this.executablePath, ['--version'], {
        shell: false,
        timeout: 5000,
      });

      let stdout = '';
      probe.stdout?.on('data', (d: Buffer) => { stdout += d.toString(); });

      probe.on('error', err => {
        log.error(`mini-agent --version failed: ${err.message}`);
        void vscode.window.showErrorMessage(
          `Cannot start Mini Agent: executable not found or not runnable.\n${err.message}`,
        );
        resolve(false);
      });

      probe.on('exit', code => {
        if (code !== 0) {
          log.error(`mini-agent --version exited with code ${code}`);
          void vscode.window.showErrorMessage(
            `Mini Agent executable at "${this.executablePath}" does not appear to be a valid mini-agent binary (--version failed with code ${code}).`,
          );
          resolve(false);
          return;
        }
        log.info(`mini-agent version: ${stdout.trim()}`);
        resolve(true);
      });
    });
  }

  private setState(state: SessionState): void {
    this.state = state;
    switch (state) {
      case 'stopped':
        this.statusBar.text = '$(circle-slash) Mini Agent';
        this.statusBar.tooltip = 'Mini Agent stopped — click to stop';
        this.statusBar.hide();
        break;
      case 'starting':
        this.statusBar.text = '$(loading~spin) Mini Agent';
        this.statusBar.tooltip = 'Mini Agent starting…';
        this.statusBar.show();
        break;
      case 'running':
        this.statusBar.text = '$(circle-filled) Mini Agent';
        this.statusBar.tooltip = `Mini Agent running (${this.folder.name}) — click to stop`;
        this.statusBar.show();
        break;
      case 'stopping':
        this.statusBar.text = '$(loading~spin) Mini Agent';
        this.statusBar.tooltip = 'Mini Agent stopping…';
        break;
    }
  }
}
