import { noDomain } from '../lib/config';

const ctx = (v: unknown) => (k: string) => (k === 'noDomain' ? v : undefined);

describe('noDomain context', () => {
  test('the string "true" (what -c noDomain=true passes) and boolean true are on', () => {
    expect(noDomain(ctx('true'))).toBe(true);
    expect(noDomain(ctx(true))).toBe(true);
  });
  test('absent, false and "false" are off', () => {
    expect(noDomain(ctx(undefined))).toBe(false);
    expect(noDomain(ctx('false'))).toBe(false);
    expect(noDomain(ctx(false))).toBe(false);
  });
  test('any other value is refused rather than silently choosing domain mode', () => {
    expect(() => noDomain(ctx('yes'))).toThrow(/noDomain/);
    expect(() => noDomain(ctx(1))).toThrow(/noDomain/);
  });
});
