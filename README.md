# json-explorer

Explora JSON grandes sin cargar el documento completo en memoria. `jexplorer` es
**exclusivamente interactivo**: abre una fuente, descubre propiedades, crea un índice
SQLite `.jindex` y consulta los objetos leyendo sus posiciones en el JSON original.
En una terminal muestra menús navegables con flechas; con entrada o salida
redirigida conserva el shell textual para scripts.
El JSON no se modifica ni se copia al indexarlo.

## Interfaz con flechas

Ejecuta `jexplorer` o `jexplorer "C:\datos\datos.json"` en CMD:

- **↑ / ↓**: elige una acción o recorre todas las coincidencias de una búsqueda.
- **Enter / →**: abre una acción o el objeto seleccionado del JSON.
- **← / Esc**: vuelve atrás; desde el visor conserva la página y el resultado.
- **RePág / AvPág**: cambia de página de resultados o desplaza el visor.
- **Inicio / Fin**: extremos de la página actual; en el visor, inicio/final del objeto.
- En el visor, **r** alterna original/formateado y **e** exporta los bytes originales.
- En los campos de texto, ← / → editan; Enter acepta y Esc cancela.
- En el menú principal, Esc o `q` salen.

Puedes buscar eligiendo propiedad, operador y valor, o escribir una consulta
completa. El navegador muestra el total de coincidencias y carga **solo una página**
de objetos cada vez; las flechas cruzan los límites de página automáticamente.
El visor no tiene el límite de 200 líneas del shell textual y ajusta las líneas
al ancho de la terminal.

Los menús incluyen apertura, indexación, propiedades, estadísticas, valores
frecuentes, verificación y ajustes. **Comandos del shell** conserva las operaciones
avanzadas; indexar y verificar muestran su salida textual y esperan Enter para
volver al menú. Rehacer un índice requiere confirmación. **Última búsqueda** vuelve
a cargar los resultados desde la primera página.

Abrir un JSON sigue sin indexarlo automáticamente. Usa **Indexar JSON** si no
hay un índice válido. Los formatos `json`, `csv` y `tsv` afectan a los comandos
del shell, no al navegador con flechas.

## Instalación

Requiere Rust 1.87 o superior y un linker de C (Visual Studio Build Tools o MinGW
en Windows; toolchain de C en Linux/macOS). SQLite se compila embebido.
Desde el repositorio:

```bat
cargo build --release
target\release\jexplorer.exe
```

En Linux/macOS, el binario es `target/release/jexplorer`. Los ejemplos siguientes
suponen que `jexplorer` está en el `PATH`; también puedes usar su ruta completa.

## Inicio rápido (CMD de Windows)

El documento debe tener un objeto raíz cuyas claves identifiquen los objetos:

```json
{
  "a": { "nombre": "Antonio", "edad": 42, "ciudad": "Toledo", "dato": null },
  "b": { "nombre": "MARÍA", "edad": 30, "ciudad": "Madrid" }
}
```

En CMD, inicia el shell vacío o abre directamente un JSON o un índice:

```bat
jexplorer
jexplorer "C:\datos con espacios\datos.json"
jexplorer "C:\datos con espacios\datos.json.jindex"
jexplorer --help
jexplorer --version
```

Sin argumentos muestra el menú principal; con entrada o salida redirigida,
un banner y el shell textual. `--help` y `--version` funcionan sin fichero. **No existen subcomandos externos**:
`jexplorer index ...`, `jexplorer search ...`, `jexplorer shell ...` y el argumento
externo `--db` no son la interfaz; los comandos se escriben dentro de la sesión.

Para escribir estos comandos en una terminal, entra en **Comandos del shell**.
Con entrada o salida redirigida se usa directamente el shell textual:

```text
jx> abrir "C:\datos con espacios\datos.json"
jx> props
jx> index --fts --batch 50000
jx> nombre = Antonio
jx> nombre ~i ari
jx> edad >= 30 & ciudad = Madrid
jx> count ciudad = Madrid | ciudad = Toledo
jx> limite 1
jx> ciudad ?
jx> n
jx> ver 1
jx> e 1 "C:\datos con espacios\objeto.json"
jx> verify
jx> q
```

Abrir un JSON **no lo indexa automáticamente**. Si existe su sidecar
`datos.json.jindex`, sano y completo, se abre ese índice; en caso contrario queda
seleccionado el JSON y se indica usar `index`. `props` permite descubrir sus
propiedades antes de indexar. Abrir un `.jindex` inválido muestra un error y
conserva la sesión anterior. Una apertura válida borra los resultados y la última
consulta de la fuente anterior.

## Comandos del shell

| Comando | Función |
| --- | --- |
| `abrir "FICHERO"` / `open "FICHERO"` | Selecciona un JSON o `.jindex`; las rutas de Windows conservan sus barras inversas. |
| `props` | Sin índice, descubre propiedades en los primeros 1000 objetos; con índice, muestra rutas indexadas. |
| `props N` | Descubre propiedades usando una muestra de N objetos del JSON. |
| `index [OPCIONES]` | Indexa la fuente y la deja lista para buscar; por defecto, todas las propiedades. |
| `PROP OP VALOR` / `buscar CONSULTA` | Busca; admite varios filtros con el mismo conector `&` o `\|`. |
| `count [CONSULTA]` | Cuenta una consulta nueva o la última búsqueda sin cambiar sus hits ni su página. |
| `n` | Siguiente página de la búsqueda activa. |
| `limite N` | Tamaño de página entre 1 y 10000; borra hits y última búsqueda. |
| `formato table\|json\|csv\|tsv` | Formato persistente en la sesión, sin mensaje de confirmación. |
| `top PROP [N]` | Valores más frecuentes, en el formato seleccionado. |
| `ver N` / `raw N` | Resultado N de la página actual, formateado o tal cual. Las posiciones empiezan en 1. |
| `id N` / `key CLAVE` | Lee por id interno (empieza en 0) o clave raíz. |
| `e N "FICHERO"` | Exporta los bytes exactos del resultado N, sin reformatearlos. |
| `verify [N]` | Verifica el índice y una muestra de objetos: 1000 por defecto, 0 para todos. Éxito: `Verificación correcta.` |
| `stats` | Estado, tamaños, modo de indexación y FTS. |
| `ayuda` | Ayuda del shell, también sin fuente abierta. |
| `q` | Sale. EOF también termina la sesión. |

Los errores de comandos se escriben en **stderr** y el shell continúa. EOF o `q`
terminan con código 0 aunque hubiera errores internos; un error al abrir el
fichero inicial termina con código 1.

### Indexación

Estos comandos se ejecutan **dentro del shell**:

```text
index
index --select nombre --select ciudad
index --all --fts --batch 50000
index --db "C:\indices\datos.jindex"
index --force --all --fts
```

- `--select PROP` es repetible y limita las propiedades; `--all` las incluye todas.
- `--fts` añade FTS5 con tokenizador trigram para búsquedas de texto.
- `--batch N` establece los objetos por transacción; debe ser positivo.
- `--db FICHERO` elige la ruta del índice; por defecto se usa el sidecar
  `FICHERO.json.jindex` junto al JSON. Si rehaces un índice abierto, conserva su ruta.
- **Precaución con `--force`: rehace el índice desde cero.** Revisa su ruta y las
  propiedades/FTS deseadas antes de usarlo. No modifica el JSON, pero sustituye
  el índice anterior; puede ser necesario al cambiar de versión del formato.

### Operadores y consultas

| Operador | Significado | Ejemplo |
| --- | --- | --- |
| `=` / `!=` | Igualdad / distinto | `nombre = Antonio` |
| `~` / `^` | Contiene / empieza por | `nombre ^ Ant` |
| `=i`, `!=i`, `~i`, `^i` | Los anteriores ignorando mayúsculas y acentos | `nombre =i maria` |
| `>`, `>=`, `<`, `<=` | Comparación numérica | `edad >= 30` |
| `?` / `!?` | Existe / falta la propiedad | `dato !?` |
| `null` | Existe y vale null; no equivale a ausencia | `dato null` |
| `~r` | Expresión regular sobre el valor almacenado | `nombre ~r "^Ant(o\|ó)"` |

```text
edad > 30 & ciudad = Toledo
ciudad = Madrid | ciudad = Toledo
etiqueta = "rock & roll"
ruta = "C:\datos con espacios\origen.json"
codigo ~r "^A\d+$"
```

`&` exige que se cumplan todos los filtros; `|` acepta cualquiera.
Una consulta puede encadenar `&` o encadenar `|`, pero **no mezclarlos**.
Separa los conectores con espacios. Entrecomilla los valores que contengan
`&` o `|` como texto literal. Las barras inversas son literales: no hace falta
duplicarlas en rutas de Windows o regex escritas en el shell. Las búsquedas
requieren propiedades indexadas.

## Formatos de salida

`formato` afecta a las búsquedas y a `top`, no es un subcomando ni un argumento
externo. El banner y los mensajes de apertura/indexación siguen siendo texto;
al procesar stdout desde scripts hay que saltarlos.

- `table`: tabla del shell con posiciones de resultado, ids y vistas por `ver`.
- `json`: búsquedas con el envelope
  `{count,ms,next_after,hits:[{id,key,type,value,offset,length}]}`. `count` en una
  búsqueda es el número de hits de esa página, no el total de coincidencias.
- `csv` / `tsv`: búsquedas con columnas `id,key,type,value,offset,length`.
- `count` imprime solo el número, salvo en JSON, donde imprime `{"count": N}`.
- `top` respeta los cuatro formatos; en JSON devuelve un array de
  `{"value": "...", "type": "string", "count": N}`.

`offset` y `length` señalan bytes del objeto en el JSON original; `next_after`
es el cursor de paginación por id. El JSON original debe seguir disponible para
leer, exportar y verificar los objetos.

## Limitaciones

- Se exploran objetos del **primer nivel** del objeto raíz y sus propiedades
  directas, no rutas anidadas como `direccion.ciudad`.
- Los valores anidados se almacenan como texto JSON, truncado a **4 KB**;
  `~` y `~r` solo buscan sobre el texto almacenado.
- Los objetos mayores de **8 MB** conservan clave y posición, pero se indexan
  **sin propiedades**.
- El tamaño del índice depende de los datos y opciones: **todas las propiedades
  más FTS pueden producir un índice mayor que el JSON**.
- Si cambia el JSON, el índice puede dejar de ser válido; verificar no actualiza
  el índice. No se garantiza actualización incremental de un JSON modificado.

## Desarrollo

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`crates/core` contiene scanner, parser, índice y búsquedas; `crates/cli` contiene
el shell y sus pruebas de integración.

## Licencia

MIT. Consulta [LICENSE](LICENSE).
