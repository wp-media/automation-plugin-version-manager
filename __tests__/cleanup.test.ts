import { afterEach, describe, expect, it, vi } from 'vitest';
import { mkdtemp, mkdir, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { removeBestEffort } from './cleanup.js';

/**
 * Whether `path` exists.
 *
 * @param path - The path to check.
 * @returns `true` when it does.
 */
async function exists(path: string): Promise<boolean> {
  return stat(path).then(
    () => true,
    () => false,
  );
}

describe('removeBestEffort()', () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('removes a whole directory tree, quietly', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const dir = await mkdtemp(join(tmpdir(), 'apvm-cleanup-it-'));
    await mkdir(join(dir, 'a', 'b'), { recursive: true });
    await writeFile(join(dir, 'a', 'b', 'file'), 'x');

    expect(await removeBestEffort(dir, 'test')).toBe(true);
    expect(await exists(dir)).toBe(false);
    expect(warn).not.toHaveBeenCalled();
  });

  it('counts a path that is already gone as removed', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const missing = join(tmpdir(), `apvm-cleanup-missing-${process.pid}-${Date.now()}`);

    expect(await removeBestEffort(missing, 'test')).toBe(true);
    expect(warn).not.toHaveBeenCalled();
  });

  it('never throws when the removal fails, and says why', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const rejects = () => Promise.reject(new Error('EBUSY: resource busy'));
    const throws = () => {
      throw new Error('EPERM: operation not permitted');
    };

    expect(await removeBestEffort('/x', 'cache.test', { remove: rejects })).toBe(false);
    expect(await removeBestEffort('/y', 'cache.test', { remove: throws })).toBe(false);
    expect(warn.mock.calls.map(([message]) => String(message))).toStrictEqual([
      '[cache.test] could not remove /x: Error: EBUSY: resource busy',
      '[cache.test] could not remove /y: Error: EPERM: operation not permitted',
    ]);
  });

  it('gives up after its budget instead of outlasting a hook timeout', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const neverDone = () => new Promise<void>(() => {});
    const started = Date.now();

    expect(await removeBestEffort('/z', 'setup', { budgetMs: 50, remove: neverDone })).toBe(false);
    expect(Date.now() - started).toBeLessThan(2_000);
    expect(warn).toHaveBeenCalledWith('[setup] could not remove /z: still not done after 50 ms');
  });
});
