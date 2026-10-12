import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    testTimeout: 30_000,
    // Headroom for hooks on a loaded Windows runner (vitest's default is
    // 10 s): a hook that overruns its timeout fails the test, even when only
    // its cleanup was slow. Deleting temp dirs, the slow part, bounds itself
    // well inside this (`removeBestEffort`, __tests__/cleanup.ts).
    hookTimeout: 30_000,
    include: ['__tests__/**/*.test.ts'],
    // Redirect APVM's artifact cache to a temp dir so the suite never warms
    // the developer's real ~/.apvm/cache. See __tests__/setup.ts.
    setupFiles: ['./__tests__/setup.ts'],
  },
});
