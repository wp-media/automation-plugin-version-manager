import { describe, it, expect } from 'vitest';
import { Apvm } from '../index.js';
import { mkdtemp, readdir, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

// =============================================================================
// Apvm.create() — Async factory
// =============================================================================

describe('Apvm.create()', () => {
  it('creates an instance with empty config (temp builds dir)', async () => {
    const apvm = await Apvm.create({});
    expect(apvm).toBeInstanceOf(Apvm);
  });
  it('creates an instance without passing config (Should create default)', async () => {
    const apvm = await Apvm.create();
    expect(apvm).toBeInstanceOf(Apvm);
  });
  it('creates an instance with explicit buildsDir', async () => {
    const apvm = await Apvm.create({ buildsDir: '/tmp/apvm-test-builds' });
    expect(apvm).toBeInstanceOf(Apvm);
  });

  it('creates an instance with buildsDir and githubToken', async () => {
    const apvm = await Apvm.create({
      buildsDir: '/tmp/apvm-test-builds',
      githubToken: 'ghp_test_fake_token_value',
    });
    expect(apvm).toBeInstanceOf(Apvm);
  });

  it('creates an instance with only githubToken (temp builds dir)', async () => {
    const apvm = await Apvm.create({ githubToken: 'ghp_test_fake_token_value' });
    expect(apvm).toBeInstanceOf(Apvm);
  });
});

// =============================================================================
// Apvm.createWithTokenResolution() — Async factory
// =============================================================================

describe('Apvm.createWithTokenResolution()', () => {
  it('creates an instance with empty config', async () => {
    const apvm = await Apvm.createWithTokenResolution({});
    expect(apvm).toBeInstanceOf(Apvm);
  });

  it('creates an instance with explicit buildsDir', async () => {
    const apvm = await Apvm.createWithTokenResolution({
      buildsDir: '/tmp/apvm-test-builds',
    });
    expect(apvm).toBeInstanceOf(Apvm);
  });

  it('creates an instance with explicit token (skips resolution)', async () => {
    const apvm = await Apvm.createWithTokenResolution({
      githubToken: 'ghp_test_fake_token_value',
    });
    expect(apvm).toBeInstanceOf(Apvm);
    expect(apvm.hasToken()).toBe(true);
    expect(apvm.tokenSource()).toBe('config file');
  });
});

// =============================================================================
// Token methods
// =============================================================================

describe('Token methods', () => {
  it('hasToken() returns false when no token provided', async () => {
    const apvm = await Apvm.create({});
    expect(apvm.hasToken()).toBe(false);
  });

  it('hasToken() returns true when token provided in config', async () => {
    const apvm = await Apvm.create({ githubToken: 'ghp_test_token' });
    expect(apvm.hasToken()).toBe(true);
  });

  it('tokenSource() returns null when no token', async () => {
    const apvm = await Apvm.create({});
    expect(apvm.tokenSource()).toBeNull();
  });

  it('tokenSource() returns "config file" when token set via config', async () => {
    const apvm = await Apvm.create({ githubToken: 'ghp_test_token' });
    expect(apvm.tokenSource()).toBe('config file');
  });
});

// =============================================================================
// Project listing
// =============================================================================

describe('listProjects()', () => {
  it('returns an array of project names', async () => {
    const apvm = await Apvm.create({});
    const projects = apvm.listProjects();
    expect(Array.isArray(projects)).toBe(true);
    expect(projects.length).toBeGreaterThan(0);
  });

  it('includes known projects', async () => {
    const apvm = await Apvm.create({});
    const projects = apvm.listProjects();
    expect(projects).toContain('wp-rocket');
    expect(projects).toContain('backwpup');
  });

  it('returns strings for all entries', async () => {
    const apvm = await Apvm.create({});
    const projects = apvm.listProjects();
    for (const project of projects) {
      expect(typeof project).toBe('string');
    }
  });
});

// =============================================================================
// Build process — basic functionality (not error handling)
// =============================================================================
describe('build() basic functionality', () => {
    it('builds wp-rocket from develop and writes artifacts to a unique temp output directory', async () => {
    let tempRoot = '';

    try {
      tempRoot = await mkdtemp(join(tmpdir(), 'apvm-build-it-'));
      const outputDir = join(tempRoot, 'output');
      const apvm = await Apvm.create({});

      const output = await apvm.buildFromBranch('wp-rocket', 'develop', outputDir);

      expect(output).toBeTruthy();
      expect(output.result).toBeTruthy();
      expect(output.result.artifacts.length).toBeGreaterThan(0);
      expect(output.description.length).toBeGreaterThan(0);
      expect(output.commit.length).toBeGreaterThan(0);
      expect(output.commitShort.length).toBeGreaterThan(0);

      const outputDirResolved = resolve(outputDir);
      const artifactsInOutput = await readdir(outputDir);
      expect(artifactsInOutput.length).toBeGreaterThan(0);

      for (const artifact of output.result.artifacts) {
        expect(artifact.filename.length).toBeGreaterThan(0);
        const expectedOutputPath = join(outputDir, artifact.filename);
        const expectedOutputPathResolved = resolve(expectedOutputPath);
        expect(expectedOutputPathResolved.startsWith(outputDirResolved)).toBe(true);

        const fileStat = await stat(expectedOutputPath);
        expect(fileStat.isFile()).toBe(true);
        expect(fileStat.size).toBeGreaterThan(0);
      }
    } finally {
      if (tempRoot) {
        await rm(tempRoot, { recursive: true, force: true });
      }
    }
  }, 10 * 60_000);
});
// =============================================================================
// Build error handling
// =============================================================================

describe('build() error handling', () => {
  it('rejects with error for unknown project', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.build({
        project: 'nonexistent-plugin',
        gitRef: 'pr:1',
        outputDir: '/tmp/output',
      }),
    ).rejects.toThrow();
  });

  it('rejects with error for empty project name', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.build({
        project: '',
        gitRef: 'pr:1',
        outputDir: '/tmp/output',
      }),
    ).rejects.toThrow();
  });
});

// =============================================================================
// Convenience build methods — error handling
// =============================================================================

describe('Convenience build methods error handling', () => {
  it('buildFromPr() rejects for unknown project', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.buildFromPr('nonexistent-plugin', 1, '/tmp/output'),
    ).rejects.toThrow();
  });

  it('buildFromBranch() rejects for unknown project', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.buildFromBranch('nonexistent-plugin', 'main', '/tmp/output'),
    ).rejects.toThrow();
  });

  it('buildFromTag() rejects for unknown project', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.buildFromTag('nonexistent-plugin', 'v1.0.0', '/tmp/output'),
    ).rejects.toThrow();
  });

  it('buildFromCommit() rejects for unknown project', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.buildFromCommit('nonexistent-plugin', 'abc1234', '/tmp/output'),
    ).rejects.toThrow();
  });
});

// =============================================================================
// Build options validation — edge cases
// =============================================================================

describe('build() options validation', () => {
  it('rejects with error for empty outputDir', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.build({
        project: 'wp-rocket',
        gitRef: 'pr:1',
        outputDir: '',
      }),
    ).rejects.toThrow();
  });

  it('rejects with error for empty gitRef', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.build({
        project: 'wp-rocket',
        gitRef: '',
        outputDir: '/tmp/output',
      }),
    ).rejects.toThrow();
  });
});

// =============================================================================
// Convenience build methods — argument types
// =============================================================================

describe('Convenience build methods accept optional params', () => {
  it('buildFromPr() accepts version and variants', async () => {
    const apvm = await Apvm.create({});
    // Should fail because project doesn't exist, but shows the signature works
    await expect(
      apvm.buildFromPr('nonexistent-plugin', 1, '/tmp/output', '5.1.0', ['free']),
    ).rejects.toThrow();
  });

  it('buildFromBranch() accepts version and variants', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.buildFromBranch('nonexistent-plugin', 'main', '/tmp/output', '5.1.0', ['free']),
    ).rejects.toThrow();
  });

  it('buildFromTag() accepts version and variants', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.buildFromTag('nonexistent-plugin', 'v1.0.0', '/tmp/output', '5.1.0', ['free']),
    ).rejects.toThrow();
  });

  it('buildFromCommit() accepts version and variants', async () => {
    const apvm = await Apvm.create({});
    await expect(
      apvm.buildFromCommit('nonexistent-plugin', 'abc1234', '/tmp/output', '5.1.0', ['free']),
    ).rejects.toThrow();
  });
});

// =============================================================================
// Project listing — structure validation
// =============================================================================

describe('listProjects() structure', () => {
  it('returns at least 2 known projects', async () => {
    const apvm = await Apvm.create({});
    const projects = apvm.listProjects();
    expect(projects.length).toBeGreaterThanOrEqual(2);
  });

  it('project names are non-empty strings', async () => {
    const apvm = await Apvm.create({});
    const projects = apvm.listProjects();
    for (const p of projects) {
      expect(p.length).toBeGreaterThan(0);
      expect(p.trim()).toBe(p);
    }
  });
});

// =============================================================================
// Token resolution — environment variables
// =============================================================================

describe('createWithTokenResolution() token sources', () => {
  it('picks up GITHUB_TOKEN env var', async () => {
    const original = process.env.GITHUB_TOKEN;
    try {
      process.env.GITHUB_TOKEN = 'ghp_env_test_token';
      // Pass no config token so env var resolution is reached
      const apvm = await Apvm.createWithTokenResolution({});
      expect(apvm.hasToken()).toBe(true);
      expect(apvm.tokenSource()).toBe('GITHUB_TOKEN env');
    } finally {
      if (original !== undefined) {
        process.env.GITHUB_TOKEN = original;
      } else {
        delete process.env.GITHUB_TOKEN;
      }
    }
  });

  it('explicit token overrides env var', async () => {
    const original = process.env.GITHUB_TOKEN;
    try {
      process.env.GITHUB_TOKEN = 'ghp_env_token';
      const apvm = await Apvm.createWithTokenResolution({
        githubToken: 'ghp_explicit_token',
      });
      expect(apvm.hasToken()).toBe(true);
      expect(apvm.tokenSource()).toBe('config file');
    } finally {
      if (original !== undefined) {
        process.env.GITHUB_TOKEN = original;
      } else {
        delete process.env.GITHUB_TOKEN;
      }
    }
  });
});

// =============================================================================
// Apvm.create() — config edge cases
// =============================================================================

describe('Apvm.create() config edge cases', () => {
  it('creates with undefined config', async () => {
    const apvm = await Apvm.create(undefined);
    expect(apvm).toBeInstanceOf(Apvm);
  });

  it('creates with null config', async () => {
    const apvm = await Apvm.create(null);
    expect(apvm).toBeInstanceOf(Apvm);
  });
});
