import Fastify from 'fastify';
import cors from '@fastify/cors';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import path from 'node:path';
import os from 'node:os';
import TurndownService from 'turndown';

const execFileP = promisify(execFile);

const WORKSPACE_ROOT = path.join(os.homedir(), 'cp', 'Codeforces');
const RUNE_DIR = path.join(os.homedir(), '.rune');
const TOKEN_PATH = path.join(RUNE_DIR, 'daemon.token');
const PID_PATH = path.join(RUNE_DIR, 'daemon.pid');

const HOST = '127.0.0.1';
const PORT = 9191;

// 16 random bytes -> 32 hex chars
const TOKEN = crypto.randomBytes(16).toString('hex');

const fastify = Fastify({ logger: true });

await fastify.register(cors, {
  // Allow any extension origin; the Bearer token is the real gate.
  origin: (origin, cb) => {
    if (!origin || origin.startsWith('chrome-extension://')) {
      cb(null, true);
      return;
    }
    cb(new Error('Origin not allowed'), false);
  },
});

fastify.addHook('onRequest', async (request, reply) => {
  if (!request.url.startsWith('/api')) return;

  const auth = request.headers.authorization ?? '';
  const [scheme, token] = auth.split(' ');

  if (scheme !== 'Bearer' || !token || !timingSafeEqual(token, TOKEN)) {
    reply.code(401).send({ success: false, error: 'Unauthorized' });
  }
});

// Unauthenticated liveness probe — used by the CLI to decide whether to
// boot a fresh daemon. Path is outside /api so the auth hook above
// early-returns before checking the Bearer token.
fastify.get('/health', async () => ({ ok: true, pid: process.pid }));

function timingSafeEqual(a, b) {
  const bufA = Buffer.from(a);
  const bufB = Buffer.from(b);
  if (bufA.length !== bufB.length) return false;
  return crypto.timingSafeEqual(bufA, bufB);
}

// Configure Turndown for Codeforces-flavored HTML:
//  - CF wraps inline math in $$$...$$$ (within text nodes, not in tags),
//    which Turndown passes through unchanged — good.
//  - GFM fenced code is friendlier than indented for sample blocks.
const turndown = new TurndownService({
  headingStyle: 'atx',
  codeBlockStyle: 'fenced',
  bulletListMarker: '-',
});
// CF often uses <pre> for sample I/O; keep as-is (fenced code block).
// Drop the redundant <div class="section-title"> labels — they map to
// generic <strong> in the output and add noise next to the H2 headings.
turndown.addRule('cf-section-title', {
  filter: (node) =>
    node.nodeName === 'DIV' && node.classList && node.classList.contains('section-title'),
  replacement: (content) => `\n\n## ${content.trim()}\n\n`,
});

function buildReadme(payload, descriptionMd) {
  const tagsLine = payload.tags?.length
    ? `\n**Tags:** ${payload.tags.map((t) => `\`${t}\``).join(', ')}\n`
    : '';
  const rating = payload.rating ? `Rating: ${payload.rating}` : 'Rating: —';
  const limits = `${(payload.timeLimitMs / 1000).toFixed(1)}s, ${payload.memoryLimitMb} MB`;

  return [
    `# [${payload.index}] ${payload.name}`,
    '',
    `*Platform: Codeforces | Contest: ${payload.contestId} | ${rating} | Limits: ${limits}*`,
    '',
    `[View on Codeforces](${payload.url})`,
    tagsLine,
    '---',
    '',
    descriptionMd.trim() || '_No problem statement captured._',
    '',
  ].join('\n');
}

const GITIGNORE_BODY = `# Rune workspace .gitignore (managed by the daemon on first init).
# Compiled solution binaries built by the TUI test runner.
solution
solution.exe
*.o
*.out
`;

async function ensureGitRepo() {
  await fs.mkdir(WORKSPACE_ROOT, { recursive: true });

  const gitDir = path.join(WORKSPACE_ROOT, '.git');
  let initialized = false;
  try {
    await fs.access(gitDir);
  } catch {
    // -b main so the initial branch matches what the TUI pushes to.
    await execFileP('git', ['init', '-b', 'main'], { cwd: WORKSPACE_ROOT });
    console.log(`Initialized git repo at ${WORKSPACE_ROOT}`);
    initialized = true;
  }

  // Write .gitignore the first time we initialize, and never overwrite if the
  // user has customized it themselves.
  const gitignorePath = path.join(WORKSPACE_ROOT, '.gitignore');
  try {
    await fs.access(gitignorePath);
  } catch {
    await fs.writeFile(gitignorePath, GITIGNORE_BODY, 'utf8');
    if (!initialized) {
      console.log(`Wrote default .gitignore at ${gitignorePath}`);
    }
  }
}

// ---------------------------------------------------------------------------
// Daemon lifecycle: token file, PID file, graceful shutdown
// ---------------------------------------------------------------------------

async function writeTokenFile() {
  await fs.mkdir(RUNE_DIR, { recursive: true });
  // 0o600 = owner read/write only. Write to a temp path then rename to
  // make replacement atomic; opening O_TRUNC on the live path could let
  // a racing reader see an empty file.
  const tmp = `${TOKEN_PATH}.${process.pid}.tmp`;
  await fs.writeFile(tmp, TOKEN, { mode: 0o600 });
  await fs.rename(tmp, TOKEN_PATH);
  // Re-apply mode in case the rename inherited a broader umask on some FS.
  await fs.chmod(TOKEN_PATH, 0o600);
}

async function writePidFile() {
  await fs.mkdir(RUNE_DIR, { recursive: true });
  await fs.writeFile(PID_PATH, String(process.pid), 'utf8');
}

async function cleanupAndExit(signal) {
  // Try to remove our own lifecycle files. We don't remove the token file
  // by default — leaving it around is harmless and lets `rune daemon
  // status` distinguish "never ran" from "ran and stopped" if we ever
  // want that. But the PID file MUST go so future `rune` invocations
  // know we're down.
  try { await fs.unlink(PID_PATH); } catch {}
  try { await fastify.close(); } catch {}
  // Re-raise so the parent sees the actual signal in $?.
  process.kill(process.pid, signal);
}

function installShutdownHandlers() {
  for (const sig of ['SIGINT', 'SIGTERM']) {
    // Replace any default handler with our cleanup, then de-register ours
    // before re-raising to avoid an infinite loop.
    process.once(sig, () => {
      process.removeAllListeners(sig);
      cleanupAndExit(sig).catch((e) => {
        console.error('shutdown error:', e);
        process.exit(1);
      });
    });
  }
}

const CPP_BOILERPLATE = (name) => `// Problem: ${name}
#include <bits/stdc++.h>
using namespace std;

int main() {
    ios_base::sync_with_stdio(false);
    cin.tie(nullptr);

    return 0;
}
`;

fastify.post('/api/cf/ingest', {
  schema: {
    body: {
      type: 'object',
      required: ['contestId', 'index', 'name', 'url', 'tags', 'timeLimitMs', 'memoryLimitMb', 'testCases'],
      properties: {
        contestId: { type: 'number' },
        index: { type: 'string', pattern: '^[A-Za-z0-9]+$' },
        name: { type: 'string' },
        url: { type: 'string' },
        rating: { type: 'number' },
        tags: { type: 'array', items: { type: 'string' } },
        timeLimitMs: { type: 'number' },
        memoryLimitMb: { type: 'number' },
        descriptionHtml: { type: 'string' },
        testCases: {
          type: 'array',
          items: {
            type: 'object',
            required: ['id', 'input', 'expectedOutput'],
            properties: {
              id: { type: 'string' },
              input: { type: 'string' },
              expectedOutput: { type: 'string' },
            },
          },
        },
      },
    },
  },
}, async (request, reply) => {
  const payload = request.body;

  // contestId is schema-validated as a number and index as alphanumeric,
  // so the joined path cannot escape the base directory.
  const dir = path.join(WORKSPACE_ROOT, String(payload.contestId), payload.index);
  await fs.mkdir(dir, { recursive: true });

  await fs.writeFile(
    path.join(dir, 'test_cases.json'),
    JSON.stringify(payload, null, 2),
    'utf8'
  );

  // README.md is always (re)written so re-ingesting picks up corrections.
  const descriptionMd = payload.descriptionHtml
    ? turndown.turndown(payload.descriptionHtml)
    : '';
  await fs.writeFile(path.join(dir, 'README.md'), buildReadme(payload, descriptionMd), 'utf8');

  const solutionPath = path.join(dir, 'solution.cpp');
  try {
    // 'wx' fails if the file exists — never clobber an in-progress solution.
    await fs.writeFile(solutionPath, CPP_BOILERPLATE(payload.name), { flag: 'wx' });
  } catch (err) {
    if (err.code !== 'EEXIST') throw err;
  }

  return reply.code(200).send({ success: true, path: dir });
});

try {
  await ensureGitRepo();
  await fastify.listen({ host: HOST, port: PORT });
  await writeTokenFile();
  await writePidFile();
  installShutdownHandlers();
  console.log('');
  console.log('='.repeat(60));
  console.log(`  CP Ingestion Daemon running at http://${HOST}:${PORT}`);
  console.log(`  Bearer Token: ${TOKEN}`);
  console.log(`  Token file:   ${TOKEN_PATH}`);
  console.log(`  PID file:     ${PID_PATH} (pid ${process.pid})`);
  console.log('='.repeat(60));
} catch (err) {
  fastify.log.error(err);
  process.exit(1);
}
