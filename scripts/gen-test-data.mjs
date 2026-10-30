import { createWriteStream, mkdirSync } from 'node:fs';
import { dirname } from 'node:path';

const N = Number(process.argv[2] ?? 300000);
const OUT = process.argv[3] ?? 'target/data/big.json';

const nombres = ['Antonio','María','Pedro','Lucía','José','Sofía','Iñaki','Ángel','Marta','Nuria','Carmen','Jesús','Raúl','Elena','Óscar','Pilar','Andrés','Beatriz','Consuelo','África'];
const apellidos = ['García','Martínez','López','Sánchez','Pérez','Gómez','Fernández','Ruiz','Díaz','Moreno','Muñoz','Álvarez','Romero','Alonso','Gutiérrez'];
const estados = ['activo','inactivo','pendiente','baja'];
const ciudades = ['Madrid','Toledo','Cuenca','Albacete','Guadalajara'];

let seed = 1234567;
const rnd = () => (seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648;
const pick = (a) => a[Math.floor(rnd() * a.length)];

mkdirSync(dirname(OUT), { recursive: true });
const out = createWriteStream(OUT);
const write = (s) => { if (!out.write(s)) return new Promise((r) => out.once('drain', r)); };

await write('{');
let buf = '';
for (let i = 0; i < N; i++) {
  const obj = {
    nombre: pick(nombres),
    apellidos: `${pick(apellidos)} ${pick(apellidos)}`,
    edad: 16 + Math.floor(rnd() * 60),
    estado: pick(estados),
    activo: rnd() > 0.3,
    descripcion: rnd() > 0.85 ? 'Comillas "raras", barra \\ y salto\nen medio' : null,
    direccion: { calle: `Calle ${Math.floor(rnd() * 200)}`, ciudad: pick(ciudades) },
    cifras: [2020 + Math.floor(rnd() * 6), 2021 + Math.floor(rnd() * 6)],
    datos: [Math.round(rnd() * 100) / 10, Math.round(rnd() * 100) / 10, Math.round(rnd() * 100) / 10],
  };
  buf += `${i > 0 ? ',' : ''}\n"id${i}":${JSON.stringify(obj)}`;
  if (buf.length > 1 << 20) {
    await write(buf);
    buf = '';
  }
}
await write(buf);
await write('\n}\n');
out.end();
await new Promise((r) => out.on('close', r));
console.log(`generados ${N} objetos en ${OUT}`);
