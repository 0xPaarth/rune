#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { chmodSync, existsSync, readFileSync, unlinkSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { homedir } from 'node:os';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const BIN_DIR = path.join(__dirname, '..', 'bin');
const DAEMON_PATH = path.join(__dirname, '..', 'daemon', 'server.js');
const RUNE_DIR = path.join(homedir(), '.rune');
const PID_PATH = path.join(RUNE_DIR, 'daemon.pid');
const HEALTH_URL = 'http://127.0.0.1:9191/health';

function getBinaryName() {
  const { platform, arch } = process;
  if (platform === 'linux' && arch === 'x64') return 'rune-linux-x64';
  if (platform === 'darwin' && arch === 'arm64') return 'rune-darwin-arm64';
  if (platform === 'darwin' && arch === 'x64') return 'rune-darwin-x64';
  if (platform === 'win32' && arch === 'x64') return 'rune-win32-x64.exe';
  throw new Error(
    `Unsupported platform: ${platform} ${arch}. ` +
    'Supported: linux-x64, darwin-arm64, darwin-x64, win32-x64.'
  );
}

function binaryPath() {
  return path.join(BIN_DIR, getBinaryName());
}

async function isDaemonRunning() {
  try {
    const res = await fetch(HEALTH_URL, { signal: AbortSignal.timeout(2000) });
    return res.ok;
  } catch {
    return false;
  }
}

function startDaemon() {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [DAEMON_PATH], {
      detached: true,
      stdio: 'ignore',
      cwd: path.join(__dirname, '..'),
    });

    child.unref();

    let attempts = 0;
    const interval = setInterval(async () => {
      attempts++;
      if (await isDaemonRunning()) {
        clearInterval(interval);
        clearTimeout(timeout);
        resolve();
      } else if (attempts >= 30) {
        clearInterval(interval);
        clearTimeout(timeout);
        reject(new Error('Daemon did not start within 3 seconds'));
      }
    }, 100);

    const timeout = setTimeout(() => {
      clearInterval(interval);
      reject(new Error('Daemon did not start within 3 seconds'));
    }, 3000);
  });
}

function runTui() {
  const bin = binaryPath();
  if (!existsSync(bin)) {
    console.error(`Error: TUI binary not found at ${bin}`);
    console.error('The package may not be properly installed. Try reinstalling.');
    process.exit(1);
  }

  const child = spawn(bin, [], { stdio: 'inherit' });
  child.on('exit', (code) => process.exit(code ?? 1));
}

async function daemonStatus() {
  if (await isDaemonRunning()) {
    console.log('Rune daemon running');
    process.exit(0);
  }
  console.log('Rune daemon stopped');
  process.exit(1);
}

function daemonStop() {
  if (!existsSync(PID_PATH)) {
    console.log('No daemon PID file found. Daemon is not running.');
    process.exit(0);
  }

  let pid;
  try {
    pid = parseInt(readFileSync(PID_PATH, 'utf8').trim(), 10);
  } catch {
    console.error('Failed to read daemon PID file.');
    process.exit(1);
  }

  if (isNaN(pid)) {
    console.error('Invalid PID in daemon PID file.');
    try { unlinkSync(PID_PATH); } catch {}
    process.exit(1);
  }

  try {
    if (process.platform === 'win32') {
      spawn('taskkill', ['/PID', String(pid), '/F'], { stdio: 'ignore' });
    } else {
      process.kill(pid, 'SIGTERM');
    }
    try { unlinkSync(PID_PATH); } catch {}
    console.log('Daemon stopped.');
    process.exit(0);
  } catch (err) {
    if (err.code === 'ESRCH') {
      console.log('Daemon process not found. Removing stale PID file.');
      try { unlinkSync(PID_PATH); } catch {}
      process.exit(0);
    }
    console.error(`Failed to stop daemon: ${err.message}`);
    process.exit(1);
  }
}

const args = process.argv.slice(2);

if (args[0] === 'daemon' && args[1] === 'status') {
  daemonStatus();
} else if (args[0] === 'daemon' && args[1] === 'stop') {
  daemonStop();
} else {
  (async () => {
    try {
      if (!(await isDaemonRunning())) {
        process.stdout.write('Starting daemon... ');
        await startDaemon();
        process.stdout.write('done\n');
      }
      runTui();
    } catch (err) {
      console.error(err.message);
      process.exit(1);
    }
  })();
}