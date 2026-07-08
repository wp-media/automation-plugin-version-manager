import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    testTimeout: 30_000,
    include: ['__tests__/**/*.test.ts'],
    // Redirect APVM's artifact cache to a temp dir so the suite never warms
    // the developer's real ~/.apvm/cache. See __tests__/setup.ts.
    setupFiles: ['./__tests__/setup.ts'],
  },
});
