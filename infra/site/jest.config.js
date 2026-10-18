// Cap workers: unbounded workers exhaust memory on many-core machines (ts-jest workers reach ~1.6 GB each).
module.exports = {
  maxWorkers: 2,
  workerIdleMemoryLimit: '1GB',
  testEnvironment: 'node',
  roots: ['<rootDir>/test'],
  testMatch: ['**/*.test.ts'],
  transform: { '^.+\\.tsx?$': ['ts-jest', { tsconfig: 'tsconfig.json' }] },
};
