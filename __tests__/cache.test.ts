import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { Apvm, ApvmCache, JsCleanTarget } from '../index.js';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, unlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { isAbsolute, join } from 'node:path';

// Cache maintenance — fully offline. A cache is seeded through the public API:
// build directories on disk plus a garbage `apvm.db`, which `repair()`
// resets in place (keeping a copy) and then re-indexes those builds into. The
// directory carries the store's lock file (`.apvm.lock`), as every real cache
// does: without it, store-shaped data is someone else's and repair refuses.
//
// Isolation: every test points APVM_CACHE_DIR at its own directory and
// restores the suite's value afterwards. The env var overrides `cacheDir`, so
// isolating through `cacheDir` alone would silently share the suite's cache.

/** The suite-wide value from `setup.ts`, restored after each test. */
const suiteCacheDir = process.env.APVM_CACHE_DIR;

let root = '';
/** This test's cache directory (absent until a test creates it). */
let cacheDir = '';

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), 'apvm-cache-it-'));
  cacheDir = join(root, 'cache');
  process.env.APVM_CACHE_DIR = cacheDir;
});

afterEach(async () => {
  if (suiteCacheDir === undefined) {
    delete process.env.APVM_CACHE_DIR;
  } else {
    process.env.APVM_CACHE_DIR = suiteCacheDir;
  }
  // Best-effort, like setup.ts: on Windows an `Apvm` the GC has not freed yet
  // still holds the database open, and deleting it fails with EBUSY/EPERM.
  try {
    await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  } catch (err) {
    console.warn(`[cache.test] could not remove ${root}: ${String(err)}`);
  }
});

/** Seeded builds: project, version, commit dir, file, content. */
const BUILDS = [
  { project: 'wp-rocket', version: '3.17.0', commit: 'abcdef1', file: 'wp-rocket.zip', content: 'rocket-zip' },
  { project: 'backwpup', version: '5.1.0', commit: '1234567', file: 'backwpup-free.zip', content: 'backwpup-zip!' },
] as const;

/** Path of a seeded build's file. */
function buildFile(build: (typeof BUILDS)[number]): string {
  return join(cacheDir, build.project, 'commits', build.version, build.commit, build.file);
}

/** Seed `cacheDir` with `BUILDS`, indexed by `repair()`; returns the handle. */
async function seed(): Promise<ApvmCache> {
  for (const build of BUILDS) {
    await mkdir(join(buildFile(build), '..'), { recursive: true });
    await writeFile(buildFile(build), build.content);
  }
  await writeFile(join(cacheDir, '.apvm.lock'), '');
  await writeFile(join(cacheDir, 'apvm.db'), 'not a database');
  const cache = ApvmCache.open();
  const report = await cache.repair();
  expect(report.buildsAdopted).toBe(BUILDS.length);
  return cache;
}

/** Index a cached release of backwpup `v5.6.8` (one asset) straight into the
 *  database — no public API stores releases offline. */
async function seedRelease(): Promise<string> {
  const { DatabaseSync } = await import('node:sqlite');
  const dir = join(cacheDir, 'backwpup', 'releases', 'v5.6.8');
  const file = join(dir, 'backwpup.zip');
  const content = 'release-asset';
  await mkdir(dir, { recursive: true });
  await writeFile(file, content);
  const db = new DatabaseSync(join(cacheDir, 'apvm.db'));
  try {
    const now = Date.now();
    const { lastInsertRowid } = db
      .prepare(
        `INSERT INTO releases (project, tag, dir_path, cached_at_ms, last_used_at_ms)
         VALUES ('backwpup', 'v5.6.8', 'backwpup/releases/v5.6.8', ?, ?)`,
      )
      .run(now, now);
    db.prepare(
      'INSERT INTO release_assets (release_id, filename, size_bytes, sha256) VALUES (?, ?, ?, ?)',
    ).run(lastInsertRowid, 'backwpup.zip', content.length, createHash('sha256').update(content).digest('hex'));
  } finally {
    db.close();
  }
  return file;
}

/** The rejection of `promise` (fails the test if it resolves). */
async function rejection(promise: Promise<unknown>): Promise<Error & { code?: string }> {
  try {
    await promise;
  } catch (err) {
    return err as Error & { code?: string };
  }
  throw new Error('expected the promise to reject');
}

// =============================================================================
// Opening — which directory
// =============================================================================

describe('ApvmCache directory', () => {
  it('open() uses APVM_CACHE_DIR over cacheDir', () => {
    expect(ApvmCache.open().dir()).toBe(cacheDir);
    expect(ApvmCache.open({ cacheDir: join(root, 'other') }).dir()).toBe(cacheDir);
  });

  it('open() uses cacheDir when APVM_CACHE_DIR is unset', () => {
    delete process.env.APVM_CACHE_DIR;
    const other = join(root, 'other');
    expect(ApvmCache.open({ cacheDir: other }).dir()).toBe(other);
  });

  it('apvm.cache() is the directory the instance builds into', async () => {
    const apvm = await Apvm.create({ cacheDir: join(root, 'ignored'), cacheEnabled: false });
    expect(apvm.cache()).toBeInstanceOf(ApvmCache);
    expect(apvm.cache().dir()).toBe(cacheDir);
    expect(apvm.cache().dir()).toBe(ApvmCache.open().dir());
  });

  it('a relative cacheDir is pinned at creation, so chdir cannot move it', async () => {
    delete process.env.APVM_CACHE_DIR;
    const cwd = process.cwd();
    try {
      await mkdir(join(root, 'a'));
      await mkdir(join(root, 'b'));
      process.chdir(join(root, 'a'));
      const apvm = await Apvm.create({ cacheDir: 'rel-cache' }); // caching on: creates a/rel-cache
      const fromInstance = apvm.cache();
      const opened = ApvmCache.open({ cacheDir: 'rel-cache' });
      expect(isAbsolute(fromInstance.dir())).toBe(true);
      expect(opened.dir()).toBe(fromInstance.dir());

      process.chdir(join(root, 'b'));
      expect((await fromInstance.info()).exists).toBe(true);
      expect((await opened.info()).exists).toBe(true);
      expect(await apvm.cacheStatus()).toEqual({ state: 'active' });
      expect(existsSync(join(root, 'b', 'rel-cache'))).toBe(false);
    } finally {
      process.chdir(cwd);
    }
  });

  it('APVM_CACHE_DIR set inside a worker thread is honored there', async () => {
    // Audit: the variable was read from the process environment, which a
    // worker's own `process.env` copy never reaches — the worker silently
    // used ~/.apvm/cache.
    const { Worker } = await import('node:worker_threads');
    const addon = createRequire(import.meta.url).resolve('../index.js');
    const workerDir = join(root, 'worker-cache');
    const worker = new Worker(
      `const { parentPort, workerData } = require('node:worker_threads');
       process.env.APVM_CACHE_DIR = workerData.dir;
       const { ApvmCache } = require(workerData.addon);
       parentPort.postMessage(ApvmCache.open().dir());`,
      { eval: true, workerData: { addon, dir: workerDir }, env: { ...process.env } },
    );
    const dir = await new Promise((resolve, reject) => {
      worker.once('message', resolve);
      worker.once('error', reject);
    });
    await worker.terminate();
    expect(dir).toBe(workerDir);
    expect(process.env.APVM_CACHE_DIR).toBe(cacheDir);
  });

  it('Apvm.create() reads APVM_CACHE_DIR when called, not when it resolves', async () => {
    const pending = Apvm.create({ cacheEnabled: false });
    process.env.APVM_CACHE_DIR = join(root, 'later');
    expect((await pending).cache().dir()).toBe(cacheDir);
  });

  it('apvm.cache() keeps the directory fixed at create()', async () => {
    const apvm = await Apvm.create({ cacheEnabled: false });
    process.env.APVM_CACHE_DIR = join(root, 'moved');
    expect(apvm.cache().dir()).toBe(cacheDir);
    expect(ApvmCache.open().dir()).toBe(join(root, 'moved'));
  });

  it('opening touches nothing on disk', async () => {
    ApvmCache.open();
    (await Apvm.create({ cacheEnabled: false })).cache();
    expect(existsSync(cacheDir)).toBe(false);
  });
});

// =============================================================================
// No cache yet
// =============================================================================

describe('missing cache directory', () => {
  it('every method resolves with an empty report and creates nothing', async () => {
    const cache = ApvmCache.open();

    const info = await cache.info();
    expect(info).toStrictEqual({
      cacheDir,
      exists: false,
      totalBytes: 0,
      buildsBytes: 0,
      releasesBytes: 0,
      buildCount: 0,
      releaseCount: 0,
      fileCount: 0,
      databaseBytes: 0,
      projects: [],
    });
    expect(await cache.clean()).toEqual({
      buildsDeleted: 0,
      releasesDeleted: 0,
      bytesFreed: 0,
      dryRun: false,
      failures: [],
    });
    expect((await cache.clean({ dryRun: true })).dryRun).toBe(true);
    expect(await cache.clear()).toStrictEqual({
      buildsDeleted: 0,
      releasesDeleted: 0,
      bytesFreed: 0,
      dryRun: false,
      failures: [],
    });
    expect((await cache.gc()).damagedArtifacts).toBe(0);
    expect((await cache.gc({ checksum: true })).failures).toEqual([]);
    expect(await cache.verify()).toEqual([]);
    expect(await cache.verify({ checksum: true })).toEqual([]);
    expect(await cache.repair()).toStrictEqual({
      rebuiltMissingDatabase: false,
      buildsAdopted: 0,
      artifactsAdopted: 0,
      entriesSkipped: 0,
      orphanReleaseDirs: 0,
    });

    expect(existsSync(cacheDir)).toBe(false);
  });

  it('an empty directory is no cache either, and stays empty', async () => {
    await mkdir(cacheDir);
    const cache = ApvmCache.open();
    expect((await cache.info()).exists).toBe(false);
    expect(await cache.verify()).toEqual([]);
    expect((await cache.repair()).buildsAdopted).toBe(0);
    expect(await readdir(cacheDir)).toEqual([]);
  });
});

// =============================================================================
// Seeded cache — every action
// =============================================================================

describe('seeded cache', () => {
  it('info() counts builds per project', async () => {
    const cache = await seed();
    const info = await cache.info();
    expect(info.exists).toBe(true);
    expect(info.cacheDir).toBe(cacheDir);
    expect(info.buildCount).toBe(2);
    expect(info.fileCount).toBe(2);
    expect(info.releaseCount).toBe(0);
    expect(info.buildsBytes).toBe(BUILDS[0].content.length + BUILDS[1].content.length);
    expect(info.totalBytes).toBe(info.buildsBytes);
    expect(info.databaseBytes).toBeGreaterThan(0);
    expect(new Date(info.oldestBuild ?? '').getTime()).not.toBeNaN();
    expect(new Date(info.newestBuild ?? '').getTime()).not.toBeNaN();
    expect(info.projects.map((p) => [p.project, p.buildCount]).sort()).toEqual([
      ['backwpup', 1],
      ['wp-rocket', 1],
    ]);
  });

  it('clean({ dryRun }) reports without deleting', async () => {
    const cache = await seed();
    const report = await cache.clean({ dryRun: true });
    expect(report).toMatchObject({ buildsDeleted: 2, dryRun: true, failures: [] });
    expect(report.bytesFreed).toBeGreaterThan(0);
    expect((await cache.info()).buildCount).toBe(2);
    for (const build of BUILDS) {
      expect(existsSync(buildFile(build))).toBe(true);
    }
  });

  it('clean({ project }) removes only that project', async () => {
    const cache = await seed();
    expect((await cache.clean({ project: 'wp-rocket' })).buildsDeleted).toBe(1);
    expect(existsSync(buildFile(BUILDS[0]))).toBe(false);
    expect(existsSync(buildFile(BUILDS[1]))).toBe(true);
    expect((await cache.info()).projects.map((p) => p.project)).toEqual(['backwpup']);
  });

  it('clean({ target }) selects the record kinds', async () => {
    const cache = await seed();
    expect((await cache.clean({ target: JsCleanTarget.Releases })).buildsDeleted).toBe(0);
    expect((await cache.clean({ target: JsCleanTarget.Builds })).buildsDeleted).toBe(2);
    expect((await cache.info()).buildCount).toBe(0);
  });

  it('clean({ olderThan }) keeps recently used builds', async () => {
    const cache = await seed();
    expect((await cache.clean({ olderThan: '30d' })).buildsDeleted).toBe(0);
    expect((await cache.info()).buildCount).toBe(2);
  });

  it('clear() removes everything but keeps the cache', async () => {
    const cache = await seed();
    expect(await cache.clear()).toMatchObject({ buildsDeleted: 2, dryRun: false, failures: [] });
    const info = await cache.info();
    expect(info.exists).toBe(true);
    expect(info.buildCount).toBe(0);
  });

  it('verify() finds a missing file, gc() removes its record', async () => {
    const cache = await seed();
    expect(await cache.verify()).toEqual([]);

    await unlink(buildFile(BUILDS[0]));
    expect(await cache.verify()).toEqual([
      {
        project: 'wp-rocket',
        kind: 'build',
        version: '3.17.0',
        commit: expect.stringMatching(/^abcdef1/),
        filename: 'wp-rocket.zip',
        path: expect.stringContaining('wp-rocket.zip'),
        problem: 'missing',
      },
    ]);

    const gc = await cache.gc();
    expect(gc.damagedArtifacts).toBe(1);
    expect(gc.staleBuildRows).toBe(1);
    expect(gc.failures).toEqual([]);
    expect(await cache.verify()).toEqual([]);
    expect((await cache.info()).buildCount).toBe(1);
  });

  it('verify() reports a size mismatch with both sizes', async () => {
    const cache = await seed();
    await writeFile(buildFile(BUILDS[0]), 'short');
    const [issue] = await cache.verify();
    expect(issue).toMatchObject({
      problem: 'size_mismatch',
      expectedSize: BUILDS[0].content.length,
      actualSize: 'short'.length,
    });
    expect((await cache.gc()).damagedArtifacts).toBe(1);
    expect(await cache.verify()).toEqual([]);
  });

  it('same-size corruption is seen and removed only with checksum', async () => {
    const cache = await seed();
    const file = buildFile(BUILDS[1]);
    const bytes = await readFile(file);
    bytes[0] ^= 0xff;
    await writeFile(file, bytes);

    expect(await cache.verify()).toEqual([]);
    const issues = await cache.verify({ checksum: true });
    expect(issues).toHaveLength(1);
    expect(issues[0]).toMatchObject({ project: 'backwpup', problem: 'checksum_mismatch' });
    expect(issues[0].expectedSha256).toMatch(/^[0-9a-f]{64}$/);
    expect(issues[0].actualSha256).toMatch(/^[0-9a-f]{64}$/);
    expect(issues[0].expectedSha256).not.toBe(issues[0].actualSha256);

    expect((await cache.gc()).damagedArtifacts).toBe(0);
    const gc = await cache.gc({ checksum: true });
    expect(gc.damagedArtifacts).toBe(1);
    expect(gc.damagedBytesRemoved).toBe(bytes.length);
    expect(existsSync(file)).toBe(false);
    expect(await cache.verify({ checksum: true })).toEqual([]);
  });

  it('concurrent calls on one handle all resolve', async () => {
    const cache = await seed();
    const results = await Promise.all(Array.from({ length: 8 }, () => cache.info()));
    for (const info of results) {
      expect(info.buildCount).toBe(2);
    }
  });

  it('concurrent mixed calls leave a healthy cache', async () => {
    const cache = await seed();
    const apvm = await Apvm.create({});
    const calls = Array.from({ length: 6 }, () => [
      cache.info(),
      cache.verify({ checksum: true }),
      cache.gc(),
      cache.clean({ dryRun: true }),
      cache.repair(),
      apvm.cacheStatus(),
      apvm.cache().info(),
    ]).flat();
    await Promise.all(calls);
    expect(await cache.verify({ checksum: true })).toStrictEqual([]);
    expect((await cache.info()).buildCount).toBe(2);
  });

  // Root reads through permission bits; Windows has none to set here.
  const permissionsBind = process.platform !== 'win32' && process.getuid?.() !== 0;
  it.skipIf(!permissionsBind)('an unreadable file is reported and kept by gc', async () => {
    const cache = await seed();
    const file = buildFile(BUILDS[0]);
    await chmod(file, 0o000);
    try {
      const issues = await cache.verify({ checksum: true });
      expect(issues).toHaveLength(1);
      expect(issues[0]).toMatchObject({ project: 'wp-rocket', problem: 'unreadable' });
      expect(issues[0].details).toBeTruthy();
      const gc = await cache.gc({ checksum: true });
      expect(gc.damagedArtifacts).toBe(0);
      expect(gc.failures).toHaveLength(1);
      expect(existsSync(file)).toBe(true);
    } finally {
      await chmod(file, 0o644);
    }
  });

  it('releases are counted, verified and cleaned by target', async () => {
    const cache = await seed();
    const asset = await seedRelease();
    const info = await cache.info();
    expect(info.releaseCount).toBe(1);
    expect(info.releasesBytes).toBe('release-asset'.length);

    await writeFile(asset, 'release-assex'); // same size, other content
    const [issue] = await cache.verify({ checksum: true });
    expect(issue).toMatchObject({
      project: 'backwpup',
      kind: 'release',
      tag: 'v5.6.8',
      filename: 'backwpup.zip',
      problem: 'checksum_mismatch',
    });
    expect(issue.version).toBeUndefined();
    expect(issue.commit).toBeUndefined();

    const report = await cache.clean({ target: JsCleanTarget.Releases });
    expect(report).toMatchObject({ buildsDeleted: 0, releasesDeleted: 1 });
    expect(existsSync(asset)).toBe(false);
    expect((await cache.info()).buildCount).toBe(2);
  });
});

// =============================================================================
// Error codes
// =============================================================================

describe('error codes', () => {
  it('a corrupt database rejects with CacheCorrupted; repair() fixes it once', async () => {
    const cache = await seed();
    await writeFile(join(cacheDir, 'apvm.db'), 'not a database');

    for (const call of [
      () => cache.info(),
      () => cache.clean({ dryRun: true }),
      () => cache.clear(),
      () => cache.gc(),
      () => cache.verify(),
    ]) {
      const err = await rejection(call());
      expect(err).toBeInstanceOf(Error);
      expect(err.code).toBe('CacheCorrupted');
      expect(err.message).toContain('call repair() to recover it');
    }

    const first = await cache.repair();
    expect(first.quarantinedDatabase).toContain('apvm.db.corrupt-');
    expect(existsSync(first.quarantinedDatabase ?? '')).toBe(true);
    expect(first.buildsAdopted).toBe(2);
    // Healthy now: a second repair does nothing.
    const second = await cache.repair();
    expect(second.quarantinedDatabase).toBeUndefined();
    expect(second.buildsAdopted).toBe(0);
    expect((await cache.info()).buildCount).toBe(2);
  });

  it('a lost database rejects with CacheCorrupted; repair() rebuilds it', async () => {
    const cache = await seed();
    for (const name of await readdir(cacheDir)) {
      if (name.startsWith('apvm.db')) {
        await rm(join(cacheDir, name));
      }
    }
    const err = await rejection(cache.info());
    expect(err.code).toBe('CacheCorrupted');
    expect(err.message).toContain('call repair() to re-index the cache from disk');

    const report = await cache.repair();
    expect(report.rebuiltMissingDatabase).toBe(true);
    expect(report.buildsAdopted).toBe(2);
    expect((await cache.info()).buildCount).toBe(2);
  });

  it('a blank database beside builds rejects with CacheCorrupted until repaired', async () => {
    const cache = await seed();
    for (const name of await readdir(cacheDir)) {
      if (name.startsWith('apvm.db')) {
        await rm(join(cacheDir, name));
      }
    }
    await writeFile(join(cacheDir, 'apvm.db'), '');
    for (const call of [() => cache.info(), () => cache.gc()]) {
      const err = await rejection(call());
      expect(err.code).toBe('CacheCorrupted');
      expect(err.message).toContain('missing, empty or half-repaired');
    }
    // Nothing was deleted or written meanwhile.
    expect(existsSync(buildFile(BUILDS[0]))).toBe(true);
    expect((await readFile(join(cacheDir, 'apvm.db'))).length).toBe(0);

    const report = await cache.repair();
    expect(report.rebuiltMissingDatabase).toBe(true);
    expect(report.buildsAdopted).toBe(2);
  });

  it('an interrupted repair leaves the cache needing repair, and repair completes it', async () => {
    // What a repair killed mid-way leaves: its marker, and an index holding
    // only part of the builds. gc must not trust it (it would delete the
    // builds the index does not list).
    const cache = await seed();
    const { DatabaseSync } = await import('node:sqlite');
    const db = new DatabaseSync(join(cacheDir, 'apvm.db'));
    try {
      db.prepare("DELETE FROM builds WHERE project = 'wp-rocket'").run();
    } finally {
      db.close();
    }
    await writeFile(join(cacheDir, 'apvm.db.repairing'), '');

    for (const call of [() => cache.info(), () => cache.gc()]) {
      expect((await rejection(call())).code).toBe('CacheCorrupted');
    }
    expect(existsSync(buildFile(BUILDS[0]))).toBe(true);

    const report = await cache.repair();
    expect(report.buildsAdopted).toBe(2);
    expect(existsSync(join(cacheDir, 'apvm.db.repairing'))).toBe(false);
    expect((await cache.info()).buildCount).toBe(2);
  });

  it.each([
    ['missing', false],
    ['existing', true],
  ])('bad clean() input rejects with InvalidArg on a %s cache', async (_label, seeded) => {
    const cache = seeded ? await seed() : ApvmCache.open();
    for (const options of [{ olderThan: '30y' }, { olderThan: 'soon' }, { project: 'Bad/Name' }]) {
      const err = await rejection(cache.clean(options));
      expect(err.code, JSON.stringify(options)).toBe('InvalidArg');
    }
    if (seeded) {
      expect((await cache.info()).buildCount).toBe(2);
    } else {
      expect(existsSync(cacheDir)).toBe(false);
    }
  });

  it('the code is defined on the error, whatever Error.prototype.code does', async () => {
    // Audit: assigning `code` instead of defining it would let an accessor
    // on Error.prototype swallow it, and no test noticed.
    const cache = await seed();
    await writeFile(join(cacheDir, 'apvm.db'), 'not a database');
    Object.defineProperty(Error.prototype, 'code', {
      get: () => undefined,
      set: () => undefined,
      configurable: true,
    });
    try {
      const err = await rejection(cache.info());
      expect(Object.getOwnPropertyDescriptor(err, 'code')?.value).toBe('CacheCorrupted');
    } finally {
      delete (Error.prototype as { code?: unknown }).code;
    }
  });

  it('an unknown target rejects with InvalidArg', async () => {
    const err = await rejection(ApvmCache.open().clean({ target: 'Nope' as JsCleanTarget }));
    expect(err.code).toBe('InvalidArg');
    expect(err.message).toContain('must be one of All, Builds, Releases');
  });

  it('validation runs before the corruption check', async () => {
    const cache = await seed();
    await writeFile(join(cacheDir, 'apvm.db'), 'not a database');
    expect((await rejection(cache.clean({ olderThan: '30y' }))).code).toBe('InvalidArg');
    expect((await rejection(cache.gc({ checksum: 'yes' } as never))).code).toBe('InvalidArg');
  });

  it('an empty cache path rejects with InvalidArg', async () => {
    delete process.env.APVM_CACHE_DIR;
    const err = await rejection(ApvmCache.open({ cacheDir: '' }).info());
    expect(err.code).toBe('InvalidArg');
  });

  it("someone else's directory is refused with a GenericFailure and left alone", async () => {
    await mkdir(join(cacheDir, 'my-plugin'), { recursive: true });
    const cache = ApvmCache.open();
    for (const call of [() => cache.info(), () => cache.gc(), () => cache.repair()]) {
      const err = await rejection(call());
      expect(err.code).toBe('GenericFailure');
      expect(err.message).toContain('Check the cache location');
    }
    expect(await readdir(cacheDir)).toEqual(['my-plugin']);
  });

  it('store-shaped data with a stray apvm.db but no lock file is never adopted', async () => {
    // Audit: a garbage `apvm.db` beside store-shaped data made someone
    // else's directory a "corrupt cache"; the recipe's repair() adopted it
    // and gc() then deleted the rest of the directory.
    for (const build of BUILDS) {
      await mkdir(join(buildFile(build), '..'), { recursive: true });
      await writeFile(buildFile(build), build.content);
    }
    await mkdir(join(cacheDir, 'mysite', 'releases', 'v1.0'), { recursive: true });
    await writeFile(join(cacheDir, 'mysite', 'releases', 'v1.0', 'notes.txt'), 'mine');
    await writeFile(join(cacheDir, 'apvm.db'), 'not a database');
    const before = (await readdir(cacheDir, { recursive: true })).sort();
    const cache = ApvmCache.open();
    for (const call of [() => cache.info(), () => cache.repair(), () => cache.gc()]) {
      const err = await rejection(call());
      expect(err.code).toBe('GenericFailure');
      expect(err.message).toContain('not an apvm cache');
    }
    expect((await readdir(cacheDir, { recursive: true })).sort()).toStrictEqual(before);
  });

  it('Apvm.create() never makes a cache of a directory holding other files', async () => {
    // Audit: the store was created among a user's files, where gc later
    // deleted store-shaped data the user put beside it.
    await mkdir(cacheDir, { recursive: true });
    await writeFile(join(cacheDir, 'notes.txt'), 'mine');
    const apvm = await Apvm.create({ cacheDir });
    const status = await apvm.cacheStatus();
    expect(status.state).toBe('unavailable');
    expect(status.reason).toContain("already contains 'notes.txt'");
    expect(await readdir(cacheDir)).toStrictEqual(['notes.txt']);
  });
});

// =============================================================================
// Apvm.cacheStatus()
// =============================================================================

describe('Apvm.cacheStatus()', () => {
  it('is active for a working cache', async () => {
    const apvm = await Apvm.create({});
    expect(await apvm.cacheStatus()).toStrictEqual({ state: 'active' });
  });

  it('is disabled when caching is off', async () => {
    const apvm = await Apvm.create({ cacheEnabled: false });
    const status = await apvm.cacheStatus();
    expect(status).toStrictEqual({ state: 'disabled' });
    expect('reason' in status).toBe(false);
  });

  it('reports a cache corrupt at create(), and is active after repair() on the same instance', async () => {
    await mkdir(cacheDir);
    await writeFile(join(cacheDir, 'apvm.db'), 'not a database');
    const apvm = await Apvm.create({});

    const status = await apvm.cacheStatus();
    expect(status.state).toBe('corrupted');
    expect(status.reason).toContain('not a database');

    await apvm.cache().repair();
    expect(await apvm.cacheStatus()).toStrictEqual({ state: 'active' });
  });

  it('is unavailable for a directory holding other data', async () => {
    await mkdir(join(cacheDir, 'my-plugin'), { recursive: true });
    const apvm = await Apvm.create({});
    const status = await apvm.cacheStatus();
    expect(status.state).toBe('unavailable');
    expect(status.reason).toContain('refusing to use');
  });
});

// =============================================================================
// Strict options — a mistake must never widen a destructive call
// =============================================================================

describe('strict options', () => {
  // `as never` passes what TypeScript would reject, as plain JS can.
  const badClean: Array<[string, unknown]> = [
    ['a string', '30d'],
    ['a boolean', true],
    ['an array', ['30d']],
    ['a misspelled key', { dryrun: true }],
    ['a snake_case key', { older_than: '30d' }],
    ['an undefined filter', { project: undefined }],
    ['a null filter', { olderThan: null }],
    ['an undefined dryRun', { dryRun: undefined }],
    ['a wrongly typed dryRun', { dryRun: 'yes' }],
    ['a wrongly typed project', { project: 42 }],
  ];

  it.each(badClean)('clean() rejects %s with InvalidArg and deletes nothing', async (_label, options) => {
    const cache = await seed();
    const err = await rejection(cache.clean(options as never));
    expect(err).toBeInstanceOf(Error);
    expect(err.code).toBe('InvalidArg');
    expect((await cache.info()).buildCount).toBe(2);
  });

  // Options are read from own properties: an object that is not plain could
  // carry them through its prototype, where they would be silently ignored.
  class GetterOptions {
    get dryRun(): boolean {
      return true;
    }
  }
  const notPlain: Array<[string, () => unknown]> = [
    ['Object.create(defaults)', () => Object.create({ dryRun: true })],
    ['a class instance with getters', () => new GetterOptions()],
    ['a Map', () => new Map([['dryRun', true]])],
    ['a Date', () => new Date()],
  ];

  it.each(notPlain)('clean() rejects %s with InvalidArg and deletes nothing', async (_label, make) => {
    const cache = await seed();
    const err = await rejection(cache.clean(make() as never));
    expect(err.code).toBe('InvalidArg');
    expect(err.message).toMatch(/must be a plain object/);
    expect((await cache.info()).buildCount).toBe(2);
  });

  class GetterConfig {
    get cacheDir(): string {
      return '/elsewhere';
    }
  }
  it.each([
    ['Object.create(defaults)', () => Object.create({ cacheDir: '/elsewhere' })],
    ['a class instance with getters', () => new GetterConfig()],
    ['a Map', () => new Map([['cacheDir', '/elsewhere']])],
  ])('ApvmCache.open() throws InvalidArg for %s, never opening the default cache', (_label, make) => {
    expect(() => ApvmCache.open(make() as never)).toThrow(expect.objectContaining({ code: 'InvalidArg' }));
  });

  it('clean() reads plain objects from any realm, and null-prototype ones', async () => {
    const { runInNewContext } = await import('node:vm');
    const foreignRealm = runInNewContext('({ dryRun: true })') as { dryRun: boolean };
    const nullPrototype = Object.assign(Object.create(null) as object, { dryRun: true });
    for (const options of [foreignRealm, nullPrototype]) {
      const cache = await seed();
      const report = await cache.clean(options);
      expect([report.dryRun, report.buildsDeleted]).toStrictEqual([true, 2]);
      expect((await cache.info()).buildCount).toBe(2);
    }
  });

  it('an exception thrown while reading the options propagates as is', async () => {
    const cache = await seed();
    const boom = new Error('boom');
    const options = {
      get dryRun(): boolean {
        throw boom;
      },
    };
    expect(() => cache.clean(options)).toThrow(boom);
    expect((await cache.info()).buildCount).toBe(2);
  });

  it('an enumerable addition to Object.prototype is not read as an option', async () => {
    // Audit: keys were listed with the prototype chain, so a polyfill's
    // enumerable `Object.prototype` property made every call reject.
    const cache = await seed();
    const proto = Object.prototype as Record<string, unknown>;
    proto.legacyHelper = () => undefined;
    try {
      expect((await cache.clean({ dryRun: true })).buildsDeleted).toBe(2);
      expect(ApvmCache.open({ cacheDir }).dir()).toBe(cacheDir);
    } finally {
      delete proto.legacyHelper;
    }
  });

  it('clean() accepts no options, undefined and null as "no filters"', async () => {
    for (const options of [undefined, null]) {
      const cache = await seed();
      expect((await cache.clean(options)).buildsDeleted).toBe(2);
    }
  });

  it.each([
    ['gc', 'checksum'],
    ['gc', { deep: true }],
    ['gc', { checksum: 'yes' }],
    ['verify', true],
    ['verify', { checksum: 1 }],
  ])('%s() rejects bad options %j with InvalidArg', async (method, options) => {
    const cache = await seed();
    const call = method === 'gc' ? cache.gc(options as never) : cache.verify(options as never);
    expect((await rejection(call)).code).toBe('InvalidArg');
  });

  it('gc() and verify() treat an undefined or null checksum as the default', async () => {
    const cache = await seed();
    expect((await cache.gc({ checksum: undefined })).damagedArtifacts).toBe(0);
    expect(await cache.verify({ checksum: null } as never)).toStrictEqual([]);
  });

  it.each([
    ['a string', '/some/dir'],
    ['an array', ['/some/dir']],
    ['a misspelled key', { cachedir: '/some/dir' }],
    ['an undefined cacheDir', { cacheDir: undefined }],
    ['a null cacheDir', { cacheDir: null }],
    ['a wrongly typed cacheDir', { cacheDir: 42 }],
  ])('ApvmCache.open() throws InvalidArg for %s', (_label, config) => {
    expect(() => ApvmCache.open(config as never)).toThrow(expect.objectContaining({ code: 'InvalidArg' }));
  });

  it('ApvmCache.open() accepts every ApvmConfig key, and no config at all', () => {
    for (const config of [undefined, null, {}, { cacheEnabled: false, githubToken: 'x' }]) {
      expect(ApvmCache.open(config).dir()).toBe(cacheDir);
    }
  });
});

// =============================================================================
// One copy of the addon per process
// =============================================================================

describe('a second copy of the addon', () => {
  it('is refused, so two SQLite libraries never share a cache', async () => {
    const require = createRequire(import.meta.url);
    require('../index.js');
    const loaded = Object.keys(require.cache).find((path) => path.endsWith('.node'));
    expect(loaded).toBeTruthy();
    ApvmCache.open(); // this copy is in use

    const copyPath = join(root, 'apvm-copy.node');
    copyFileSync(loaded as string, copyPath);
    const copy = require(copyPath) as typeof import('../index.js');
    expect(copy.ApvmCache).not.toBe(ApvmCache);
    for (const call of [
      () => copy.ApvmCache.open(),
      () => copy.Apvm.create({}),
      () => copy.Apvm.createWithTokenResolution({}),
    ]) {
      expect(call).toThrow(/second copy of apvm-napi/);
    }
    // The first copy keeps working.
    expect((await ApvmCache.open().info()).exists).toBe(false);
  });

  it('does not lock the addon out where globalThis cannot be extended', async () => {
    // Hardened environments freeze `globalThis`, where the guard cannot
    // record this copy: it is skipped instead of failing every entry point.
    const addon = createRequire(import.meta.url).resolve('../index.js');
    const script = join(root, 'frozen-global.cjs');
    await writeFile(
      script,
      `const { Apvm, ApvmCache } = require(${JSON.stringify(addon)});
      Object.freeze(globalThis);
      const cache = ApvmCache.open();
      Promise.all([cache.info(), Apvm.create({ cacheEnabled: false })]).then(
        ([usage]) => console.log(JSON.stringify({ dir: cache.dir(), exists: usage.exists })),
        (err) => { console.error(err); process.exitCode = 1; },
      );`,
    );
    const output = execFileSync(process.execPath, [script], {
      env: { ...process.env, APVM_CACHE_DIR: cacheDir },
      encoding: 'utf8',
    });
    expect(JSON.parse(output)).toStrictEqual({ dir: cacheDir, exists: false });
  });
});
