import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { JsOwnershipPolicy, SiteInventory } from '../index.js';
import type { JsDefaults, JsGroup, JsServer, JsSite } from '../index.js';
import { execFile, spawn, spawnSync } from 'node:child_process';
import { accessSync, constants, existsSync, readFileSync, statSync } from 'node:fs';
import { createRequire } from 'node:module';
import { chmod, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { promisify } from 'node:util';
import { Worker } from 'node:worker_threads';
import { removeBestEffort } from './cleanup.js';

// SiteInventory — the site inventory from Node, fully offline. Every test
// works on files under its own temp directory, and APVM_INVENTORY points
// there too, so a call that falls back to the default location can never
// reach the developer's ~/.apvm/inventory.json (setup.ts isolates the whole
// run the same way). Other processes are real Node processes loading the
// same addon, as a parallel test worker or a long-running tool would.

/** The suite-wide value from `setup.ts`, restored after each test. */
const suiteInventory = process.env.APVM_INVENTORY;
/** The addon's `index.js`, for workers and child processes. */
const ADDON = createRequire(import.meta.url).resolve('../index.js');
/** Longest a child process may run: under the 30 s `testTimeout`. */
const CHILD_TIMEOUT_MS = 20_000;
/** Timeout of the tests that wait out the 10 s inventory lock. */
const LOCK_TEST_TIMEOUT_MS = 25_000;
/** Windows has no POSIX modes and needs privileges for symbolic links. */
const isWindows = process.platform === 'win32';

let root = '';
/** This test's inventory file (absent until a test writes it). */
let file = '';

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), 'apvm-inventory-it-'));
  file = join(root, 'inventory.json');
  process.env.APVM_INVENTORY = join(root, 'env', 'inventory.json');
});

afterEach(async () => {
  if (suiteInventory === undefined) {
    delete process.env.APVM_INVENTORY;
  } else {
    process.env.APVM_INVENTORY = suiteInventory;
  }
  await removeBestEffort(root, 'sites-inventory.test');
});

/** A server with every field set. */
const SERVER: JsServer = {
  id: 'eu-1',
  name: 'EU VPS',
  host: '203.0.113.10',
  ssh: {
    user: 'ubuntu',
    port: 2222,
    identityFile: '~/.ssh/eu1_ed25519',
    connectTimeoutSecs: 20,
    knownHostsFile: '~/.ssh/known_hosts_apvm',
    sshConfigFile: '~/.ssh/config_apvm',
    proxyJump: 'bastion',
    multiplex: false,
  },
  notes: 'primary',
};

/** A site with every field set. */
const SITE: JsSite = {
  id: 'blog',
  name: 'Blog',
  server: 'eu-1',
  path: '/var/www/blog',
  url: 'https://blog.example.com',
  wpCli: ['/usr/bin/php8.3', '/opt/wp-cli.phar'],
  runAs: 'login',
  owner: 'ftpuser:www-data',
  ownership: JsOwnershipPolicy.MirrorRoot,
  pluginsDir: '/var/www/blog/app/plugins',
  allowRoot: false,
  remoteTmpDir: '/var/tmp',
  commandTimeoutSecs: 600,
  notes: 'ftpuser:www-data',
};

/** A group with every field set. */
const GROUP: JsGroup = { id: 'qa', sites: ['blog'], notes: 'QA' };

/** Defaults with every SSH field a default may set. */
const DEFAULTS: JsDefaults = {
  ssh: {
    user: 'deploy',
    port: 22,
    identityFile: '~/.ssh/id_ed25519',
    connectTimeoutSecs: 15,
    knownHostsFile: '~/.ssh/known_hosts',
    sshConfigFile: '~/.ssh/config',
    proxyJump: 'jump.example.com',
    multiplex: true,
    binary: '/usr/bin/ssh',
  },
};

/** The error `run` throws (fails the test if it returns). */
function thrown(run: () => unknown): Error & { code?: string } {
  try {
    run();
  } catch (err) {
    return err as Error & { code?: string };
  }
  throw new Error('expected the call to throw');
}

/** Ids of the servers in the inventory file at `path`, read afresh. */
function serverIds(path: string = file): string[] {
  return SiteInventory.load(path)
    .servers()
    .map((server) => server.id);
}

/** An inventory file at `path` holding one server per id (written with `update`). */
function seed(ids: string[], path: string = file): void {
  SiteInventory.update((inventory) => {
    for (const id of ids) inventory.upsertServer({ id, host: `${id}.example.com` });
  }, path);
}

/**
 * Run `script` (CommonJS) in a fresh Node process under the strict Node-API
 * exception policy, so an exception the addon leaves pending crashes it.
 * `ADDON` holds the addon's path; `FILE` this test's inventory file.
 *
 * @param script - The script body.
 * @param env - Extra environment variables for the child.
 * @returns The finished child process.
 */
function runNode(script: string, env: NodeJS.ProcessEnv = {}): ReturnType<typeof spawnSync> {
  const prelude = `const ADDON = ${JSON.stringify(ADDON)}; const FILE = ${JSON.stringify(file)};\n`;
  return spawnSync(process.execPath, ['--force-node-api-uncaught-exceptions-policy=true', '-e', prelude + script], {
    encoding: 'utf8',
    timeout: CHILD_TIMEOUT_MS,
    env: { ...process.env, ...env },
  });
}

/**
 * Settle `promise`, or reject once `ms` have passed.
 *
 * @param promise - What to wait for.
 * @param ms - The limit.
 * @param what - Named in the timeout error.
 */
async function within<T>(promise: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${what} did not finish within ${ms} ms`)), ms);
  });
  try {
    return await Promise.race([promise, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Start a Node process that holds the lock of `path` (inside an `update()`
 * callback) until `${path}.stop` exists, at most `CHILD_TIMEOUT_MS`. It
 * signals with a file, `${path}.locked`: on macOS a write to a piped
 * stdout is asynchronous, and the holder's busy loop would delay it.
 *
 * @param path - The inventory file to lock.
 * @returns Once the lock is held: a function that ends the hold and
 *   resolves with the holder's exit code.
 */
async function holdLock(path: string): Promise<() => Promise<number | null>> {
  const [locked, stop] = [`${path}.locked`, `${path}.stop`];
  const script = `const { existsSync, writeFileSync } = require('node:fs');
    const { SiteInventory } = require(${JSON.stringify(ADDON)});
    const deadline = Date.now() + ${CHILD_TIMEOUT_MS};
    SiteInventory.update(() => {
      writeFileSync(${JSON.stringify(locked)}, '');
      while (!existsSync(${JSON.stringify(stop)}) && Date.now() < deadline) {}
    }, ${JSON.stringify(path)});`;
  const child = spawn(process.execPath, ['-e', script], { stdio: 'inherit' });
  const exited = new Promise<number | null>((resolve) => child.once('exit', resolve));
  const release = async () => {
    await writeFile(stop, '');
    return within(exited, CHILD_TIMEOUT_MS, 'the lock holder');
  };
  const deadline = Date.now() + CHILD_TIMEOUT_MS;
  while (!existsSync(locked)) {
    if (child.exitCode !== null || Date.now() > deadline) {
      await release();
      throw new Error(`the lock holder never took the lock (exit code ${child.exitCode})`);
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  return release;
}

/** `runNode`, expecting a clean exit; returns what the child printed. */
function runNodeOk(script: string, env: NodeJS.ProcessEnv = {}): string {
  const child = runNode(script, env);
  expect({ status: child.status, signal: child.signal, stderr: child.stderr }).toStrictEqual({
    status: 0,
    signal: null,
    stderr: '',
  });
  return String(child.stdout);
}

// =============================================================================
// Loading and the inventory's location
// =============================================================================

describe('SiteInventory.load() and empty()', () => {
  it('load() of a missing file is an empty inventory and creates nothing', () => {
    const inventory = SiteInventory.load(file);
    expect(inventory.path).toBe(file);
    expect(inventory.readOnly).toBe(false);
    expect(inventory.servers()).toEqual([]);
    expect(inventory.sites()).toEqual([]);
    expect(inventory.groups()).toEqual([]);
    expect(inventory.defaults()).toEqual({});
    expect(existsSync(file)).toBe(false);
    expect(existsSync(file + '.lock')).toBe(false);
  });

  it('empty() is tied to no file', () => {
    const inventory = SiteInventory.empty();
    expect(inventory.path).toBeNull();
    expect(inventory.servers()).toEqual([]);
    expect(inventory.readOnly).toBe(false);
  });

  it('every field round-trips through the file, in the file format', () => {
    const inventory = SiteInventory.load(file);
    inventory.setDefaults(DEFAULTS);
    inventory.upsertServer(SERVER);
    inventory.upsertSite(SITE);
    inventory.upsertGroup(GROUP);
    expect(inventory.save()).toBe(file);

    const loaded = SiteInventory.load(file);
    expect(loaded.defaults()).toStrictEqual(DEFAULTS);
    expect(loaded.servers()).toStrictEqual([SERVER]);
    expect(loaded.sites()).toStrictEqual([SITE]);
    expect(loaded.groups()).toStrictEqual([GROUP]);

    // The CLI's format: snake_case, the policy as written in the file.
    const json = JSON.parse(readFileSync(file, 'utf8'));
    expect(json.version).toBe(1);
    expect(json.servers[0].ssh).toMatchObject({ identity_file: '~/.ssh/eu1_ed25519', connect_timeout_secs: 20 });
    expect(json.sites[0]).toMatchObject({
      wp_cli: ['/usr/bin/php8.3', '/opt/wp-cli.phar'],
      run_as: 'login',
      ownership: 'mirror-root',
      command_timeout_secs: 600,
    });
  });

  it('unset fields are absent, in records and in the file', () => {
    const inventory = SiteInventory.load(file);
    inventory.upsertServer({ id: 'eu-1', host: 'h', name: undefined, notes: null as never, ssh: {} });
    inventory.upsertSite({ id: 'shop', server: 'eu-1', path: '/var/www/shop', ownership: undefined });
    expect(inventory.servers()).toStrictEqual([{ id: 'eu-1', host: 'h' }]);
    expect(inventory.sites()).toStrictEqual([{ id: 'shop', server: 'eu-1', path: '/var/www/shop' }]);
    inventory.save();
    expect(JSON.parse(readFileSync(file, 'utf8')).servers).toStrictEqual([{ id: 'eu-1', host: 'h' }]);
  });

  it.skipIf(isWindows)('save() writes the file 0600 in a 0700 directory it creates', () => {
    const nested = join(root, 'new', 'dir', 'inventory.json');
    SiteInventory.empty().save(nested);
    expect(statSync(nested).mode & 0o777).toBe(0o600);
    expect(statSync(join(root, 'new', 'dir')).mode & 0o777).toBe(0o700);
    expect(statSync(join(root, 'new')).mode & 0o777).toBe(0o700);
  });

  it('a relative path is made absolute when loaded, so chdir changes nothing', async () => {
    const cwd = process.cwd();
    await mkdir(join(root, 'a'));
    await mkdir(join(root, 'b'));
    process.chdir(join(root, 'a'));
    try {
      const inventory = SiteInventory.load('inventory.json');
      expect(inventory.path).toBe(join(process.cwd(), 'inventory.json'));
      process.chdir(join(root, 'b'));
      inventory.upsertServer({ id: 'eu-1', host: 'h' });
      expect(inventory.save()).toBe(inventory.path);
      expect(existsSync(join(root, 'b', 'inventory.json'))).toBe(false);
    } finally {
      process.chdir(cwd);
    }
    expect(existsSync(join(root, 'a', 'inventory.json'))).toBe(true);
  });

  it('APVM_INVENTORY is read from process.env at each call', () => {
    const first = join(root, 'first.json');
    const second = join(root, 'second.json');
    process.env.APVM_INVENTORY = first;
    SiteInventory.update((inventory) => inventory.upsertServer({ id: 'one', host: 'h' }));
    expect(SiteInventory.load().path).toBe(first);
    expect(serverIds(first)).toEqual(['one']);

    process.env.APVM_INVENTORY = second;
    const fresh = SiteInventory.empty();
    fresh.upsertServer({ id: 'two', host: 'h' });
    expect(fresh.save()).toBe(second);
    expect(SiteInventory.load().servers().map((s) => s.id)).toEqual(['two']);
    // A tracked object keeps its file whatever the variable says now.
    const loaded = SiteInventory.load(first);
    process.env.APVM_INVENTORY = join(root, 'third.json');
    expect(loaded.save()).toBe(first);
  });

  it('a relative APVM_INVENTORY is made absolute when read', () => {
    const cwd = process.cwd();
    process.chdir(root);
    try {
      process.env.APVM_INVENTORY = 'rel.json';
      expect(SiteInventory.load().path).toBe(join(process.cwd(), 'rel.json'));
    } finally {
      process.chdir(cwd);
    }
  });

  it('APVM_INVENTORY set inside a worker thread is honored there', async () => {
    const workerFile = join(root, 'worker.json');
    const worker = new Worker(
      `const { parentPort, workerData } = require('node:worker_threads');
       process.env.APVM_INVENTORY = workerData.file;
       const { SiteInventory } = require(workerData.addon);
       const inventory = SiteInventory.empty();
       inventory.upsertServer({ id: 'w', host: 'h' });
       parentPort.postMessage(inventory.save());`,
      { eval: true, workerData: { addon: ADDON, file: workerFile }, env: { ...process.env } },
    );
    let saved: unknown;
    try {
      saved = await within(
        new Promise((resolve, reject) => {
          worker.once('message', resolve);
          worker.once('error', reject);
        }),
        CHILD_TIMEOUT_MS,
        'the worker',
      );
    } finally {
      await worker.terminate();
    }
    expect(saved).toBe(workerFile);
    expect(serverIds(workerFile)).toEqual(['w']);
    expect(process.env.APVM_INVENTORY).toBe(join(root, 'env', 'inventory.json'));
  });

  it.skipIf(isWindows)('without a path or APVM_INVENTORY, the file is ~/.apvm/inventory.json', async () => {
    // A child with its own HOME, so the real home is never touched. An empty
    // APVM_INVENTORY counts as unset.
    const home = join(root, 'home');
    await mkdir(home);
    const out = runNodeOk(
      `const { SiteInventory } = require(ADDON);
       const inventory = SiteInventory.empty();
       inventory.upsertServer({ id: 'eu-1', host: 'h' });
       console.log(JSON.stringify({ saved: inventory.save(), loaded: SiteInventory.load().path }));`,
      { HOME: home, APVM_INVENTORY: '' },
    );
    const expected = join(home, '.apvm', 'inventory.json');
    expect(JSON.parse(out)).toStrictEqual({ saved: expected, loaded: expected });
    expect(statSync(join(home, '.apvm')).mode & 0o777).toBe(0o700);
  });

  it('an unreadable process.env falls back to the process environment, without a pending exception', () => {
    const out = runNodeOk(
      `const { SiteInventory } = require(ADDON);
       Object.defineProperty(process, 'env', { get() { throw new Error('env getter threw'); } });
       const first = SiteInventory.load().path;
       console.log(JSON.stringify({ first, second: SiteInventory.load().path }));`,
      { APVM_INVENTORY: file },
    );
    expect(JSON.parse(out)).toStrictEqual({ first: file, second: file });
  });
});

// =============================================================================
// Reading the file: errors
// =============================================================================

describe('file errors carry err.code', () => {
  it('an invalid inventory is InventoryInvalid, with the location', async () => {
    await writeFile(file, JSON.stringify({ version: 1, sites: [{ id: 'shop', server: 'eu-2', path: '/p' }] }));
    const err = thrown(() => SiteInventory.load(file));
    expect(err).toBeInstanceOf(Error);
    expect(err.code).toBe('InventoryInvalid');
    expect(err.message).toContain("sites[0].server: unknown server 'eu-2'");
    expect(err.message).toContain(file);
  });

  it('text that is not JSON, or an unknown field, is InventoryInvalid', async () => {
    await writeFile(file, '{ "version": 1, ');
    expect(thrown(() => SiteInventory.load(file)).code).toBe('InventoryInvalid');
    await writeFile(file, JSON.stringify({ version: 1, site: [] }));
    expect(thrown(() => SiteInventory.load(file)).message).toContain('unknown field `site`');
  });

  it('a newer inventory this build cannot read is InventoryUnsupported', async () => {
    await writeFile(file, JSON.stringify({ version: 2, future: true }));
    const err = thrown(() => SiteInventory.load(file));
    expect(err.code).toBe('InventoryUnsupported');
    expect(err.message).toContain('upgrade apvm');
  });

  it('a directory at the path is InventoryIo', async () => {
    await mkdir(file);
    expect(thrown(() => SiteInventory.load(file)).code).toBe('InventoryIo');
    expect(thrown(() => SiteInventory.empty().save(file)).code).toBe('InventoryIo');
  });

  it.skipIf(isWindows)('a symbolic link is refused with InventoryIo, never followed', async () => {
    const target = join(root, 'target.json');
    seed(['eu-1'], target);
    await symlink(target, file);
    const err = thrown(() => SiteInventory.load(file));
    expect(err.code).toBe('InventoryIo');
    expect(err.message).toContain('symbolic link');
  });

  it('a path that names no file is InventoryIo', () => {
    expect(thrown(() => SiteInventory.load('')).code).toBe('InventoryIo');
    expect(thrown(() => SiteInventory.load(root + '/')).code).toBe('InventoryIo');
  });

  it('a path ending with `.` or `..` is refused as given, before it is made absolute (as the CLI does)', async () => {
    // Made absolute first, `x/.` would become `x` (and on Windows `x/..` the
    // parent): a file the CLI would refuse to touch.
    const sep = isWindows ? '\\' : '/';
    const x = join(root, 'x');
    await writeFile(x, JSON.stringify({ version: 1 }));
    const before = readFileSync(x, 'utf8');
    for (const path of [`${x}${sep}.`, `${x}${sep}..`, `${x}/.`, `${root}${sep}..`]) {
      const calls: Array<() => unknown> = [
        () => SiteInventory.load(path),
        () => SiteInventory.empty().save(path),
        () => SiteInventory.empty().save(path, { force: true }),
        () => SiteInventory.load(file).save(path),
        () => SiteInventory.update(() => expect.unreachable('fn must not run'), path),
      ];
      for (const call of calls) {
        const err = thrown(call);
        expect(err.code, `${path}: ${err.message}`).toBe('InventoryIo');
        expect(err.message).toContain('the path names no file');
      }
    }
    // The default location is checked the same way.
    process.env.APVM_INVENTORY = `${x}${sep}.`;
    expect(thrown(() => SiteInventory.load()).message).toContain('the path names no file');
    expect(readFileSync(x, 'utf8')).toBe(before);
    expect(existsSync(x + '.lock')).toBe(false);
  });

  it('a file name ending with `.` or a space is refused everywhere (Windows would drop it)', () => {
    // On Windows `inv.json.` opens `inv.json` but locks `inv.json..lock`:
    // two writers could both get in. Refused on every platform alike.
    for (const name of ['inventory.json.', 'inventory.json ', 'x..']) {
      const path = join(root, name);
      const calls: Array<() => unknown> = [
        () => SiteInventory.load(path),
        () => SiteInventory.empty().save(path, { force: true }),
        () => SiteInventory.update(() => expect.unreachable('fn must not run'), path),
      ];
      for (const call of calls) {
        const err = thrown(call);
        expect(err.code, err.message).toBe('InventoryIo');
        expect(err.message).toContain("the file name ends with '.' or a space");
      }
    }
    process.env.APVM_INVENTORY = join(root, 'inventory.json.');
    expect(thrown(() => SiteInventory.load()).code).toBe('InventoryIo');
  });

  it('a refused target gets no lock file: a directory, or a symbolic link', async () => {
    await mkdir(file);
    const calls: Array<() => unknown> = [
      () => SiteInventory.empty().save(file),
      () => SiteInventory.empty().save(file, { force: true }),
      () => SiteInventory.update(() => expect.unreachable('fn must not run'), file),
    ];
    for (const call of calls) expect(thrown(call).code).toBe('InventoryIo');
    expect(existsSync(file + '.lock')).toBe(false);
    if (isWindows) return; // symbolic links need a privilege there
    const link = join(root, 'link.json');
    seed(['eu-1'], join(root, 'target.json'));
    await symlink(join(root, 'target.json'), link);
    for (const call of [
      () => SiteInventory.empty().save(link, { force: true }),
      () => SiteInventory.update(() => expect.unreachable('fn must not run'), link),
    ]) {
      expect(thrown(call).message).toContain('symbolic link');
    }
    expect(existsSync(link + '.lock')).toBe(false);
  });
});

// =============================================================================
// Editing: records are validated
// =============================================================================

describe('editing', () => {
  it('upsert adds a record, then replaces it whole', () => {
    const inventory = SiteInventory.empty();
    inventory.upsertServer(SERVER);
    inventory.upsertServer({ id: 'eu-1', host: 'eu1.example.com' });
    expect(inventory.servers()).toStrictEqual([{ id: 'eu-1', host: 'eu1.example.com' }]);
    inventory.upsertSite(SITE);
    inventory.upsertSite({ ...SITE, notes: 'changed' });
    expect(inventory.sites()).toStrictEqual([{ ...SITE, notes: 'changed' }]);
    inventory.upsertGroup(GROUP);
    inventory.upsertGroup({ id: 'qa', sites: [] });
    expect(inventory.groups()).toStrictEqual([{ id: 'qa', sites: [] }]);
  });

  it('records are returned as copies', () => {
    const inventory = SiteInventory.empty();
    inventory.upsertServer(SERVER);
    const [server] = inventory.servers();
    server.host = 'changed';
    server.ssh!.port = 1;
    expect(inventory.servers()).toStrictEqual([SERVER]);
  });

  it('setDefaults replaces the defaults; {} clears them', () => {
    const inventory = SiteInventory.empty();
    inventory.setDefaults(DEFAULTS);
    expect(inventory.defaults()).toStrictEqual(DEFAULTS);
    inventory.setDefaults({ ssh: { multiplex: false } });
    expect(inventory.defaults()).toStrictEqual({ ssh: { multiplex: false } });
    inventory.setDefaults({});
    expect(inventory.defaults()).toStrictEqual({});
  });

  it('remove returns false for a missing record', () => {
    const inventory = SiteInventory.empty();
    expect(inventory.removeServer('nope')).toBe(false);
    expect(inventory.removeSite('nope')).toBe(false);
    expect(inventory.removeGroup('nope')).toBe(false);
  });

  it('removing a site takes it out of its groups; a group removal keeps its sites', () => {
    const inventory = SiteInventory.empty();
    inventory.upsertServer(SERVER);
    inventory.upsertSite(SITE);
    inventory.upsertSite({ id: 'shop', server: 'eu-1', path: '/var/www/shop' });
    inventory.upsertGroup({ id: 'qa', sites: ['blog', 'shop'] });
    expect(inventory.removeSite('blog')).toBe(true);
    expect(inventory.groups()).toStrictEqual([{ id: 'qa', sites: ['shop'] }]);
    expect(inventory.removeGroup('qa')).toBe(true);
    expect(inventory.sites().map((s) => s.id)).toEqual(['shop']);
  });

  it('a server with sites is removed only with cascade', () => {
    const inventory = SiteInventory.empty();
    inventory.upsertServer(SERVER);
    inventory.upsertSite(SITE);
    inventory.upsertGroup(GROUP);
    const err = thrown(() => inventory.removeServer('eu-1'));
    expect(err.code).toBe('InvalidArg');
    expect(err.message).toContain("server 'eu-1' is used by 1 site(s): 'blog'");
    expect(err.message).toContain('Pass cascade = true');
    expect(inventory.servers()).toHaveLength(1);
    expect(thrown(() => inventory.removeServer('eu-1', false)).code).toBe('InvalidArg');
    expect(inventory.removeServer('eu-1', true)).toBe(true);
    expect(inventory.sites()).toEqual([]);
    expect(inventory.groups()).toStrictEqual([{ id: 'qa', sites: [], notes: 'QA' }]);
  });

  it('values the inventory rules refuse are InvalidArg, and change nothing', () => {
    const inventory = SiteInventory.empty();
    inventory.upsertServer({ id: 'eu-1', host: 'h' });
    const refusals: Array<[() => void, string]> = [
      [() => inventory.upsertServer({ id: 'EU', host: 'h' }), "invalid server 'EU': id:"],
      [() => inventory.upsertServer({ id: 'x', host: '-oProxyCommand=sh' }), "invalid server 'x': host:"],
      [() => inventory.upsertServer({ id: 'x', host: 'h', ssh: { port: 0 } }), 'ssh.port'],
      [() => inventory.upsertServer({ id: 'x', host: 'h', ssh: { binary: '/usr/bin/ssh' } }), 'ssh.binary'],
      [() => inventory.upsertSite({ id: 's', server: 'eu-2', path: '/p' }), "unknown server 'eu-2'"],
      [() => inventory.upsertSite({ id: 's', server: 'eu-1', path: 'var/www' }), 'path:'],
      [() => inventory.upsertSite({ id: 's', server: 'eu-1', path: '/p', runAs: 'root' }), 'run_as'],
      [() => inventory.upsertSite({ id: 's', server: 'eu-1', path: '/p', wpCli: ['sudo', 'wp'] }), 'wp_cli'],
      [() => inventory.upsertGroup({ id: 'g', sites: ['nope'] }), "unknown site 'nope'"],
      [() => inventory.setDefaults({ ssh: { connectTimeoutSecs: 0 } }), 'connect_timeout_secs'],
    ];
    for (const [refused, expected] of refusals) {
      const err = thrown(refused);
      expect(err.code, err.message).toBe('InvalidArg');
      expect(err.message).toContain(expected);
    }
    expect(inventory.servers()).toStrictEqual([{ id: 'eu-1', host: 'h' }]);
    expect(inventory.sites()).toEqual([]);
    expect(inventory.groups()).toEqual([]);
    expect(inventory.defaults()).toEqual({});
  });

  it('records are read strictly: unknown keys, wrong types and non-plain objects are InvalidArg', () => {
    const inventory = SiteInventory.empty();
    inventory.upsertServer({ id: 'eu-1', host: 'h' });
    class ServerClass {
      id = 'x';
      host = 'h';
    }
    const cases: Array<[unknown, string]> = [
      [{ id: 'x', host: 'h', prot: 22 }, 'upsertServer(): unknown field `prot`'],
      [{ id: 'x', host: 'h', ssh: { prot: 22 } }, 'upsertServer(): unknown field `ssh.prot`'],
      [{ id: 'x', host: 'h', ssh: { port: '22' } }, '`ssh.port` must be a number, got a string'],
      [{ id: 'x', host: 'h', ssh: { port: 70000 } }, '`ssh.port` must be a whole number from 0 to 65535, got 70000'],
      [{ id: 'x', host: 'h', ssh: { port: 22.5 } }, 'got 22.5'],
      [{ id: 'x', host: 'h', ssh: { port: -1 } }, 'got -1'],
      [{ id: 'x', host: 'h', ssh: { port: Number.NaN } }, 'got NaN'],
      [{ id: 'x', host: 'h', ssh: { multiplex: 'yes' } }, '`ssh.multiplex` must be a boolean, got a string'],
      [{ id: 'x', host: 'h', ssh: [] }, 'upsertServer() `ssh` must be an object, got an array'],
      [{ id: 'x', host: 'h', ssh: 'bastion' }, 'upsertServer() `ssh` must be an object, got a string'],
      [{ id: 'x' }, 'upsertServer(): `host` is required'],
      [{ id: 'x', host: null }, 'upsertServer(): `host` is required'],
      [{ id: 1, host: 'h' }, '`id` must be a string, got a number'],
      [{ id: 'x', host: 'h', identity_file: '~/.ssh/k' }, 'unknown field `identity_file`'],
      [new ServerClass(), 'must be a plain object'],
      [new Map([['id', 'x']]), 'must be a plain object'],
      [Object.create({ id: 'x', host: 'h' }), 'must be a plain object'],
      [[], 'upsertServer() must be an object, got an array'],
      ['eu-1', 'upsertServer() must be an object, got a string'],
      [undefined, 'upsertServer() must be an object, got undefined'],
    ];
    for (const [record, expected] of cases) {
      const err = thrown(() => inventory.upsertServer(record as JsServer));
      expect(err.code, err.message).toBe('InvalidArg');
      expect(err.message).toContain(expected);
    }
    expect(inventory.servers()).toStrictEqual([{ id: 'eu-1', host: 'h' }]);
  });

  it('site and group fields are type-checked too', () => {
    const inventory = SiteInventory.empty();
    inventory.upsertServer({ id: 'eu-1', host: 'h' });
    const base = { id: 's', server: 'eu-1', path: '/p' };
    const siteCases: Array<[unknown, string]> = [
      [{ ...base, ownership: 'Mirror' }, "`ownership`: unknown ownership policy 'Mirror'"],
      [{ ...base, ownership: 1 }, '`ownership` must be a string, got a number'],
      [{ ...base, wpCli: 'wp' }, '`wpCli` must be an array of strings, got a string'],
      [{ ...base, wpCli: ['wp', 1] }, '`wpCli[1]` must be a string, got a number'],
      [{ ...base, allowRoot: 'false' }, '`allowRoot` must be a boolean, got a string'],
      [{ ...base, commandTimeoutSecs: 2 ** 32 }, '`commandTimeoutSecs` must be a whole number from 0 to 4294967295'],
      [{ ...base, owenrship: 'check' }, 'unknown field `owenrship`'],
      [{ id: 's', server: 'eu-1' }, '`path` is required'],
    ];
    for (const [record, expected] of siteCases) {
      const err = thrown(() => inventory.upsertSite(record as JsSite));
      expect(err.code, err.message).toBe('InvalidArg');
      expect(err.message).toContain(expected);
    }
    const groupCases: Array<[unknown, string]> = [
      [{ id: 'g' }, 'upsertGroup(): `sites` is required'],
      [{ id: 'g', sites: 'blog' }, '`sites` must be an array of strings, got a string'],
      [{ id: 'g', sites: [null] }, '`sites[0]` must be a string, got null'],
      [{ id: 'g', sites: [], members: [] }, 'unknown field `members`'],
    ];
    for (const [record, expected] of groupCases) {
      const err = thrown(() => inventory.upsertGroup(record as JsGroup));
      expect(err.code, err.message).toBe('InvalidArg');
      expect(err.message).toContain(expected);
    }
    expect(thrown(() => inventory.setDefaults({ ssh: { usr: 'x' } } as never)).message).toContain(
      'setDefaults(): unknown field `ssh.usr`',
    );
    expect(inventory.sites()).toEqual([]);
  });

  it('non-enumerable own keys are refused, known or unknown, at any depth', () => {
    const inventory = SiteInventory.empty();
    const hidden = (record: object, key: string, value: unknown) =>
      Object.defineProperty({ ...record }, key, { value, enumerable: false });
    const cases: Array<[() => void, string]> = [
      [
        () => inventory.upsertServer(hidden({ id: 'x', host: 'h' }, 'notes', 'n') as JsServer),
        'upsertServer(): field `notes` is not enumerable',
      ],
      [
        () => inventory.upsertServer(hidden({ id: 'x' }, 'host', 'h') as JsServer),
        'upsertServer(): field `host` is not enumerable',
      ],
      [
        () => inventory.upsertServer({ id: 'x', host: 'h', ssh: hidden({}, 'port', 22) } as JsServer),
        'upsertServer(): field `ssh.port` is not enumerable',
      ],
      [
        () => inventory.upsertSite(hidden({ id: 's', server: 'eu-1', path: '/p' }, 'ownership', 'check') as JsSite),
        'upsertSite(): field `ownership` is not enumerable',
      ],
      [
        () => inventory.upsertServer(hidden({ id: 'x', host: 'h' }, 'bogus', 1) as JsServer),
        'upsertServer(): unknown field `bogus`',
      ],
      [
        () => inventory.save(file, hidden({}, 'force', true) as never),
        'save() options: field `force` is not enumerable',
      ],
    ];
    for (const [call, expected] of cases) {
      const err = thrown(call);
      expect(err.code, err.message).toBe('InvalidArg');
      expect(err.message).toContain(expected);
    }
    // Symbol keys are not fields: ignored.
    inventory.upsertServer({ id: 'x', host: 'h', [Symbol('meta')]: 1 } as JsServer);
    expect(inventory.servers()).toStrictEqual([{ id: 'x', host: 'h' }]);
    expect(existsSync(file)).toBe(false);
  });

  it('list fields must be plain arrays: extra own properties are refused', () => {
    const inventory = SiteInventory.empty();
    inventory.upsertServer({ id: 'eu-1', host: 'h' });
    inventory.upsertSite({ id: 's', server: 'eu-1', path: '/p' });
    const cases: Array<[() => void, string]> = [
      [
        () => inventory.upsertSite({ id: 't', server: 'eu-1', path: '/p', wpCli: Object.assign(['wp'], { extra: 1 }) }),
        'upsertSite(): `wpCli` must be a plain array of strings, got one with the property `extra`',
      ],
      [
        () => inventory.upsertGroup({ id: 'g', sites: Object.defineProperty(['s'], 'hidden', { value: 1 }) }),
        'upsertGroup(): `sites` must be a plain array of strings, got one with the property `hidden`',
      ],
    ];
    for (const [call, expected] of cases) {
      const err = thrown(call);
      expect(err.code, err.message).toBe('InvalidArg');
      expect(err.message).toContain(expected);
    }
    expect(inventory.sites().map((site) => site.id)).toEqual(['s']);
    expect(inventory.groups()).toEqual([]);
  });

  it('a lone surrogate in a string becomes U+FFFD; ASCII-only fields then refuse it', () => {
    const inventory = SiteInventory.empty();
    inventory.upsertServer({ id: 'x', host: 'h', notes: 'a\uD800b' });
    expect(inventory.servers()[0].notes).toBe('a\uFFFDb');
    const err = thrown(() => inventory.upsertServer({ id: 'y\uDC00', host: 'h' }));
    expect(err.code).toBe('InvalidArg');
    expect(err.message).toContain('\uFFFD');
  });

  it('arguments of the wrong type are InvalidArg', () => {
    const inventory = SiteInventory.empty();
    const cases: Array<[() => unknown, string]> = [
      [() => SiteInventory.load(42 as never), 'SiteInventory.load(): `path` must be a string, got a number'],
      [() => inventory.save(42 as never), 'save(): `path` must be a string, got a number'],
      [() => inventory.save(file, { forse: true } as never), 'save() options: unknown field `forse`'],
      [() => inventory.save(file, { force: 'yes' } as never), '`force` must be a boolean, got a string'],
      [() => inventory.save(file, true as never), 'save() options must be an object, got a boolean'],
      [() => inventory.removeSite(1 as never), 'removeSite(): `id` must be a string, got a number'],
      [() => inventory.removeServer('x', 'yes' as never), 'removeServer(): `cascade` must be a boolean'],
      [() => inventory.removeGroup(undefined as never), 'removeGroup(): `id` must be a string, got undefined'],
      [() => SiteInventory.update('fn' as never, file), 'SiteInventory.update(): `fn` must be a function, got a string'],
      [() => SiteInventory.update(() => {}, 1 as never), 'SiteInventory.update(): `path` must be a string'],
    ];
    for (const [call, expected] of cases) {
      const err = thrown(call);
      expect(err.code, err.message).toBe('InvalidArg');
      expect(err.message).toContain(expected);
    }
    expect(existsSync(file)).toBe(false);
  });

  it('`new SiteInventory()` is refused: instances come from the factories', () => {
    const Ctor = SiteInventory as unknown as new () => SiteInventory;
    expect(() => new Ctor()).toThrow();
  });
});

// =============================================================================
// Saving: no lost edits
// =============================================================================

describe('save()', () => {
  it('refuses with InventoryChanged when another process saved since load', () => {
    seed(['eu-1']);
    const inventory = SiteInventory.load(file);
    inventory.upsertServer({ id: 'mine', host: 'h' });

    runNodeOk(
      `const { SiteInventory } = require(ADDON);
       SiteInventory.update((inventory) => inventory.upsertServer({ id: 'theirs', host: 'h' }), FILE);`,
    );
    const before = readFileSync(file, 'utf8');
    const err = thrown(() => inventory.save());
    expect(err.code).toBe('InventoryChanged');
    expect(err.message).toContain('changed after it was loaded; nothing was written');
    expect(err.message).toContain('Load it again, reapply your edits and save.');
    // Forcing would discard their edit: never offered as the fix.
    expect(err.message).not.toContain('force');

    // The file keeps their edit; the object keeps its own.
    expect(readFileSync(file, 'utf8')).toBe(before);
    expect(serverIds()).toEqual(['eu-1', 'theirs']);
    expect(inventory.servers().map((s) => s.id)).toEqual(['eu-1', 'mine']);
    // Still refused: the object still expects what it loaded.
    expect(thrown(() => inventory.save()).code).toBe('InventoryChanged');

    // Reload, reapply, save: both edits land.
    const reloaded = SiteInventory.load(file);
    reloaded.upsertServer({ id: 'mine', host: 'h' });
    expect(reloaded.save()).toBe(file);
    expect(serverIds()).toEqual(['eu-1', 'theirs', 'mine']);
  });

  it('refuses when the file was deleted, or created, since load', async () => {
    seed(['eu-1']);
    const loaded = SiteInventory.load(file);
    await rm(file);
    expect(thrown(() => loaded.save()).code).toBe('InventoryChanged');
    expect(existsSync(file)).toBe(false);

    const other = join(root, 'other.json');
    const missing = SiteInventory.load(other);
    seed(['theirs'], other);
    missing.upsertServer({ id: 'mine', host: 'h' });
    expect(thrown(() => missing.save()).code).toBe('InventoryChanged');
    expect(serverIds(other)).toEqual(['theirs']);
  });

  it('a rewrite with the same content is not a conflict', () => {
    seed(['eu-1']);
    const inventory = SiteInventory.load(file);
    SiteInventory.load(file).save(); // same bytes
    inventory.upsertServer({ id: 'mine', host: 'h' });
    expect(inventory.save()).toBe(file);
    expect(serverIds()).toEqual(['eu-1', 'mine']);
  });

  it('without force: true, every spelling of the options is a guarded save', () => {
    seed(['eu-1']);
    const inventory = SiteInventory.load(file);
    seed(['theirs']);
    const before = readFileSync(file, 'utf8');
    inventory.upsertServer({ id: 'mine', host: 'h' });
    const spellings: Array<() => string> = [
      () => inventory.save(),
      () => inventory.save(undefined, undefined),
      () => inventory.save(undefined, null),
      () => inventory.save(undefined, {}),
      () => inventory.save(undefined, { force: undefined }),
      () => inventory.save(undefined, { force: null } as never),
      () => inventory.save(undefined, { force: false }),
      () => inventory.save(file, {}),
    ];
    for (const save of spellings) {
      expect(thrown(save).code).toBe('InventoryChanged');
      expect(readFileSync(file, 'utf8')).toBe(before);
    }
  });

  it('{ force: true } overwrites, and later guarded saves chain from it', () => {
    seed(['eu-1']);
    const inventory = SiteInventory.load(file);
    seed(['theirs']);
    inventory.upsertServer({ id: 'mine', host: 'h' });
    expect(thrown(() => inventory.save()).code).toBe('InventoryChanged');
    expect(inventory.save(undefined, { force: true })).toBe(file);
    expect(serverIds()).toEqual(['eu-1', 'mine']);
    inventory.upsertServer({ id: 'next', host: 'h' });
    expect(inventory.save(null, {})).toBe(file);
    expect(serverIds()).toEqual(['eu-1', 'mine', 'next']);
  });

  it('{ force: true } replaces whatever the file holds, even an unreadable one', async () => {
    await writeFile(file, '{ not json');
    expect(thrown(() => SiteInventory.load(file)).code).toBe('InventoryInvalid');
    const backup = SiteInventory.empty();
    backup.upsertServer({ id: 'restored', host: 'h' });
    expect(thrown(() => backup.save(file)).code).toBe('InventoryChanged');
    expect(backup.save(file, { force: true })).toBe(file);
    expect(serverIds()).toEqual(['restored']);
  });

  it('{ force: true } never replaces an inventory a newer apvm wrote, readable here or not', async () => {
    const backup = SiteInventory.empty();
    backup.upsertServer({ id: 'restored', host: 'h' });
    // The codes are those update() gives for the same file.
    const newer: Array<[string, string]> = [
      [JSON.stringify({ version: 2, servers: [{ id: 'eu-1', host: 'h' }] }), 'InventoryReadOnly'],
      [JSON.stringify({ version: 2, future: true }), 'InventoryUnsupported'],
    ];
    for (const [content, code] of newer) {
      await writeFile(file, content);
      const err = thrown(() => backup.save(file, { force: true }));
      expect(err.code).toBe(code);
      expect(err.message).toContain('To restore a backup over it, move that file aside by hand first');
      expect(thrown(() => SiteInventory.update((inv) => inv.upsertServer({ id: 'x', host: 'h' }), file)).code).toBe(code);
      expect(readFileSync(file, 'utf8')).toBe(content);
      expect(backup.path).toBeNull();
    }
    // Moved aside by hand, the backup is restored.
    await rm(file);
    expect(backup.save(file, { force: true })).toBe(file);
    expect(serverIds()).toEqual(['restored']);
  });

  it.skipIf(isWindows)('{ force: true } refuses a file it cannot read: it might be a newer inventory', async () => {
    seed(['eu-1']);
    const before = readFileSync(file, 'utf8');
    await chmod(file, 0o000);
    try {
      try {
        accessSync(file, constants.R_OK);
        return; // permissions are not enforced (running as root?)
      } catch {
        // Unreadable, as intended.
      }
      const backup = SiteInventory.empty();
      backup.upsertServer({ id: 'restored', host: 'h' });
      const err = thrown(() => backup.save(file, { force: true }));
      expect(err.code).toBe('InventoryIo');
      expect(err.message).toContain('cannot check the format version (before a forced save)');
      expect(err.message).toContain('make the file readable, or move it aside by hand, then save again.');
    } finally {
      await chmod(file, 0o600);
    }
    expect(readFileSync(file, 'utf8')).toBe(before);
  });

  it('empty().save() never replaces an existing inventory', () => {
    seed(['eu-1']);
    const before = readFileSync(file, 'utf8');
    const fresh = SiteInventory.empty();
    fresh.upsertServer({ id: 'new', host: 'h' });
    const err = thrown(() => fresh.save(file));
    expect(err.code).toBe('InventoryChanged');
    expect(readFileSync(file, 'utf8')).toBe(before);
    expect(fresh.path).toBeNull();

    // The default location is "another file" too.
    process.env.APVM_INVENTORY = file;
    expect(thrown(() => fresh.save()).code).toBe('InventoryChanged');
    expect(readFileSync(file, 'utf8')).toBe(before);
  });

  it('empty().save() creates a missing file and then tracks it', () => {
    const fresh = SiteInventory.empty();
    fresh.upsertServer({ id: 'a', host: 'h' });
    expect(fresh.save(file)).toBe(file);
    expect(fresh.path).toBe(file);
    fresh.upsertServer({ id: 'b', host: 'h' });
    expect(fresh.save()).toBe(file);
    expect(serverIds()).toEqual(['a', 'b']);
  });

  it('saves on one object chain', () => {
    const inventory = SiteInventory.load(file);
    for (const id of ['a', 'b', 'c', 'd']) {
      inventory.upsertServer({ id, host: 'h' });
      expect(inventory.save()).toBe(file);
    }
    expect(inventory.removeServer('b')).toBe(true);
    inventory.save();
    expect(serverIds()).toEqual(['a', 'c', 'd']);
  });

  it('a save to another file expects it missing, then the object tracks that file', () => {
    seed(['eu-1']);
    const inventory = SiteInventory.load(file);
    const copy = join(root, 'copy.json');
    expect(inventory.save(copy)).toBe(copy);
    expect(inventory.path).toBe(copy);
    inventory.upsertServer({ id: 'mine', host: 'h' });
    inventory.save();
    expect(serverIds(copy)).toEqual(['eu-1', 'mine']);
    expect(serverIds(file)).toEqual(['eu-1']);

    // Back to the original: not the tracked file any more, and it exists.
    expect(thrown(() => inventory.save(file)).code).toBe('InventoryChanged');
  });

  it('the tracked file given as absolute or relative is the same file', () => {
    seed(['eu-1']);
    const cwd = process.cwd();
    process.chdir(root);
    try {
      // The current directory's own spelling (macOS reports /private/var/…
      // for a /var/… temp dir).
      const absolutePath = join(process.cwd(), 'inventory.json');
      const inventory = SiteInventory.load('inventory.json');
      expect(inventory.path).toBe(absolutePath);
      inventory.upsertServer({ id: 'mine', host: 'h' });
      expect(inventory.save(absolutePath)).toBe(absolutePath);
      inventory.upsertServer({ id: 'more', host: 'h' });
      expect(inventory.save('./inventory.json')).toBe(absolutePath);
    } finally {
      process.chdir(cwd);
    }
    expect(serverIds()).toEqual(['eu-1', 'mine', 'more']);
  });

  it.skipIf(isWindows)('the tracked file reached through a symbolic link counts as another file (refused, safe)', async () => {
    seed(['eu-1']);
    const link = join(root, 'link');
    await symlink(root, link);
    const inventory = SiteInventory.load(join(link, 'inventory.json'));
    inventory.upsertServer({ id: 'mine', host: 'h' });
    const before = readFileSync(file, 'utf8');
    expect(thrown(() => inventory.save(file)).code).toBe('InventoryChanged');
    expect(readFileSync(file, 'utf8')).toBe(before);
    expect(inventory.save()).toBe(join(link, 'inventory.json'));
    expect(serverIds()).toEqual(['eu-1', 'mine']);
  });

  it('an inventory written by a newer apvm is read-only: edits and saves are InventoryReadOnly', async () => {
    const newer = JSON.stringify({ version: 2, servers: [{ id: 'eu-1', host: 'h' }] });
    await writeFile(file, newer);
    const inventory = SiteInventory.load(file);
    expect(inventory.readOnly).toBe(true);
    expect(inventory.servers()).toStrictEqual([{ id: 'eu-1', host: 'h' }]);
    expect(thrown(() => inventory.upsertServer({ id: 'x', host: 'h' })).code).toBe('InventoryReadOnly');
    expect(thrown(() => inventory.removeServer('eu-1')).code).toBe('InventoryReadOnly');
    expect(thrown(() => inventory.save()).code).toBe('InventoryReadOnly');
    const forced = thrown(() => inventory.save(undefined, { force: true }));
    expect(forced.code).toBe('InventoryReadOnly');
    // The object itself is the read-only one: no advice about the file.
    expect(forced.message).not.toContain('move that file aside');
    expect(thrown(() => inventory.save(join(root, 'elsewhere.json'))).code).toBe('InventoryReadOnly');
    expect(readFileSync(file, 'utf8')).toBe(newer);
    expect(existsSync(join(root, 'elsewhere.json'))).toBe(false);
    // A guarded save from another object never replaces it either.
    expect(thrown(() => SiteInventory.empty().save(file)).code).toBe('InventoryChanged');
    expect(readFileSync(file, 'utf8')).toBe(newer);
  });
});

// =============================================================================
// update(): load → fn → save under the lock
// =============================================================================

describe('SiteInventory.update()', () => {
  it('applies fn and returns the object fn received, tracking the saved file', () => {
    seed(['eu-1']);
    let received: SiteInventory | undefined;
    const updated = SiteInventory.update((inventory) => {
      received = inventory;
      expect(inventory.path).toBe(file);
      expect(inventory.servers().map((s) => s.id)).toEqual(['eu-1']);
      inventory.upsertServer({ id: 'new', host: 'h' });
    }, file);
    expect(updated).toBe(received);
    expect(updated.path).toBe(file);
    expect(serverIds()).toEqual(['eu-1', 'new']);
    // Tracked with what update wrote: a guarded save chains.
    updated.upsertServer({ id: 'after', host: 'h' });
    expect(updated.save()).toBe(file);
    expect(serverIds()).toEqual(['eu-1', 'new', 'after']);
  });

  it('uses APVM_INVENTORY without a path', () => {
    process.env.APVM_INVENTORY = file;
    const updated = SiteInventory.update((inventory) => inventory.upsertServer({ id: 'a', host: 'h' }));
    expect(updated.path).toBe(file);
    expect(serverIds()).toEqual(['a']);
  });

  it('writes nothing when fn changes nothing', () => {
    const updated = SiteInventory.update(() => {}, file);
    expect(existsSync(file)).toBe(false);
    // Tracked as missing: the next save creates the file.
    updated.upsertServer({ id: 'a', host: 'h' });
    expect(updated.save()).toBe(file);
    expect(serverIds()).toEqual(['a']);
  });

  it('never conflicts with a save made before it', () => {
    seed(['eu-1']);
    const stale = SiteInventory.load(file);
    seed(['theirs']);
    SiteInventory.update((inventory) => inventory.upsertServer({ id: 'mine', host: 'h' }), file);
    expect(serverIds()).toEqual(['eu-1', 'theirs', 'mine']);
    expect(thrown(() => stale.save()).code).toBe('InventoryChanged');
  });

  it('a throwing fn writes nothing and its exception propagates unchanged', async () => {
    const values: unknown[] = [new TypeError('boom'), 'a string', { reason: 'object' }, 42, null, undefined];
    // A missing file stays missing.
    for (const value of values) {
      let caught: unknown = 'not thrown';
      try {
        SiteInventory.update((inventory) => {
          inventory.upsertServer({ id: 'a', host: 'h' });
          throw value;
        }, file);
      } catch (err) {
        caught = err;
      }
      expect(caught).toBe(value);
      expect(existsSync(file)).toBe(false);
    }
    // An existing file keeps its bytes.
    seed(['eu-1']);
    const before = await readFile(file, 'utf8');
    const error = new Error('stop');
    expect(() =>
      SiteInventory.update((inventory) => {
        inventory.removeServer('eu-1');
        throw error;
      }, file),
    ).toThrow(error);
    expect(await readFile(file, 'utf8')).toBe(before);
    // The lock was released: the next update goes through at once.
    const started = Date.now();
    SiteInventory.update((inventory) => inventory.upsertServer({ id: 'b', host: 'h' }), file);
    expect(Date.now() - started).toBeLessThan(5_000);
    expect(serverIds()).toEqual(['eu-1', 'b']);
  });

  it('a throwing fn leaves no pending exception behind (strict Node-API policy)', () => {
    const out = runNodeOk(
      `const { SiteInventory } = require(ADDON);
       const caught = [];
       for (const value of [new Error('boom'), 'text', undefined]) {
         try { SiteInventory.update(() => { throw value; }, FILE); } catch (err) { caught.push(err === value); }
       }
       SiteInventory.update((inventory) => inventory.upsertServer({ id: 'a', host: 'h' }), FILE);
       console.log(JSON.stringify({ caught, servers: SiteInventory.load(FILE).servers().length }));`,
    );
    expect(JSON.parse(out)).toStrictEqual({ caught: [true, true, true], servers: 1 });
  });

  it('a fn that returns a promise is refused, and nothing is written', async () => {
    const err = thrown(() =>
      SiteInventory.update(async (inventory) => {
        inventory.upsertServer({ id: 'a', host: 'h' });
      }, file),
    );
    expect(err.code).toBe('InvalidArg');
    expect(err.message).toContain('`fn` returned a promise (or another thenable); it must be synchronous');
    expect(err.message).toContain('update() wrote nothing');
    expect(existsSync(file)).toBe(false);
  });

  it('an async fn that throws is refused and never becomes an unhandled rejection', () => {
    // Node's default policy ends the process on an unhandled rejection: the
    // child would exit 1 with the error on stderr.
    const out = runNodeOk(
      `const { existsSync } = require('node:fs');
       const { SiteInventory } = require(ADDON);
       const codes = [];
       const fns = [
         async () => { throw new Error('at once'); },
         async () => { await null; throw new Error('after an await'); },
         () => Promise.reject(new Error('a rejected promise')),
       ];
       for (const fn of fns) {
         try { SiteInventory.update(fn, FILE); } catch (err) { codes.push(err.code); }
       }
       setTimeout(() => console.log(JSON.stringify({ codes, written: existsSync(FILE) })), 50);`,
    );
    expect(JSON.parse(out)).toStrictEqual({ codes: ['InvalidArg', 'InvalidArg', 'InvalidArg'], written: false });
  });

  it('a promise subclass overriding then or catch cannot make the rejection unhandled', () => {
    // The handler is attached with the Promise.prototype.then captured when
    // the addon loaded, never with the promise's own then/catch.
    const out = runNodeOk(
      `const { existsSync } = require('node:fs');
       const { SiteInventory } = require(ADDON);
       class CatchThrows extends Promise { catch() { throw new Error('catch overridden'); } }
       class ThenThrows extends Promise { then() { throw new Error('then overridden'); } }
       // Replaced after the addon loaded: not used either.
       const original = Promise.prototype.then;
       Promise.prototype.then = function () { throw new Error('then replaced'); };
       const codes = [];
       const fns = [
         () => CatchThrows.reject(new Error('rejected')),
         () => Promise.reject.call(ThenThrows, new Error('rejected')),
         () => Promise.reject(new Error('rejected')),
       ];
       for (const fn of fns) {
         try { SiteInventory.update(fn, FILE); } catch (err) { codes.push(err.code); }
       }
       Promise.prototype.then = original;
       setTimeout(() => console.log(JSON.stringify({ codes, written: existsSync(FILE) })), 50);`,
    );
    expect(JSON.parse(out)).toStrictEqual({ codes: ['InvalidArg', 'InvalidArg', 'InvalidArg'], written: false });
  });

  it('any thenable is refused, not only a native promise', () => {
    const callable = Object.assign(() => {}, { then: () => {} });
    for (const value of [{ then() {} }, callable, Object.create({ then() {} })]) {
      const err = thrown(() =>
        SiteInventory.update((inventory) => {
          inventory.upsertServer({ id: 'a', host: 'h' });
          return value as never;
        }, file),
      );
      expect(err.code).toBe('InvalidArg');
      expect(existsSync(file)).toBe(false);
    }
    // A `then` that is not a function makes no thenable: the update lands.
    SiteInventory.update((inventory) => {
      inventory.upsertServer({ id: 'a', host: 'h' });
      return { then: 1 } as never;
    }, file);
    expect(serverIds()).toEqual(['a']);
  });

  it('a `then` getter that throws propagates its exception; nothing is written', () => {
    const boom = new Error('then getter');
    const value = Object.defineProperty({}, 'then', {
      get() {
        throw boom;
      },
    });
    let caught: unknown;
    try {
      SiteInventory.update((inventory) => {
        inventory.upsertServer({ id: 'a', host: 'h' });
        return value as never;
      }, file);
    } catch (err) {
      caught = err;
    }
    expect(caught).toBe(boom);
    expect(existsSync(file)).toBe(false);
  });

  it('after a refused or throwing fn, the object it received keeps its edits and saves guarded', () => {
    seed(['eu-1']);
    let kept: SiteInventory | undefined;
    expect(() =>
      SiteInventory.update((inventory) => {
        kept = inventory;
        inventory.upsertServer({ id: 'mine', host: 'h' });
        throw new Error('stop');
      }, file),
    ).toThrow('stop');
    expect(serverIds()).toEqual(['eu-1']);
    expect(kept!.servers().map((s) => s.id)).toEqual(['eu-1', 'mine']);
    expect(kept!.path).toBe(file);
    // Unchanged since update() read it: the guarded save lands.
    expect(kept!.save()).toBe(file);
    expect(serverIds()).toEqual(['eu-1', 'mine']);

    // Changed since: the guarded save of a refused fn's object is refused.
    let refused: SiteInventory | undefined;
    thrown(() =>
      SiteInventory.update(async (inventory) => {
        refused = inventory;
        inventory.upsertServer({ id: 'late', host: 'h' });
      }, file),
    );
    seed(['theirs']);
    expect(thrown(() => refused!.save()).code).toBe('InventoryChanged');
    expect(serverIds()).toEqual(['eu-1', 'mine', 'theirs']);
  });

  it('a library error thrown inside fn propagates with its code', async () => {
    await writeFile(file, JSON.stringify({ version: 2 }));
    const err = thrown(() =>
      SiteInventory.update((inventory) => inventory.upsertServer({ id: 'a', host: 'h' }), file),
    );
    expect(err.code).toBe('InventoryReadOnly');
    // Reading a newer inventory inside update is fine.
    expect(SiteInventory.update(() => {}, file).readOnly).toBe(true);
  });

  it('updates from parallel workers and processes all land', async () => {
    const PER_WRITER = 8;
    const WORKERS = 4;
    const PROCESSES = 2;
    const source = (writer: string) => `
      for (let i = 0; i < ${PER_WRITER}; i++) {
        SiteInventory.update((inventory) => inventory.upsertServer({ id: \`${writer}-\${i}\`, host: 'h' }), FILE);
      }`;
    const workers = Array.from(
      { length: WORKERS },
      (_, n) =>
        new Worker(
          `const { workerData } = require('node:worker_threads');
           const { SiteInventory } = require(workerData.addon);
           const FILE = workerData.file;
           ${source(`w${n}`)}`,
          { eval: true, workerData: { addon: ADDON, file } },
        ),
    );
    const finished = workers.map(
      (worker) =>
        new Promise<void>((resolve, reject) => {
          worker.once('error', reject);
          worker.once('exit', (code) => (code === 0 ? resolve() : reject(new Error(`worker exited ${code}`))));
        }),
    );
    const run = promisify(execFile);
    const processes = Array.from({ length: PROCESSES }, (_, n) =>
      run(process.execPath, ['-e', `const { SiteInventory } = require(${JSON.stringify(ADDON)}); const FILE = ${JSON.stringify(file)};${source(`p${n}`)}`], {
        timeout: CHILD_TIMEOUT_MS,
      }),
    );
    try {
      await within(Promise.all([...finished, ...processes]), CHILD_TIMEOUT_MS, 'the writers');
    } finally {
      // A failed or hung writer must not outlive the test.
      await Promise.allSettled(workers.map((worker) => worker.terminate()));
    }
    const ids = serverIds();
    expect(ids).toHaveLength((WORKERS + PROCESSES) * PER_WRITER);
    for (const writer of [...Array.from({ length: WORKERS }, (_, n) => `w${n}`), ...Array.from({ length: PROCESSES }, (_, n) => `p${n}`)]) {
      // Each writer's own updates landed in its order.
      expect(ids.filter((id) => id.startsWith(`${writer}-`))).toEqual(
        Array.from({ length: PER_WRITER }, (_, i) => `${writer}-${i}`),
      );
    }
  });

  it(
    'saving the same file inside fn fails with InventoryLocked after 10 s, never deadlocks',
    () => {
      seed(['eu-1']);
      let inner: (Error & { code?: string }) | undefined;
      let waited = 0;
      const updated = SiteInventory.update((inventory) => {
        inventory.upsertServer({ id: 'outer', host: 'h' });
        const started = Date.now();
        inner = thrown(() => inventory.save());
        waited = Date.now() - started;
      }, file);
      expect(inner?.code).toBe('InventoryLocked');
      expect(inner?.message).toContain('This call ran inside an update() callback of the same file');
      expect(waited).toBeGreaterThanOrEqual(9_900);
      // The outer update still saved what fn left.
      expect(updated.servers().map((s) => s.id)).toEqual(['eu-1', 'outer']);
      expect(serverIds()).toEqual(['eu-1', 'outer']);
    },
    LOCK_TEST_TIMEOUT_MS,
  );

  it(
    'updating the same file inside fn fails with InventoryLocked; uncaught, it aborts the outer update',
    () => {
      seed(['eu-1']);
      const before = readFileSync(file, 'utf8');
      const err = thrown(() =>
        SiteInventory.update((inventory) => {
          inventory.upsertServer({ id: 'outer', host: 'h' });
          SiteInventory.update((nested) => nested.upsertServer({ id: 'inner', host: 'h' }), file);
        }, file),
      );
      expect(err.code).toBe('InventoryLocked');
      expect(readFileSync(file, 'utf8')).toBe(before);
    },
    LOCK_TEST_TIMEOUT_MS,
  );

  it(
    'a lock held by another process is InventoryLocked without the update() hint, even inside fn of another file',
    async () => {
      seed(['eu-1']);
      // A callback of this very file that threw must not leave it marked
      // as "inside update()": the hint below would then be wrong.
      expect(() =>
        SiteInventory.update(() => {
          throw new Error('stop');
        }, file),
      ).toThrow('stop');
      const release = await holdLock(file);
      try {
        const inventory = SiteInventory.load(file);
        let err: (Error & { code?: string }) | undefined;
        SiteInventory.update(() => {
          err = thrown(() => inventory.save());
        }, join(root, 'other.json'));
        expect(err?.code).toBe('InventoryLocked');
        expect(err?.message).toContain('another process held it for 10 s');
        expect(err?.message).not.toContain('update() callback');
      } finally {
        expect(await release()).toBe(0);
      }
    },
    LOCK_TEST_TIMEOUT_MS,
  );

  it(
    'saving the outer file from inside a nested update() of another file waits for the outer lock, with the hint',
    () => {
      seed(['eu-1']);
      const other = join(root, 'other.json');
      let inner: (Error & { code?: string }) | undefined;
      let waited = 0;
      SiteInventory.update((outer) => {
        outer.upsertServer({ id: 'outer', host: 'h' });
        SiteInventory.update((nested) => {
          nested.upsertServer({ id: 'nested', host: 'h' });
          const started = Date.now();
          inner = thrown(() => outer.save());
          waited = Date.now() - started;
        }, other);
      }, file);
      expect(inner?.code).toBe('InventoryLocked');
      expect(inner?.message).toContain('This call ran inside an update() callback of the same file');
      expect(waited).toBeGreaterThanOrEqual(9_900);
      // Both updates still saved what their callbacks left.
      expect(serverIds()).toEqual(['eu-1', 'outer']);
      expect(serverIds(other)).toEqual(['nested']);
    },
    LOCK_TEST_TIMEOUT_MS,
  );

  it('another file can be updated inside fn', () => {
    const other = join(root, 'other.json');
    SiteInventory.update((inventory) => {
      inventory.upsertServer({ id: 'outer', host: 'h' });
      SiteInventory.update((nested) => nested.upsertServer({ id: 'inner', host: 'h' }), other);
    }, file);
    expect(serverIds()).toEqual(['outer']);
    expect(serverIds(other)).toEqual(['inner']);
  });
});
