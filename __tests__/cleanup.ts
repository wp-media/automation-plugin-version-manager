import { rm } from 'node:fs/promises';

/**
 * Longest a best-effort cleanup waits: room for `rm`'s whole retry schedule
 * (about 5.5 s of back-off), and well inside the 30 s `hookTimeout`, which
 * fails a hook that overruns it even when the hook is synchronous.
 */
export const CLEANUP_BUDGET_MS = 10_000;

/** How the directory is removed (injectable for tests). */
export type Remove = (path: string) => Promise<void>;

/**
 * Remove `path` recursively, retrying while Windows still holds one of its
 * files (an SQLite database whose `Apvm` the GC has not freed yet, a `.node`
 * file a child process just loaded, a fresh file being scanned).
 *
 * @param path - What to remove.
 * @returns Settles once removed (or already gone); rejects once retries run out.
 */
const removeWithRetries: Remove = (path) =>
  rm(path, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });

/**
 * Remove a temp directory, best-effort: a cleanup must never fail a test.
 * It never throws, and gives up after `budgetMs`, warning either way it
 * fails; the OS reclaims what is left in its temp dir. A removal that is
 * still running when it gives up carries on in the background, its outcome
 * ignored.
 *
 * @param path - The directory (or file) to remove.
 * @param label - Who cleans up, named in the warning.
 * @param options.budgetMs - Longest wait for the removal.
 * @param options.remove - How to remove it.
 * @returns Whether `path` was removed in time.
 */
export async function removeBestEffort(
  path: string,
  label: string,
  { budgetMs = CLEANUP_BUDGET_MS, remove = removeWithRetries }: { budgetMs?: number; remove?: Remove } = {},
): Promise<boolean> {
  let timer: NodeJS.Timeout | undefined;
  // Through `then`, so even a synchronous throw becomes a handled failure.
  const removal = Promise.resolve()
    .then(() => remove(path))
    .then(
      () => null,
      (err: unknown) => String(err),
    );
  const deadline = new Promise<string>((resolve) => {
    timer = setTimeout(() => resolve(`still not done after ${budgetMs} ms`), budgetMs);
    // Never keeps the process alive on its own.
    timer.unref();
  });
  const failure = await Promise.race([removal, deadline]);
  clearTimeout(timer);
  if (failure !== null) {
    console.warn(`[${label}] could not remove ${path}: ${failure}`);
  }
  return failure === null;
}
