// Refreshes crates/capture/src/win/anticheat-games.tsv, the games known to
// use anti-cheat, from AreWeAntiCheatYet's list (MIT licence,
// https://github.com/AreWeAntiCheatYet/AreWeAntiCheatYet). The game capture
// hook is never used on these unless you ask for it.
//
//   node scripts/update-anticheat-list.mjs
//
// One game a line: name, anti-cheats (comma-separated), Steam app id, Epic
// namespace, tab-separated (empty when unknown).
import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const url = 'https://raw.githubusercontent.com/AreWeAntiCheatYet/AreWeAntiCheatYet/master/games.json';
const games = await (await fetch(url)).json();
const clean = (s) => String(s ?? '').replace(/[\t\r\n]+/g, ' ').trim();
const lines = games
    .filter((g) => g.anticheats?.length)
    .map((g) => [clean(g.name), g.anticheats.map(clean).join(','), clean(g.storeIds?.steam), clean(g.storeIds?.epic?.namespace)].join('\t'))
    .sort((a, b) => a.localeCompare(b));
const out = fileURLToPath(new URL('../crates/capture/src/win/anticheat-games.tsv', import.meta.url));
const header = '# From AreWeAntiCheatYet (MIT): https://github.com/AreWeAntiCheatYet/AreWeAntiCheatYet\n# name\tanti-cheats\tsteam app id\tepic namespace\n';
writeFileSync(out, header + lines.join('\n') + '\n');
console.log(`${lines.length} games`);
