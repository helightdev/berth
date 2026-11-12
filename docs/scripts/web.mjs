// Puts the page of `berth serve` into public/web, so the site carries it at
// /web as plain files. The page is built with berth (cargo build -p berth
// --features web leaves it in target/web-dist); without it the site still builds, and /web
// says how to get it.
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const dist = join(root, 'target', 'web-dist');
const out = join(root, 'docs', 'public', 'web');
const files = ['berth-web.js', 'berth-web_bg.wasm'];
const built = () => files.every((file) => existsSync(join(dist, file)));

if (!built() && process.env.BERTH_WEB_BUILD !== '0') {
  try {
    // The page of a release, though berth itself is a development build.
    execFileSync('cargo', ['build', '--locked', '-p', 'berth', '--features', 'web'], {
      cwd: root,
      stdio: 'inherit',
      env: { ...process.env, BERTH_WEB_PROFILE: 'web' },
    });
  } catch {
    // No Rust here, or no wasm target: the site is built without the page.
  }
}

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
if (built()) {
  for (const file of files) cpSync(join(dist, file), join(out, file));
  const build = createHash('sha256').update(readFileSync(join(dist, files[1]))).digest('hex').slice(0, 16);
  const page = readFileSync(join(root, 'web', 'index.html'), 'utf8');
  // No server of its own: the page asks for one, or takes ?server= from its address.
  writeFileSync(join(out, 'index.html'), page.replaceAll('@BUILD@', build).replaceAll('@SERVER@', ''));
} else {
  console.warn('docs: the page for /web is not built (cargo build -p berth --features web); a note is put in its place');
  writeFileSync(
    join(out, 'index.html'),
    '<!doctype html><meta charset="utf-8"><title>berth</title><body style="background:#101216;color:#cdd6f4;font-family:ui-monospace,monospace;padding:2rem">This copy of the site was built without the page. <code>berth serve --listen ADDRESS --web</code> shows it.</body>\n',
  );
}
