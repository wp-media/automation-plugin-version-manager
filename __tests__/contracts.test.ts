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
