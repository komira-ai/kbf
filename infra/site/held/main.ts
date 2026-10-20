import { execFileSync } from 'child_process';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { Refused, Tools, main } from './steps';

const root = path.resolve(__dirname, '..');
const tools: Tools = {
  capture: (cmd, args, cwd) => execFileSync(cmd, args, { cwd: cwd ?? root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }),
  stream: (cmd, args, cwd) => {
    execFileSync(cmd, args, { cwd: cwd ?? root, stdio: 'inherit' });
  },
  log: (line) => console.log(line),
  mkScratch: () => fs.mkdtempSync(path.join(os.tmpdir(), 'kbf-upload-')),
  removeScratch: (dir) => fs.rmSync(dir, { recursive: true, force: true }),
  readFile: (p) => fs.readFileSync(p, 'utf8'),
  writeFile: (p, data) => fs.writeFileSync(p, data),
  listFiles: (dir) => {
    const out: string[] = [];
    const walk = (d: string): void => {
      for (const f of fs.readdirSync(d).sort()) {
        const p = path.join(d, f);
        if (fs.statSync(p).isDirectory()) walk(p);
        else out.push(path.relative(dir, p).split(path.sep).join('/'));
      }
    };
    walk(dir);
    return out.sort();
  },
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
