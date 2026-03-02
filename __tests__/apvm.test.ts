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
