import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterAll } from 'vitest';

// Isolate APVM's artifact cache for the whole test run.
//
// Several tests construct `Apvm.create({})` with no `cacheDir`, which would
// otherwise default to the developer's real ~/.apvm/cache — reading from and
// warming it. Pointing APVM_CACHE_DIR at a throwaway directory redirects the
// cache for every Apvm instance created in this worker (the Rust core reads
// APVM_CACHE_DIR at Apvm.create() time, and setup files run in the same worker
// process as the tests). The tests never touch the real cache as a result.
const cacheDir = mkdtempSync(join(tmpdir(), 'apvm-test-cache-'));
process.env.APVM_CACHE_DIR = cacheDir;

afterAll(() => {
  // Best-effort removal of the throwaway cache dir.
  //
  // The Rust core opens a WAL-mode SQLite store (apvm.db + -wal/-shm) and holds
  // it open for the lifetime of each Apvm instance; napi frees those instances
  // on GC, which is non-deterministic, so their file handles may still be open
  // here. On Windows an open handle blocks deletion, surfacing as EPERM/EBUSY.
  //
  // `maxRetries` lets Node retry with backoff once the handles are released;
  // if it still can't remove the dir, we warn instead of failing an otherwise
  // green run — it's a temp dir the OS reclaims regardless. (POSIX unlinks the
  // files immediately, so this path effectively never triggers off Windows.)
  try {
    rmSync(cacheDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  } catch (err) {
    console.warn(`[apvm test setup] could not remove temp cache dir ${cacheDir}: ${String(err)}`);
  }
});
