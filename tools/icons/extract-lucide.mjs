// Turns Lucide icon data (ISC licence) from an installed lucide-react package into SVG files
// that Slint can load and recolour with `colorize`.
//
//   node tools/icons/extract-lucide.mjs <lucide-react dir> <out dir> [names.txt]
//
// names.txt holds one icon name per line (Lucide's kebab-case names); `#` starts a comment.
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const [, , pkgDir, outDir, listFile = path.join(path.dirname(new URL(import.meta.url).pathname.replace(/^\/(\w:)/, '$1')), 'icons.txt')] = process.argv;
if (!pkgDir || !outDir) {
  console.error('usage: node extract-lucide.mjs <lucide-react dir> <out dir> [names.txt]');
  process.exit(64);
}
const names = fs.readFileSync(listFile, 'utf8').split(/\r?\n/).map((l) => l.replace(/#.*/, '').trim()).filter(Boolean);
fs.mkdirSync(outDir, { recursive: true });
const esc = (v) => String(v).replace(/&/g, '&amp;').replace(/"/g, '&quot;');
let missing = [];
for (const name of names) {
  const file = path.join(pkgDir, 'dist', 'esm', 'icons', `${name}.mjs`);
  if (!fs.existsSync(file)) { missing.push(name); continue; }
  // Alias modules (renamed icons) only re-export another icon's module; follow them.
  let target = file;
  for (let hops = 0; hops < 4; hops++) {
    const src = fs.readFileSync(target, 'utf8');
    if (src.includes('__iconData')) break;
    const m = src.match(/from '\.\/([\w-]+\.mjs)'/);
    if (!m) break;
    target = path.join(path.dirname(target), m[1]);
  }
  const mod = await import(pathToFileURL(target).href);
  if (!mod.__iconData) { missing.push(name); continue; }
  const nodes = mod.__iconData.node;
  const body = nodes.map(([tag, attrs]) => `<${tag} ${Object.entries(attrs).filter(([k]) => k !== 'key').map(([k, v]) => `${k}="${esc(v)}"`).join(' ')}/>`).join('');
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">${body}</svg>\n`;
  fs.writeFileSync(path.join(outDir, `${name}.svg`), svg);
}
console.log(`wrote ${names.length - missing.length} icons to ${outDir}`);
if (missing.length) { console.error(`missing: ${missing.join(', ')}`); process.exitCode = 1; }
