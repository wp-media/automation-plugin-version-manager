import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { describe, expect, it, vi } from 'vitest';
import { Apvm, type JsBuildEvent, JsReleaseSelector } from '../index.js';

// Error and event contracts of `Apvm`, fully offline.
//
// Every call here fails (or reports) in the core's fail-fast checks — the
// registry lookup, the private-repo token check, the version guard, the
// releases check — which all run before any network or git work. The older
// suites only assert *that* these calls reject; these lock *how*: the
// `err.code` a caller branches on and the message a user reads.

/** A rejection as JS sees it: an `Error` with napi's `code`. */
type CodedError = Error & { code?: string };

/** The rejection of `promise` (fails the test if it resolves). */
async function rejection(promise: Promise<unknown>): Promise<CodedError> {
  try {
    await promise;
  } catch (err) {
    return err as CodedError;
  }
  throw new Error('expected the promise to reject');
}

/** The error `call` throws synchronously (fails the test if it does not). */
function thrown(call: () => unknown): CodedError {
  try {
    call();
  } catch (err) {
    return err as CodedError;
  }
  throw new Error('expected the call to throw');
}

/** Never written: every call is refused before anything is delivered. */
const OUT = '/tmp/apvm-contracts-unused-output';

// =============================================================================
// Fail-fast rejections
// =============================================================================

describe('fail-fast rejections carry a code and an actionable message', () => {
  const unknownProject: Array<[string, (apvm: Apvm) => Promise<unknown>]> = [
    ['build()', (apvm) => apvm.build({ project: 'nope', gitRef: 'pr:1', outputDir: OUT })],
    ['warmCache()', (apvm) => apvm.warmCache({ project: 'nope', gitRef: 'pr:1' })],
    ['buildFromPr()', (apvm) => apvm.buildFromPr('nope', 1, OUT)],
    ['buildFromBranch()', (apvm) => apvm.buildFromBranch('nope', 'main', OUT)],
    ['buildFromTag()', (apvm) => apvm.buildFromTag('nope', 'v1', OUT)],
    ['buildFromCommit()', (apvm) => apvm.buildFromCommit('nope', 'abc1234', OUT)],
    ['downloadRelease()', (apvm) => apvm.downloadRelease('nope', 'v1', OUT)],
    [
      'downloadReleaseBySelector()',
      (apvm) => apvm.downloadReleaseBySelector('nope', JsReleaseSelector.LatestStable, OUT),
    ],
  ];

  it.each(unknownProject)('%s rejects an unknown project with InvalidArg', async (_label, call) => {
    const err = await rejection(call(await Apvm.create({})));
    expect(err).toBeInstanceOf(Error);
    expect(err.code).toBe('InvalidArg');
    expect(err.message).toBe('Project not found: nope');
  });

  it('a private project without a token is InvalidArg naming the fix', async () => {
    // `create()` never resolves a token from the environment, so this holds
    // even where GITHUB_TOKEN is set.
    const apvm = await Apvm.create({});
    expect(apvm.hasToken()).toBe(false);
    const err = await rejection(apvm.build({ project: 'backwpup', gitRef: 'pr:1', outputDir: OUT }));
    expect(err.code).toBe('InvalidArg');
    expect(err.message).toMatch(/^Repository 'wp-media\/backwpup-pro' is private and requires a GitHub token/);
    expect(err.message).toContain('GITHUB_TOKEN');
  });

  it('a release of a project without releases is InvalidArg suggesting a git ref', async () => {
    const apvm = await Apvm.create({});
    // Thunks, so each call starts only when awaited: a promise created early
    // would reject unobserved and surface as an unhandled rejection.
    for (const call of [
      () => apvm.downloadRelease('wp-rocket', 'v3.17.0', OUT),
      () => apvm.build({ project: 'wp-rocket', gitRef: 'release:v3.17.0', outputDir: OUT }),
    ]) {
      const err = await rejection(call());
      expect(err.code).toBe('InvalidArg');
      expect(err.message).toMatch(/^Project 'wp-rocket' does not have GitHub Releases/);
      expect(err.message).toContain('tag:v3.17.0');
    }
  });

  it('a version with shell-active characters is refused before any clone', async () => {
    // The version is spliced into shell build steps: it must never reach one.
    const apvm = await Apvm.create({});
    for (const version of ['1.0;rm -rf ~', '$(id)', '1.0 2', '']) {
      const err = await rejection(
        apvm.build({ project: 'wp-rocket', gitRef: 'pr:1', outputDir: OUT, version }),
      );
      expect(err.message, JSON.stringify(version)).toContain('contains unsupported characters');
    }
  });

  it('a malformed PR number is refused with the value it got', async () => {
    const apvm = await Apvm.create({});
    const err = await rejection(apvm.build({ project: 'wp-rocket', gitRef: 'pr:abc', outputDir: OUT }));
    expect(err.message).toContain("Invalid PR number: 'abc'");
  });
});

// =============================================================================
// Argument conversion (napi's own checks, thrown synchronously)
// =============================================================================

describe('mistyped arguments throw synchronously', () => {
  it('a null cacheDir throws StringExpected (only undefined means omitted)', () => {
    // Documented on `ApvmConfig.cacheDir`.
    const err = thrown(() => Apvm.create({ cacheDir: null as never }));
    expect(err.code).toBe('StringExpected');
    expect(err.message).toContain('ApvmConfig.cacheDir');
  });

  it('a wrongly typed build option names the field', async () => {
    const apvm = await Apvm.create({});
    const err = thrown(() => apvm.build({ project: 42 as never, gitRef: 'pr:1', outputDir: OUT }));
    expect(err.code).toBe('StringExpected');
    expect(err.message).toContain('BuildOptions.project');
  });

  it('a selector outside JsReleaseSelector is InvalidArg', async () => {
    const apvm = await Apvm.create({});
    const err = thrown(() => apvm.downloadReleaseBySelector('wp-rocket', 'Newest' as never, OUT));
    expect(err.code).toBe('InvalidArg');
    expect(err.message).toContain('JsReleaseSelector');
  });
});

// =============================================================================
// Progress events reach JS
// =============================================================================

/**
 * Run `script` (CommonJS) in a fresh Node process under the strict Node-API
 * exception policy, so an exception the addon leaves pending in a callback
 * crashes the child instead of only printing a DEP0168 warning.
 *
 * spawnSync, not execFileSync: a crash then fails the assertion with the
 * child's exit status and stderr instead of a dump of the script. The 20 s
 * cap stays under the 30 s test timeout, which cannot fire while this blocks.
 *
 * @param script - Script body; `ADDON` holds the path of the addon's `index.js`.
 * @returns The finished child process.
 */
function runStrictNode(script: string): ReturnType<typeof spawnSync> {
  const addon = createRequire(import.meta.url).resolve('../index.js');
  return spawnSync(
    process.execPath,
    ['--force-node-api-uncaught-exceptions-policy=true', '-e', `const ADDON = ${JSON.stringify(addon)};\n${script}`],
    { encoding: 'utf8', timeout: 20_000 },
  );
}

/** The parts of a finished child that say whether it ran cleanly. */
function exitOf(child: ReturnType<typeof spawnSync>): { status: number | null; signal: string | null; stderr: unknown } {
  return { status: child.status, signal: child.signal, stderr: child.stderr };
}

/** What `exitOf` reports for a child that exited normally and printed no errors. */
const CLEAN_EXIT = { status: 0, signal: null, stderr: '' };

describe('progress callback', () => {
  /** Let queued thread-safe-function calls reach JS. */
  const drain = () => new Promise<void>((resolve) => setTimeout(resolve, 50));

  it('delivers events as plain objects with absent fields omitted', async () => {
    // Warming with caching off warns before the (offline) releases check
    // fails, so exactly one event is emitted, deterministically.
    const apvm = await Apvm.create({ cacheEnabled: false });
    const events: Array<[Error | null, JsBuildEvent]> = [];
    const err = await rejection(
      apvm.warmCache({ project: 'wp-rocket', gitRef: 'release:v1.0.0' }, (error, event) => {
        events.push([error, event]);
      }),
    );
    expect(err.code).toBe('InvalidArg');
    // Poll rather than sleep a fixed time: a loaded CI runner may deliver the
    // queued event late. The drain afterwards catches any unexpected extra.
    await vi.waitFor(() => expect(events).toHaveLength(1), { timeout: 5_000 });
    await drain();
    expect(events).toHaveLength(1);
    const [error, event] = events[0];
    expect(error).toBeNull();
    expect(event).toStrictEqual({
      type: 'warning',
      message:
        'cache warming was requested, but the artifact cache is disabled or unavailable; nothing will be cached',
    });
  });

  it('discards callback errors, sync or async, without crashing the process', () => {
    // Documented: progress is best-effort, so a failing callback never fails
    // the call or kills Node. Each case used to crash the process — a sync
    // throw as an uncaught exception, an async one as an unhandled rejection —
    // so the scenario runs in a child process that reports what it saw.
    const child = runStrictNode(`
      const { Apvm } = require(ADDON);
      const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
      // JS can throw anything, and an async callback rejects instead.
      const failures = {
        error: () => { throw new Error('callback boom'); },
        string: () => { throw 'not an Error'; },
        symbol: () => { throw Symbol('boom'); },
        asyncThrow: async () => { throw new Error('async boom'); },
        rejected: () => Promise.reject(new Error('rejected')),
      };
      (async () => {
        const apvm = await Apvm.create({ cacheEnabled: false });
        const results = {};
        for (const [kind, fail] of Object.entries(failures)) {
          let calls = 0;
          const outcome = await apvm
            .warmCache({ project: 'wp-rocket', gitRef: 'release:v1.0.0' }, () => {
              calls += 1;
              return fail();
            })
            .then(() => 'resolved', (err) => err.code);
          // Let the queued warning reach the callback, then let any
          // rejection settle (an unhandled one would kill this process).
          for (let i = 0; i < 250 && calls === 0; i += 1) await sleep(20);
          await sleep(50);
          results[kind] = { outcome, calls };
        }
        console.log(JSON.stringify(results));
      })().catch((err) => {
        console.error(err);
        process.exitCode = 1;
      });
    `);
    expect(exitOf(child)).toStrictEqual(CLEAN_EXIT);
    // Each call still rejects with its own error, after exactly one event.
    const each = { outcome: 'InvalidArg', calls: 1 };
    expect(JSON.parse(String(child.stdout))).toStrictEqual({
      error: each,
      string: each,
      symbol: each,
      asyncThrow: each,
      rejected: each,
    });
  });

  it('never leaves an exception pending, even with a sabotaged Promise.prototype.catch', () => {
    // Settling an async callback's promise runs its `catch`; when that throws,
    // the addon must clear the exception rather than leave it pending (fatal
    // under the strict policy, a DEP0168 warning otherwise).
    const child = runStrictNode(`
      const { Apvm } = require(ADDON);
      Promise.prototype.catch = function () { throw new Error('patched catch'); };
      (async () => {
        const apvm = await Apvm.create({ cacheEnabled: false });
        let calls = 0;
        const outcome = await apvm
          .warmCache({ project: 'wp-rocket', gitRef: 'release:v1.0.0' }, async () => {
            calls += 1;
          })
          .then(() => 'resolved', (err) => err.code);
        for (let i = 0; i < 250 && calls === 0; i += 1) await new Promise((r) => setTimeout(r, 20));
        await new Promise((r) => setTimeout(r, 50));
        console.log(JSON.stringify({ outcome, calls }));
      })();
    `);
    expect(exitOf(child)).toStrictEqual(CLEAN_EXIT);
    expect(JSON.parse(String(child.stdout))).toStrictEqual({ outcome: 'InvalidArg', calls: 1 });
  });

  it('reports nothing for a call refused before any work', async () => {
    const apvm = await Apvm.create({});
    const events: JsBuildEvent[] = [];
    await rejection(
      apvm.build({ project: 'nope', gitRef: 'pr:1', outputDir: OUT }, (_error, event) => {
        events.push(event);
      }),
    );
    await drain();
    expect(events).toStrictEqual([]);
  });
});

// =============================================================================
// Worker threads
// =============================================================================

describe('a worker thread that exits while calls are in flight', () => {
  // An `async fn(&self)` napi method keeps a native borrow of the JS object
  // while its future runs. When the worker's env was torn down first, napi-rs
  // released it on a Tokio thread and called process::abort() — or the process
  // died with SIGSEGV/SIGBUS. Each case starts calls that are still pending
  // when the worker exits (synchronously, right after starting them); the
  // whole process must survive. One child process per case, so a failure
  // names the method. A single round can miss the race (calls that fail fast
  // may settle before the teardown), hence several rounds per case and each
  // method in both setups below.
  const methods = [
    'build',
    'warmCache',
    'buildFromPr',
    'buildFromBranch',
    'buildFromTag',
    'buildFromCommit',
    'downloadRelease',
    'downloadReleaseBySelector',
    'cacheStatus',
    'cacheInfo',
  ] as const;
  // `mainLoads`: the main thread loads the addon too (the usual setup), so
  // the Tokio runtime outlives the worker and pending calls settle after its
  // env is gone — a separate crash path.
  const cases = methods.flatMap((method) => [
    { method, mainLoads: false },
    { method, mainLoads: true },
  ]);

  it.each(cases)('$method (main thread loads the addon: $mainLoads)', ({ method, mainLoads }) => {
    const child = runStrictNode(`
      const { Worker } = require('node:worker_threads');
      if (${mainLoads}) require(ADDON);
      const workerSource = \`
        const { parentPort, workerData } = require('node:worker_threads');
        const { Apvm, JsReleaseSelector } = require(workerData.addon);
        const out = '/nonexistent/apvm-worker-test';
        // Every other call also streams progress to a callback.
        const calls = {
          build: (a, p) => a.build({ project: 'nope', gitRef: 'pr:1', outputDir: out }, p),
          warmCache: (a, p) => a.warmCache({ project: 'wp-rocket', gitRef: 'release:v1.0.0' }, p),
          buildFromPr: (a, p) => a.buildFromPr('nope', 1, out, undefined, undefined, p),
          buildFromBranch: (a, p) => a.buildFromBranch('nope', 'main', out, undefined, undefined, p),
          buildFromTag: (a, p) => a.buildFromTag('nope', 'v1', out, undefined, undefined, p),
          buildFromCommit: (a, p) => a.buildFromCommit('nope', 'abc1234', out, undefined, undefined, p),
          downloadRelease: (a, p) => a.downloadRelease('nope', 'v1', out, undefined, p),
          downloadReleaseBySelector: (a, p) =>
            a.downloadReleaseBySelector('nope', JsReleaseSelector.LatestStable, out, undefined, p),
          cacheStatus: (a) => a.cacheStatus(),
          cacheInfo: (a) => a.cache().info(),
        };
        (async () => {
          const apvm = await Apvm.create({ cacheEnabled: false });
          for (let i = 0; i < 30; i += 1) {
            calls[workerData.method](apvm, i % 2 === 0 ? undefined : () => {}).catch(() => {});
          }
          parentPort.postMessage('started');
          process.exit(0);
        })();
      \`;
      (async () => {
        for (let round = 0; round < 8; round += 1) {
          const worker = new Worker(workerSource, {
            eval: true,
            workerData: { addon: ADDON, method: ${JSON.stringify(method)} },
          });
          await new Promise((resolve) => worker.once('message', resolve).once('exit', resolve));
          await worker.terminate();
        }
        // Let calls that outlived their worker settle in this process.
        await new Promise((resolve) => setTimeout(resolve, 100));
        console.log(JSON.stringify({ survived: true }));
      })();
    `);
    expect(exitOf(child)).toStrictEqual(CLEAN_EXIT);
    expect(JSON.parse(String(child.stdout))).toStrictEqual({ survived: true });
  });
});

describe('an environment that goes away mid-build', () => {
  // The build's `git ls-remote` is redirected (through git's own GIT_CONFIG_*
  // environment) to an `ext::` command that appends the PIDs of the top git
  // process and of itself to two files, then sleeps — a git that hangs, fully
  // offline. Line N of each file belongs to the Nth hanging git.
  // git runs it through its `git remote-ext` helper, so this is a three-level
  // process tree; all of it must stop when the environment that started it
  // goes away. Needs `sh` and `ps`, so Unix only.

  /**
   * Child-script prelude: redirects git and defines `fs`, `path`, `dir`,
   * `buildOptions`, `gitPidFile`, `helperPidFile`, `workerBuild`,
   * `workerData`, `readPid`, `readPids`, `running`, `stopsSoon`,
   * `killSurvivors`, `removeDir`, `fail` and `sleep`.
   */
  const HANGING_GIT = `
    const fs = require('node:fs');
    const os = require('node:os');
    const path = require('node:path');
    const { execFileSync } = require('node:child_process');

    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'apvm-git-tree-'));
    const removeDir = () => fs.rmSync(dir, { recursive: true, force: true });
    const gitPidFile = path.join(dir, 'git.pid');
    const helperPidFile = path.join(dir, 'helper.pid');
    // ext:: splits on spaces; '% ' is a literal space. The helper then
    // \`exec\`s into sleep, so its recorded PID is the sleeping process.
    const ext = ['ext::sh -c ps', '-o', 'ppid=', '-p', '$PPID', '>>', gitPidFile + ';',
      'echo', '$$', '>>', helperPidFile + ';', 'exec', 'sleep', '30'].join('% ');
    Object.assign(process.env, {
      GIT_CONFIG_COUNT: '2',
      GIT_CONFIG_KEY_0: 'protocol.ext.allow',
      GIT_CONFIG_VALUE_0: 'always',
      GIT_CONFIG_KEY_1: 'url.' + ext + '.insteadOf',
      GIT_CONFIG_VALUE_1: 'https://github.com/wp-media/wp-rocket.git',
    });
    const buildOptions = { project: 'wp-rocket', gitRef: 'branch:develop', outputDir: path.join(dir, 'out') };
    // A worker (eval source) that starts the build; given \`end\`, it runs that
    // code when the main thread posts it a message — only after checking its
    // processes, so it can never end before that check. Its workerData is
    // \`workerData\` below.
    const workerBuild = (end) => [
      "const { parentPort, workerData } = require('node:worker_threads');",
      "const { Apvm } = require(workerData.addon);",
      "Apvm.create({ cacheEnabled: false }).then((apvm) => apvm.build(workerData.options).catch(() => {}));",
      end ? "parentPort.once('message', () => { " + end + " });" : '',
    ].join('\\n');
    const workerData = { addon: ADDON, options: buildOptions };

    const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    // Running = listed by ps and not a zombie (a killed child of this process
    // stays a zombie until it is reaped).
    const running = (pid) => {
      try {
        const state = execFileSync('ps', ['-o', 'stat=', '-p', String(pid)], { encoding: 'utf8' }).trim();
        return state !== '' && !state.startsWith('Z');
      } catch {
        return false;
      }
    };
    // The PID on line \`index\` of \`file\`, once written in full.
    const readPid = async (file, index = 0) => {
      for (let i = 0; i < 500; i += 1) {
        const text = fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : '';
        const lines = text.split('\\n').slice(0, -1);
        if (lines.length > index) return Number(lines[index].trim());
        await sleep(20);
      }
      throw new Error('process #' + index + ' never started: ' + file);
    };
    // The PIDs of the Nth hanging git and of its helper.
    const readPids = async (index = 0) => ({
      git: await readPid(gitPidFile, index),
      helper: await readPid(helperPidFile, index),
    });
    const stopsSoon = async (pid) => {
      for (let i = 0; i < 250; i += 1) {
        if (!running(pid)) return true;
        await sleep(20);
      }
      return false;
    };
    // Leave nothing behind when a check failed: SIGKILL each process that was
    // never seen stopping (so is still ours). One that stopped is never
    // signalled — its PID may already belong to another process.
    const killSurvivors = (pids, stopped) => {
      for (const name of Object.keys(pids)) {
        if (!stopped[name]) {
          try { process.kill(pids[name], 'SIGKILL'); } catch {}
        }
      }
    };
    // An unexpected error: report it, and still remove the temp dir.
    const fail = (err) => {
      console.error(err);
      removeDir();
      process.exitCode = 2;
    };
  `;

  /** Whether `pid` is still running, as `ps` sees it from this process. */
  function running(pid: number): boolean {
    const ps = spawnSync('ps', ['-o', 'stat=', '-p', String(pid)], { encoding: 'utf8' });
    const state = String(ps.stdout).trim();
    return state !== '' && !state.startsWith('Z');
  }

  /** Whether `pid` stops running within 5 s. */
  async function stopsSoon(pid: number): Promise<boolean> {
    for (let i = 0; i < 250; i += 1) {
      if (!running(pid)) return true;
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    return false;
  }

  /**
   * Whether each of `pids` stops soon, SIGKILLing the ones that do not (so
   * a failed check leaves nothing behind). One that stopped is never
   * signalled: its PID may already belong to another process.
   */
  async function stopAll<K extends string>(pids: Record<K, number>): Promise<Record<K, boolean>> {
    const stopped = {} as Record<K, boolean>;
    for (const name of Object.keys(pids) as K[]) {
      stopped[name] = await stopsSoon(pids[name]);
      if (!stopped[name]) {
        try {
          process.kill(pids[name], 'SIGKILL');
        } catch {
          // Gone meanwhile.
        }
      }
    }
    return stopped;
  }

  // Every way a worker can end while its build runs. The call is cancelled
  // even while the main thread keeps the addon and its runtime alive, and its
  // whole process tree is killed.
  const workerEndings = [
    { how: 'being terminated', mainLoads: false, inWorker: '', fromMain: 'await worker.terminate();' },
    { how: 'being terminated', mainLoads: true, inWorker: '', fromMain: 'await worker.terminate();' },
    {
      how: 'calling process.exit()',
      mainLoads: true,
      inWorker: 'process.exit(0);',
      fromMain: "worker.postMessage('end');",
    },
    {
      how: 'an uncaught exception',
      mainLoads: true,
      inWorker: "throw new Error('boom');",
      fromMain: "worker.postMessage('end');",
    },
  ];

  it.skipIf(process.platform === 'win32').each(workerEndings)(
    'a worker ending by $how stops the git processes it started (main thread loads the addon: $mainLoads)',
    ({ mainLoads, inWorker, fromMain }) => {
      const child = runStrictNode(`
        ${HANGING_GIT}
        const { Worker } = require('node:worker_threads');
        if (${mainLoads}) require(ADDON);
        (async () => {
          const worker = new Worker(workerBuild(${JSON.stringify(inWorker)}), { eval: true, workerData });
          // Not events.once(): it rejects on the 'error' an uncaught exception emits.
          worker.on('error', () => {});
          const exited = new Promise((resolve) => worker.once('exit', resolve));
          const pids = await readPids();
          const before = { git: running(pids.git), helper: running(pids.helper) };
          ${fromMain}
          await exited;
          const stopped = { git: await stopsSoon(pids.git), helper: await stopsSoon(pids.helper) };
          killSurvivors(pids, stopped);
          removeDir();
          console.log(JSON.stringify({ before, stopped }));
        })().catch(fail);
      `);
      expect(exitOf(child)).toStrictEqual(CLEAN_EXIT);
      expect(JSON.parse(String(child.stdout))).toStrictEqual({
        before: { git: true, helper: true },
        stopped: { git: true, helper: true },
      });
    },
  );

  it.skipIf(process.platform === 'win32')(
    'a worker ending stops the `gh auth token` it started',
    () => {
      // `createWithTokenResolution` runs `gh` — the one process outside a
      // build. A fake `gh` first on PATH records its PID and hangs.
      const child = runStrictNode(`
        ${HANGING_GIT}
        const { Worker } = require('node:worker_threads');
        (async () => {
          const bin = path.join(dir, 'bin');
          const ghPidFile = path.join(dir, 'gh.pid');
          fs.mkdirSync(bin);
          fs.writeFileSync(path.join(bin, 'gh'), '#!/bin/sh\\necho $$ >> "' + ghPidFile + '"\\nexec sleep 30\\n', { mode: 0o755 });
          // The real environment: Rust reads it, not a worker's copy.
          process.env.PATH = bin + path.delimiter + process.env.PATH;
          delete process.env.GITHUB_TOKEN;
          delete process.env.GH_TOKEN;
          const worker = new Worker(
            "require(require('node:worker_threads').workerData.addon).Apvm.createWithTokenResolution({ cacheEnabled: false }).catch(() => {});",
            { eval: true, workerData },
          );
          const pids = { gh: await readPid(ghPidFile) };
          const before = { gh: running(pids.gh) };
          await worker.terminate();
          const stopped = { gh: await stopsSoon(pids.gh) };
          killSurvivors(pids, stopped);
          removeDir();
          console.log(JSON.stringify({ before, stopped }));
        })().catch(fail);
      `);
      expect(exitOf(child)).toStrictEqual(CLEAN_EXIT);
      expect(JSON.parse(String(child.stdout))).toStrictEqual({ before: { gh: true }, stopped: { gh: true } });
    },
  );

  it.skipIf(process.platform === 'win32')(
    "a worker exiting leaves the main thread's git processes running",
    () => {
      // Each env kills only what its own calls started.
      const child = runStrictNode(`
        ${HANGING_GIT}
        const { Worker } = require('node:worker_threads');
        const { Apvm } = require(ADDON);
        (async () => {
          const apvm = await Apvm.create({ cacheEnabled: false });
          apvm.build(buildOptions).catch(() => {});
          const main = await readPids(0);
          const worker = new Worker(workerBuild(''), { eval: true, workerData });
          const workers = await readPids(1);
          await worker.terminate();
          const workerStopped = { git: await stopsSoon(workers.git), helper: await stopsSoon(workers.helper) };
          const mainRunning = { git: running(main.git), helper: running(main.helper) };
          killSurvivors(workers, workerStopped);
          // The main thread's processes are this test's to end: still running.
          killSurvivors(main, { git: !mainRunning.git, helper: !mainRunning.helper });
          removeDir();
          console.log(JSON.stringify({ workerStopped, mainRunning }));
        })().catch(fail);
      `);
      expect(exitOf(child)).toStrictEqual(CLEAN_EXIT);
      expect(JSON.parse(String(child.stdout))).toStrictEqual({
        workerStopped: { git: true, helper: true },
        mainRunning: { git: true, helper: true },
      });
    },
  );

  it.skipIf(process.platform === 'win32')(
    "a stray process.emit('exit') neither kills nor blocks builds",
    async () => {
      // Only a real exit kills: an emitted `exit` (a library simulating one)
      // must leave running builds alone, and new ones still able to start.
      const child = runStrictNode(`
        ${HANGING_GIT}
        const { Apvm } = require(ADDON);
        (async () => {
          const apvm = await Apvm.create({ cacheEnabled: false });
          apvm.build(buildOptions).catch(() => {});
          const first = await readPids(0);
          process.emit('exit', 0);
          await sleep(200);
          const afterStrayExit = { git: running(first.git), helper: running(first.helper) };
          apvm.build(buildOptions).catch(() => {});
          const second = await readPids(1);
          removeDir();
          // Synchronous: process.exit() can drop buffered console output.
          fs.writeSync(1, JSON.stringify({ afterStrayExit, pids: { first, second } }));
          process.exit(0); // a real exit: kills both builds' processes
        })().catch(fail);
      `);
      expect(exitOf(child)).toStrictEqual(CLEAN_EXIT);
      const { afterStrayExit, pids } = JSON.parse(String(child.stdout)) as {
        afterStrayExit: { git: boolean; helper: boolean };
        pids: Record<'first' | 'second', { git: number; helper: number }>;
      };
      const stopped = {
        first: await stopAll(pids.first),
        second: await stopAll(pids.second),
      };
      expect(afterStrayExit).toStrictEqual({ git: true, helper: true });
      expect(stopped).toStrictEqual({
        first: { git: true, helper: true },
        second: { git: true, helper: true },
      });
    },
  );

  /** Child-script helper: the addon's `exit` listeners on this thread. */
  const OUR_EXIT_LISTENERS =
    "const ours = () => process.listeners('exit').filter((f) => f.name === 'apvmKillChildProcesses').length;";

  it('hooks process exit once per environment, however many calls', () => {
    // One listener per call would leak; a call made from inside
    // `process.on` must not hook twice.
    const child = runStrictNode(`
      ${OUR_EXIT_LISTENERS}
      const { Apvm } = require(ADDON);
      const realOn = process.on;
      let reentered = false;
      process.on = function (event, listener) {
        if (event === 'exit' && !reentered) {
          reentered = true;
          Apvm.create({ cacheEnabled: false }).catch(() => {});
        }
        return realOn.call(this, event, listener);
      };
      (async () => {
        for (let i = 0; i < 5; i += 1) await Apvm.create({ cacheEnabled: false });
        console.log(JSON.stringify({ reentered, hooks: ours() }));
      })().catch((err) => {
        console.error(err);
        process.exitCode = 2;
      });
    `);
    expect(exitOf(child)).toStrictEqual(CLEAN_EXIT);
    expect(JSON.parse(String(child.stdout))).toStrictEqual({ reentered: true, hooks: 1 });
  });

  it('hooks process exit without a MaxListenersExceededWarning', () => {
    // A host already at its `exit` listener limit (10 by default) gets no
    // warning for the addon's, and keeps its limit.
    const child = runStrictNode(`
      ${OUR_EXIT_LISTENERS}
      for (let i = 0; i < 10; i += 1) process.on('exit', () => {});
      const { Apvm } = require(ADDON);
      Apvm.create({ cacheEnabled: false })
        .then(() => {
          // After a turn, as Node emits the warning asynchronously.
          setImmediate(() => {
            const state = { hooks: ours(), total: process.listenerCount('exit'), max: process.getMaxListeners() };
            console.log(JSON.stringify(state));
          });
        })
        .catch((err) => {
          console.error(err);
          process.exitCode = 2;
        });
    `);
    expect(exitOf(child)).toStrictEqual(CLEAN_EXIT); // stderr: no warning
    expect(JSON.parse(String(child.stdout))).toStrictEqual({ hooks: 1, total: 11, max: 10 });
  });

  it('a process.on that throws never breaks a call, and the hook is retried', () => {
    // Hooking `exit` is best-effort: a host whose patched `process.on`
    // throws still gets its call and never sees that exception; a later call
    // hooks again.
    const child = runStrictNode(`
      ${OUR_EXIT_LISTENERS}
      const realOn = process.on;
      let attempts = 0;
      process.on = function (event, listener) {
        if (event === 'exit') {
          attempts += 1;
          if (attempts === 1) throw new Error('patched process.on');
        }
        return realOn.call(this, event, listener);
      };
      const { Apvm } = require(ADDON);
      (async () => {
        await Apvm.create({ cacheEnabled: false });
        const afterFirst = ours();
        await Apvm.create({ cacheEnabled: false });
        console.log(JSON.stringify({ attempts, afterFirst, afterSecond: ours() }));
      })().catch((err) => {
        console.error(err);
        process.exitCode = 2;
      });
    `);
    expect(exitOf(child)).toStrictEqual(CLEAN_EXIT);
    expect(JSON.parse(String(child.stdout))).toStrictEqual({ attempts: 2, afterFirst: 0, afterSecond: 1 });
  });

  it('an unreadable process.env never makes a call throw', () => {
    // `APVM_CACHE_DIR` then comes from the process environment, and the
    // getter's exception must not surface from a call that goes on.
    const child = runStrictNode(`
      ${OUR_EXIT_LISTENERS}
      const { Apvm } = require(ADDON);
      Object.defineProperty(process, 'env', {
        get() {
          throw new Error('env getter threw');
        },
      });
      (async () => {
        await Apvm.create({ cacheEnabled: false });
        await Apvm.create({ cacheEnabled: false });
        console.log(JSON.stringify({ created: 2, hooks: ours() }));
      })().catch((err) => {
        console.error(err);
        process.exitCode = 2;
      });
    `);
    expect(exitOf(child)).toStrictEqual(CLEAN_EXIT);
    expect(JSON.parse(String(child.stdout))).toStrictEqual({ created: 2, hooks: 1 });
  });

  // Every way the main thread can end the process while a build runs. None
  // drops the main thread's futures, and `process.exit()` and crashes run no
  // env cleanup hooks there: the addon kills from the `exit` event, and a
  // worker's calls stop with the worker, which Node tears down on the way.
  const endings = [
    { how: 'process.exit()', end: 'process.exit(0);', status: 0 },
    { how: 'an uncaught exception', end: "setTimeout(() => { throw new Error('boom'); });", status: 1 },
    { how: 'an unhandled rejection', end: "Promise.reject(new Error('boom'));", status: 1 },
  ];
  const mainThreadEndings = [
    ...endings.map((ending) => ({ ...ending, where: 'the main thread' })),
    ...endings.map((ending) => ({ ...ending, where: 'a worker' })),
    // Only an unref'd worker lets the event loop empty: a pending call on the
    // main thread keeps the process alive.
    { how: 'its event loop emptying', end: 'worker.unref();', status: 0, where: 'a worker' },
  ];

  it.skipIf(process.platform === 'win32').each(mainThreadEndings)(
    'the main thread ending by $how stops the git processes started on $where',
    async ({ end, status, where }) => {
      const start =
        where === 'a worker'
          ? "const worker = new Worker(workerBuild(''), { eval: true, workerData });"
          : 'const apvm = await Apvm.create({ cacheEnabled: false }); apvm.build(buildOptions).catch(() => {});';
      const child = runStrictNode(`
        ${HANGING_GIT}
        const { Worker } = require('node:worker_threads');
        const { Apvm } = require(ADDON);
        (async () => {
          ${start}
          const pids = await readPids();
          removeDir();
          // Synchronous: process.exit() can drop buffered console output.
          fs.writeSync(1, JSON.stringify(pids));
          ${end}
        })().catch(fail);
      `);
      expect(child.status).toBe(status);
      if (status !== 0) expect(String(child.stderr)).toContain('boom');
      const pids = JSON.parse(String(child.stdout)) as { git: number; helper: number };
      expect(await stopAll(pids)).toStrictEqual({ git: true, helper: true });
    },
  );
});

// =============================================================================
// Instances with caching off
// =============================================================================

describe('an instance with caching off', () => {
  it('reports the cache as disabled but still hands out its maintenance handle', async () => {
    const apvm = await Apvm.create({ cacheEnabled: false });
    expect((await apvm.cacheStatus()).state).toBe('disabled');
    // The handle maintains the configured directory regardless of the flag.
    expect(apvm.cache().dir()).toBe(process.env.APVM_CACHE_DIR);
  });
});
