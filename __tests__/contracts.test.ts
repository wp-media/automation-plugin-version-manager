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
