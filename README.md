# json-explorer

Explora y busca ficheros JSON gigantes **sin cargarlos en memoria**.

`jexplorer` recorre el fichero una sola vez, guarda las posiciones de cada objeto en un índice
SQLite y a partir de ahí responde búsquedas y consultas en milisegundos, incluso con cientos de
miles de objetos. El JSON original no se modifica ni se copia: cada resultado apunta a su
`offset` y longitud exactos dentro del fichero.

```
JSON de 250 MB  ──►  índice .jindex (SQLite)  ──►  jexplorer search / shell
     (intacto)              (~decenas de MB)
```

## Características

- **Escaneo en streaming**: ventanas de 8 MB, memoria constante, sin `serde_json` para el recorrido.
- **Índice reanudable**: si la indexación se interrumpe, continúa donde se quedó.
- **Búsqueda avanzada**: igualdad, prefijo, contiene, rangos numéricos, nulos, existencia,
  ausencia y expresiones regulares.
- **Sin mayúsculas ni acentos** con `--fold` (Unicode NFD).
- **Varios filtros** combinables con `AND` u `OR`, y `--count` para contar sin traer resultados.
- **FTS5 opcional** (`--fts`) para búsquedas de texto rápido con tokenizador *trigram*.
- **Salidas** en tabla, JSON, CSV y TSV, listas para `jq`, Excel o scripts.
- **Shell interactivo** con historial, paginación, vista formateada y exportación.
- **Verificación de integridad**: comprueba que el índice sigue coincidiendo con el fichero.
- **Solo lectura**: las consultas abren el índice en modo `read-only` (con *fallback* seguro).

## Instalación

Requisitos:

- Rust **1.87 o superior** (`rustup` recomendado).
- Un *linker* de C: Visual Studio Build Tools en Windows, o las toolchains GNU (`stable-x86_64-pc-windows-gnu`
  con MinGW-w64). En Linux/macOS, `build-essential` / Xcode Command Line Tools.

```bash
git clone <repo> json-explorer
cd json-explorer
cargo build --release
# binario en target/release/jexplorer (jexplorer.exe en Windows)
```

SQLite se compila embebido (`rusqlite/bundled`): no hace falta instalar nada aparte.

## Inicio rápido

El JSON de entrada debe ser un **objeto cuyas claves sean los objetos** a explorar:

```json
{
  "id0": { "nombre": "Antonio", "edad": 42, "ciudad": "Toledo" },
  "id1": { "nombre": "Lucía",   "edad": 31, "ciudad": "Madrid" }
}
```

```bash
# 1. ¿Qué propiedades hay?
jexplorer props datos.json

# 2. Indexar todas las propiedades (+ FTS para texto)
jexplorer index datos.json --all --fts

# 3. Buscar
jexplorer search --db datos.json.jindex --path ciudad --value Madrid
jexplorer search --db datos.json.jindex --path edad --op gte --value 30
```

El índice se crea por defecto junto al fichero: `datos.json.jindex`.

## Comandos

| Comando | Qué hace |
| --- | --- |
| `props <file> [--sample N]` | Muestra propiedades, apariciones y tipos de los primeros objetos. |
| `index <file>` | Construye o actualiza el índice. |
| `search` | Busca por una o varias propiedades. |
| `get --id N \| --key CLAVE` | Muestra un objeto por id interno o por su clave raíz. |
| `export --id N --out f.json` | Extrae un objeto tal cual a un fichero. |
| `groupby --path PROP [--limit N]` | Agrupa valores y muestra los más frecuentes. |
| `verify [--sample N]` | Comprueba índice contra fichero (0 = comprobar todo). |
| `stats` | Estado, tamaño y propiedades del índice. |
| `shell` | Sesión interactiva. |

### Indexar

```bash
jexplorer index datos.json --select nombre --select ciudad
jexplorer index datos.json --all --fts --batch 50000
jexplorer index datos.json --force          # rehacer desde cero
```

- `--select PROP` (repetible) indexa solo esas propiedades; `--all` indexa todas las de primer nivel.
- `--fts` añade la tabla FTS5 *trigram* (útil con `--fold` para `contains`).
- `--batch` ajusta objetos por transacción (más alto = más rápido, más memoria).
- Los cambios de formato del índice se resuelven con `--force`; se avisa al abrirlo.

### Buscar

```bash
jexplorer search --db datos.json.jindex --path nombre --value Antonio
jexplorer search --db datos.json.jindex --path nombre --op prefix --value Ant --fold
jexplorer search --db datos.json.jindex --path descripcion --op contains --value "texto libre" --fold
jexplorer search --db datos.json.jindex --path edad --op gt --value 18 --format json
jexplorer search --db datos.json.jindex --path dato --op null
jexplorer search --db datos.json.jindex --path telefono --op missing
```

Operadores (`--op`):

| Operador | Alias | Significado |
| --- | --- | --- |
| `eq` | `=`, `==`, `igual` | Igualdad exacta |
| `ne` | `!=`, `<>`, `distinto` | Distinto de |
| `contains` | `~`, `contiene` | Contiene la subcadena |
| `prefix` | `^`, `empieza` | Empieza por |
| `gt` / `gte` / `lt` / `lte` | `>`, `>=`, `<`, `<=` | Rangos numéricos |
| `exists` | `?`, `existe` | La propiedad está presente |
| `missing` | `!?`, `falta` | La propiedad no está |
| `null` | `nulo` | La propiedad existe y vale `null` |
| `regex` | `~r` | Expresión regular (Rust `regex`) |

Opciones útiles:

- `--fold`: ignora mayúsculas y acentos en `eq`, `ne`, `contains` y `prefix`.
- `--filter "PROP OP VALOR"` (repetible): filtros adicionales; se combinan con `AND`.
- `--any`: combina los filtros con `OR`.
- `--count`: devuelve solo el número de coincidencias.
- `--limit` / `--after`: paginación estable por id interno (sirve como cursor).
- `--format table|json|csv|tsv`.

```bash
# AND: activos mayores de 30
jexplorer search --db datos.json.jindex \
  --path edad --op gt --value 30 \
  --filter "estado = activo"

# OR: de Madrid o de Toledo
jexplorer search --db datos.json.jindex \
  --path ciudad --value Madrid \
  --filter "ciudad = Toledo" --any --count
```

### Agrupar y verificar

```bash
jexplorer groupby --db datos.json.jindex --path ciudad --limit 10 --format csv
jexplorer verify --db datos.json.jindex --sample 5000
jexplorer get --db datos.json.jindex --key id42
jexplorer export --db datos.json.jindex --key id42 --out objeto.json
```

`verify` recorre el fichero y el índice en paralelo: compara claves y *spans*, parsea una muestra
de objetos y contrasta sus propiedades. También detecta objetos mayores de 8 MB, que se indexan
sin propiedades.

### Shell interactivo

```bash
jexplorer shell --db datos.json.jindex
```

```
jx> nombre = Antonio
jx> nombre ~i ant            # contiene, sin mayúsculas ni acentos
jx> edad >= 30               # rangos
jx> n                        # siguiente página
jx> ver 3                    # abrir el resultado 3 formateado
jx> raw 3                    # verlo tal cual está en el fichero
jx> e 3 salida.json          # exportar
jx> key id42                 # abrir por clave raíz
jx> top ciudad 5             # agrupar valores
jx> props | stats | ayuda | q
```

Los sufijos de operador del shell: `=i`, `~i`, `^i` (con *fold*), `>`, `>=`, `<`, `<=`, `?`,
`!?`, `null` y `~r` (regex).

## Cómo funciona

1. **Escaneo**: el *scanner* lee el JSON en ventanas de 8 MB y localiza los miembros del objeto
   raíz (`clave: {...}`) sin materializar el documento completo.
2. **Índice**: para cada objeto guarda `id`, clave raíz, `byte_offset` y `byte_length`. Para las
   propiedades seleccionadas guarda tipo, valor textual, valor numérico y valor normalizado
   (plegado Unicode) en `props`.
3. **Consultas**: un único filtro usa índices SQLite dedicados (`lookup`, `norm`, `num` o FTS);
   varios filtros se resuelven con predicados `EXISTS` combinados con `AND`/`OR`.
4. **Lectura**: `get`/`export` leen únicamente los bytes del objeto desde el JSON original.

Si el JSON cambia de tamaño o *fingerprint*, el índice se considera inválido; si solo cambia la
fecha de modificación, se avisa pero se permite consultar.

## Limitaciones

- Solo se indexan **objetos de primer nivel** del objeto raíz; los anidados se guardan como texto
  JSON (hasta 4 KB) y se pueden buscar con `contains`/`regex`, no con rutas anidadas.
- Objetos mayores de **8 MB** se indexan por clave y posición, pero sin propiedades.
- `regex` se evalúa sobre los valores almacenados, no sobre posiciones arbitrarias del fichero.
- El índice es propio de la versión actual del formato (`FORMAT_VERSION`); tras actualizar la
  herramienta puede ser necesario `--force`.

## Desarrollo

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Estructura:

- `crates/core` (`jxcore`): scanner, parser, índice SQLite y motor de búsqueda.
- `crates/cli` (`jexplorer`): comandos, salidas y shell.
- `scripts/gen-test-data.mjs`: genera un JSON de prueba (por defecto 300 000 objetos).
- `scripts/verify-offsets.mjs`: verifica *offsets* y propiedades del índice contra el JSON.

```bash
node scripts/gen-test-data.mjs 300000 target/data/big.json
target/release/jexplorer index target/data/big.json --all --fts
node scripts/verify-offsets.mjs target/data/big.json target/data/big.json.jindex
```

## Hoja de ruta

- Rutas anidadas (`direccion.ciudad`).
- Índices incrementales cuando el JSON cambia.
- Exportación de conjuntos de resultados completos (JSON Lines / CSV).
- Estadísticas por propiedad (mín./máx./media) en `stats`.
- Empaquetado multiplataforma y binarios precompilados.

## Licencia

MIT. Consulta [LICENSE](LICENSE).
