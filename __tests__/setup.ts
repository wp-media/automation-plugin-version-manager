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
  rmSync(cacheDir, { recursive: true, force: true });
});
