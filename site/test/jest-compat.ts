// A small jest-shaped layer over node:test and node:assert, so the tests copied from mopm.dev's site/ (the Markdown
// renderer's: unit, attack corpus, linear time, fuzz) run on node:test with their bodies unchanged. Only what those
// tests use: describe, test, test.each, and the matchers below (with .not). No mocks, no timers, no snapshots.

import assert from 'node:assert/strict';
import { beforeEach as nodeBeforeEach, describe as nodeDescribe, test as nodeTest } from 'node:test';
import { format } from 'node:util';

export const describe = nodeDescribe;
export const beforeEach = nodeBeforeEach;

type Table = readonly (readonly unknown[] | unknown)[];
type Fn = (...args: any[]) => unknown;

function each(table: Table): (name: string, fn: Fn) => void {
  return (name, fn) => {
    const rows = table.every((r) => Array.isArray(r));
    for (const row of table) {
      const args = rows ? (row as unknown[]) : [row];
      // jest's %s, %j, %o placeholders, filled from the row.
      nodeTest(format(name, ...args.slice(0, (name.match(/%[sjodi]/g) ?? []).length)), () => {
        fn(...args);
      });
    }
  };
}

export const test = Object.assign(((name: string, fn: () => unknown) =>
  nodeTest(name, () => {
    fn();
  })) as (name: string, fn: () => unknown) => void, { each: each as (table: Table) => (name: string, fn: Fn) => void });

function matchers(actual: unknown, negate: boolean) {
  const check = (ok: boolean, message: string): void => {
    if (ok === negate) assert.fail(`${negate ? 'not ' : ''}${message}`);
  };
  const includes = (container: unknown, item: unknown): boolean =>
    typeof container === 'string' ? container.includes(String(item)) : container instanceof Set ? container.has(item) : (container as unknown[]).includes(item);
  return {
    toBe: (want: unknown) => check(Object.is(actual, want), `expected ${format('%j', actual)} to be ${format('%j', want)}`),
    toEqual: (want: unknown) => {
      let ok = true;
      try {
        assert.deepEqual(actual, want);
      } catch {
        ok = false;
      }
      check(ok, `expected ${format('%j', actual)} to equal ${format('%j', want)}`);
    },
    toContain: (item: unknown) => check(includes(actual, item), `expected ${format('%j', actual instanceof Set ? [...actual] : actual)} to contain ${format('%j', item)}`),
    toMatch: (re: RegExp) => check(re.test(String(actual)), `expected ${format('%j', actual)} to match ${re}`),
    toBeNull: () => check(actual === null, `expected ${format('%j', actual)} to be null`),
    toHaveLength: (n: number) => check((actual as { length: number }).length === n, `expected length ${(actual as { length: number }).length} to be ${n}`),
    toBeLessThanOrEqual: (n: number) => check((actual as number) <= n, `expected ${actual} <= ${n}`),
    toBeGreaterThanOrEqual: (n: number) => check((actual as number) >= n, `expected ${actual} >= ${n}`),
    toThrow: (re?: RegExp) => {
      let threw: unknown = null;
      try {
        (actual as () => unknown)();
      } catch (e) {
        threw = e;
      }
      check(threw !== null && (re === undefined || re.test(String((threw as Error).message))), `expected the call to throw${re ? ` ${re}` : ''}, got ${format('%s', threw)}`);
    },
  };
}

export function expect(actual: unknown) {
  return { ...matchers(actual, false), not: matchers(actual, true) };
}
