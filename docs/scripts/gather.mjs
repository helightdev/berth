// The manual is the README: this splits it into one page per section, so the
// site and the repository say the same thing. Runs before every build.
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const docs = join(dirname(fileURLToPath(import.meta.url)), '..');
const out = join(docs, 'content/docs');
const blob = 'https://github.com/helightdev/berth/blob/main/';

const slug = (title) => title.toLowerCase().replace(/\(.*?\)/g, '').replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
const first = (body) => (body.trim().split(/\n\s*\n/)[0] || '').replace(/\s+/g, ' ').replace(/[`*_]|\[(.*?)\]\(.*?\)/g, '$1');
const page = (title, body) => {
  const text = body
    // The guides are pages here; other files are on GitHub.
    .replace(/\]\(docs\/content\/docs\/([a-z-]+)\.md(#[^)]*)?\)/g, '](/docs/$1$2)')
    .replace(/\]\((?!https?:|#|\/|mailto:)([^)]+)\)/g, `](${blob}$1)`)
    // Sections were one level down in the README.
    .replace(/^##(#+) /gm, '#$1 ');
  const description = first(body).slice(0, 180).replace(/"/g, "'");
  return `---\ntitle: "${title}"\ndescription: "${description}"\n---\n\n${text.trim()}\n`;
};

const readme = readFileSync(join(docs, '../README.md'), 'utf8');
const [intro, ...sections] = readme.split(/^## /m);
rmSync(join(out, 'manual'), { recursive: true, force: true });
mkdirSync(join(out, 'manual'), { recursive: true });

writeFileSync(join(out, 'index.md'), page('berth', intro.replace(/^<img[^\n]*\n/m, '').replace(/^# .*\n/m, '')));
const pages = [];
for (const section of sections) {
  const title = section.slice(0, section.indexOf('\n')).trim();
  if (title === 'License') continue;
  pages.push(slug(title));
  writeFileSync(join(out, 'manual', `${slug(title)}.md`), page(title, section.slice(section.indexOf('\n'))));
}
writeFileSync(join(out, 'manual/meta.json'), JSON.stringify({ title: 'Manual', pages }, null, 2) + '\n');
console.log(`gathered ${pages.length + 1} pages from README.md`);
