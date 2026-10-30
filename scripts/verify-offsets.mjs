import { DatabaseSync } from 'node:sqlite';
import { readFileSync } from 'node:fs';

const jsonPath = process.argv[2];
const dbPath = process.argv[3];

const buf = readFileSync(jsonPath);
const parsed = JSON.parse(buf.toString('utf8'));
const db = new DatabaseSync(dbPath, { readOnly: true });

const total = Number(db.prepare('SELECT count(*) AS c FROM objects').get().c);
const keys = Object.keys(parsed);
console.log(`objetos en el índice: ${total}   claves en el JSON: ${keys.length}`);
if (total !== keys.length) console.log('AVISO: el recuento no coincide');

const rows = db.prepare('SELECT id, root_key, byte_offset, byte_length FROM objects').all();
const step = Math.max(1, Math.floor(rows.length / 5000));
const sample = rows.filter((r) => Number(r.id) % step === 0);

let bad = 0;
let badKey = 0;
const examples = [];
for (const r of sample) {
  const off = Number(r.byte_offset);
  const len = Number(r.byte_length);
  const slice = buf.subarray(off, off + len);
  let obj;
  try {
    obj = JSON.parse(slice.toString('utf8'));
  } catch {
    bad++;
    if (examples.length < 3) examples.push(`id ${r.id}: no parsea en ${off}:${len}`);
    continue;
  }
  const src = parsed[r.root_key];
  if (JSON.stringify(obj) !== JSON.stringify(src)) {
    bad++;
    if (examples.length < 3) examples.push(`id ${r.id}: contenido distinto`);
  }
  const expectedKey = `id${r.id}`;
  if (r.root_key !== expectedKey) badKey++;
}
console.log(`offsets comprobados: ${sample.length}   inconsistentes: ${bad}   claves fuera de orden: ${badKey}`);
for (const e of examples) console.log('  ', e);

const pathId = new Map(db.prepare('SELECT id, path FROM paths').all().map((r) => [Number(r.id), r.path]));
let propBad = 0;
let propChecked = 0;
const propSample = sample.filter((_, i) => i % 100 === 0).slice(0, 50);
for (const r of propSample) {
  const src = parsed[r.root_key];
  const got = db.prepare('SELECT path_id, type, value FROM props WHERE object_id = ?').all(Number(r.id));
  for (const p of got) {
    propChecked++;
    const path = pathId.get(Number(p.path_id));
    const v = src[path];
    const expected =
      typeof v === 'string' ? v : v === null ? 'null' : typeof v === 'object' ? JSON.stringify(v) : String(v);
    if (p.value !== expected) {
      propBad++;
      if (propBad <= 3) console.log(`  prop id ${r.id} ${path}: "${p.value}" != "${expected}"`);
    }
  }
}
console.log(`propiedades comprobadas: ${propChecked}   incorrectas: ${propBad}`);

const paths = [...pathId.values()].join(', ');
console.log(`rutas indexadas: ${paths}`);
