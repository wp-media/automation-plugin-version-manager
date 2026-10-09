import { describe, expect, it } from 'vitest';
import { JsBuildPhase, JsCleanTarget, JsOutputStream, JsReleaseSelector } from '../index.js';

// The string enums are declared as runtime enums (`export declare enum`) via
// `constEnum: false` + `runtimeStringEnum: true` in package.json's `napi` config.
// A `const enum` cannot be read under `isolatedModules` (TS2748), so this file
// doubles as the `npm run typecheck` guard: it reads members as values.

/** Each enum with the exact member → value map its typings promise. */
const ENUMS: Array<[name: string, runtime: object, expected: Record<string, string>]> = [
  [
    'JsBuildPhase',
    JsBuildPhase,
    {
      Preflight: 'Preflight',
      Cache: 'Cache',
      ReleaseDownload: 'ReleaseDownload',
      Clone: 'Clone',
      Checkout: 'Checkout',
      DependencyCheck: 'DependencyCheck',
      PreBuild: 'PreBuild',
      Setup: 'Setup',
      Build: 'Build',
      BuildHook: 'BuildHook',
      PostBuild: 'PostBuild',
      CollectArtifacts: 'CollectArtifacts',
    },
  ],
  ['JsCleanTarget', JsCleanTarget, { All: 'All', Builds: 'Builds', Releases: 'Releases' }],
  ['JsOutputStream', JsOutputStream, { Stdout: 'Stdout', Stderr: 'Stderr' }],
  [
    'JsReleaseSelector',
    JsReleaseSelector,
    {
      LatestStable: 'LatestStable',
      PreviousStable: 'PreviousStable',
      Latest: 'Latest',
      PreviousLatest: 'PreviousLatest',
    },
  ],
];

describe('string enums', () => {
  it.each(ENUMS)('%s exposes every member at runtime with its string value', (_, runtime, expected) => {
    for (const [member, value] of Object.entries(expected)) {
      expect(Reflect.get(runtime, member)).toBe(value);
    }
  });

  it.each(ENUMS)('%s members are not enumerable (napi-rs limitation: never iterate)', (_, runtime) => {
    // napi-rs defines enum members with default (non-enumerable) attributes, so
    // `Object.values()` type-checks yet yields nothing. Read members by name.
    expect(Object.keys(runtime)).toEqual([]);
    expect(Object.values(runtime)).toEqual([]);
  });

  it('members are usable as typed values under isolatedModules', () => {
    const selector: JsReleaseSelector = JsReleaseSelector.Latest;
    const target: JsCleanTarget = JsCleanTarget.Builds;
    // A bare string stays a type error, so callers must go through the enum.
    // @ts-expect-error — 'Builds' is not assignable to JsCleanTarget.
    const bare: JsCleanTarget = 'Builds';
    expect([selector, target, bare]).toEqual(['Latest', 'Builds', 'Builds']);
  });
});
