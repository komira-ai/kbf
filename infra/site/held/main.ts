import { execFileSync } from 'child_process';
import * as path from 'path';
import { Refused, Tools, main } from './steps';

const root = path.resolve(__dirname, '..');
const tools: Tools = {
  capture: (cmd, args) => execFileSync(cmd, args, { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }),
  stream: (cmd, args) => {
    execFileSync(cmd, args, { cwd: root, stdio: 'inherit' });
  },
  log: (line) => console.log(line),
};

try {
  main(process.argv.slice(2), tools);
} catch (e) {
  if (e instanceof Refused) {
    console.error(`error: ${e.message}`);
    process.exit(2);
  }
  throw e;
}
