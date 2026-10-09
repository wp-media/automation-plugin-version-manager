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

  it.each(ENUMS)('%s iterates exactly its members, in declaration order', (_, runtime, expected) => {
    // Since napi-derive-backend 6.1.5 members are enumerable data properties,
    // like a compiled TypeScript enum (earlier versions hid them). Exact
    // entries also prove there are no extra or missing keys.
    expect(Object.entries(runtime)).toEqual(Object.entries(expected));
  });

  it.each(ENUMS)('%s members are plain data properties, like a compiled TypeScript enum', (_, runtime, expected) => {
    // tsc emits `E["A"] = "A"`: writable, enumerable, configurable. The docs
    // promise that shape, so pin every attribute, not just enumerability.
    const plain = Object.fromEntries(
      Object.entries(expected).map(([member, value]) => [
        member,
        { value, writable: true, enumerable: true, configurable: true },
      ]),
    );
    expect(Object.getOwnPropertyDescriptors(runtime)).toStrictEqual(plain);
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
