# json-explorer

[![CI](https://github.com/espiroaka/json-explorer/actions/workflows/ci.yml/badge.svg)](https://github.com/espiroaka/json-explorer/actions/workflows/ci.yml)
![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)
![Rust](https://img.shields.io/badge/rust-1.87%2B-orange.svg)

> **TL;DR (English).** `jexplorer` explores multi-gigabyte JSON files without loading the
> document into memory. It streams the file once, indexes the top-level objects into a
> folder of sharded SQLite databases, and answers queries by reading each object back at
> its byte offset in the original file. The JSON is never copied, reformatted or modified.
> Arrow-key menus in a terminal, plus a textual shell for scripts.
>
> La documentación completa está en español más abajo.

**json-explorer** explora JSON grandes sin cargar el documento completo en memoria.
`jexplorer` abre una fuente, descubre propiedades, crea un índice SQLite repartido en
varios fragmentos dentro de una carpeta y consulta los objetos leyendo sus posiciones en el
JSON original. En una terminal muestra menús navegables con flechas; con entrada o salida
redirigida conserva el shell textual para scripts. El JSON no se modifica ni se copia al
indexarlo.

## Índice

- [Por qué y con qué compararlo](#por-qué-y-con-qué-compararlo)
- [Instalación](#instalación)
- [Inicio rápido](#inicio-rápido)
- [Interfaz con flechas](#interfaz-con-flechas)
- [Comandos del shell](#comandos-del-shell)
  - [Dónde vive el índice](#dónde-vive-el-índice)
  - [Indexación](#indexación)
  - [Operadores y consultas](#operadores-y-consultas)
- [Formatos de salida](#formatos-de-salida)
- [Limitaciones](#limitaciones)
- [Cómo está escrito este proyecto](#cómo-está-escrito-este-proyecto)
- [Desarrollo y contribuciones](#desarrollo-y-contribuciones)
- [Licencia](#licencia)

## Por qué y con qué compararlo

Un JSON de varios gigabytes no cabe en la memoria de un parser clásico, y sin embargo es
el formato en el que acaban muchos volcados de bases de datos, logs o datasets.
`jexplorer` lo ataja con dos ideas:

- **Escáner de streaming.** Recorre el objeto raíz en ventanas de 8 MB sin parsear el
  documento entero y registra, por objeto, su clave, su `id` interno y el par
  `(offset, length)` de sus bytes exactos.
- **Índice fragmentado.** Las propiedades indexadas viven en varias bases SQLite (FTS5
  incluido) dentro de una misma carpeta. La consulta recorre los fragmentos y devuelve
  solo una página de resultados; cada objeto se lee del JSON cuando lo pides.

Consecuencia práctica: indexar no duplica el fichero y consultar no depende de cuánto mida
el JSON, sino de cuántos objetos devuelvas. El JSON original tiene que seguir disponible
para leer, exportar y verificar.

| | `jexplorer` | `jq` | Importar a una BD |
| --- | --- | --- | --- |
| Memoria con un JSON de 4 GB | Constante (ventanas de 8 MB) | Carga el documento entero | Depende del import |
| Repetir consultas distintas | Índice persistente | Recorre el fichero en cada consulta | Sí, después de importarlo |
| Conservar los bytes originales | Sí (`raw N`, `e N`) | Reformatea | Se pierden al normalizar |
| Exploración interactiva | Menús con flechas | Tuberías | Cliente externo |
| Rutas anidadas (`direccion.ciudad`) | No: solo primer nivel | Sí | Sí |

`jq --stream` también puede recortar la memoria, a costa de consultas mucho menos
legibles; aquí no hay que elegir entre ambas cosas.

No compite con `jq` para transformar documentos, ni con una BD relacional para joins o
datos anidados: está pensado para **preguntar muchas veces a un volcado plano que no cabe
en RAM** sin duplicarlo.

## Instalación

Requiere Rust 1.87 o superior y un linker de C (Visual Studio Build Tools o MinGW en
Windows; toolchain de C en Linux/macOS). SQLite se compila embebido.

```bash
cargo install --path crates/cli     # o: cargo build --release
```

El ejecutable es `target/release/jexplorer` (`target\release\jexplorer.exe` en Windows).
Los ejemplos suponen que `jexplorer` está en el `PATH`; también puedes usar su ruta
completa.

## Inicio rápido

El documento debe tener un objeto raíz cuyas claves identifiquen los objetos:

```json
{
  "a": { "nombre": "Antonio", "edad": 42, "ciudad": "Toledo", "dato": null },
  "b": { "nombre": "MARÍA", "edad": 30, "ciudad": "Madrid" }
}
```

Inicia el navegador vacío, o abre directamente un JSON o la carpeta de un índice:

```bat
jexplorer
jexplorer "C:\datos con espacios\datos.json"
jexplorer "%LOCALAPPDATA%\json-explorer\indexes\datos-a1b2c3d4e5f60718"
jexplorer --help
jexplorer --version
```

Sin argumentos muestra el menú principal; con entrada o salida redirigida,
un banner y el shell textual. `--help` y `--version` funcionan sin fichero. **No existen subcomandos externos**:
`jexplorer index ...`, `jexplorer search ...`, `jexplorer shell ...` y el argumento
externo `--db` no son la interfaz; los comandos se escriben dentro de la sesión.
Para teclearlos, entra en **Comandos del shell**.

Salida real de una sesión sobre un JSON de 4.000 objetos, con las líneas de progreso y el
final de la lista de resultados recortados:

```text
jx> abrir "C:\datos\clientes.json"
propiedad     apariciones  tipos
activo              1.000  bool
apellidos           1.000  string
cifras              1.000  array
descripcion         1.000  null, string
direccion           1.000  object
edad                1.000  number
estado              1.000  string
nombre              1.000  string
datos               1.000  array
JSON: C:\datos\clientes.json
Usa `index` para indexar todas las propiedades, o `index --fts` para añadir FTS.

jx> index --all --jobs 2
Indexando C:\datos\clientes.json
      → %LOCALAPPDATA%\json-explorer\indexes\clientes-a1b2c3d4e5f60718
Objetos indexados:  4.000
Propiedades:        activo, apellidos, cifras, descripcion, direccion, edad, estado, nombre, datos
Fragmentos:         2
Tiempo:             0.1 s
Tamaño del índice:  2.06 MB

jx> nombre =i maria
  1         68  id68                       nombre = María                               [14966 · 207 B]
  2         85  id85                       nombre = María                               [18755 · 201 B]
  3         97  id97                       nombre = María                               [21394 · 255 B]
  4        109  id109                      nombre = María                               [24052 · 202 B]
  …

20 resultado(s) en 1 ms   `n` para más, `ver <n>` para abrir

jx> ver 1
{
  "activo": true,
  "apellidos": "García López",
  "cifras": [
    2020,
    2025
  ],
  "descripcion": null,
  "direccion": {
    "calle": "Calle 125",
    "ciudad": "Toledo"
  },
  "edad": 58,
  "estado": "pendiente",
  "nombre": "María",
  "datos": [
    2.9,
    0.1,
    1.7
  ]
}

jx> top estado 3
valor         tipo         apariciones
inactivo      string             1.048
baja          string             1.013
activo        string             1.000

jx> count estado = activo
1000

jx> q
```

Abrir un JSON **no lo indexa automáticamente**. Para encontrar su índice se miran, en este
orden, el almacén de índices (ver
[Dónde vive el índice](#dónde-vive-el-índice)) y la carpeta local `FICHERO.json.jindex`
junto al JSON. Si ninguno abre de forma válida, el JSON queda seleccionado y se indica usar
`index`; `props` permite descubrir sus propiedades antes de indexarlo. Un índice de un
formato antiguo, uno incompleto o un `.jindex` suelto del formato de un solo fichero muestran
un error explicativo y conservan la sesión anterior. Una apertura válida borra los resultados
y la última consulta de la fuente anterior.

## Interfaz con flechas

Ejecuta `jexplorer` o `jexplorer "C:\datos\datos.json"`:

- **↑ / ↓**: elige una acción o recorre todas las coincidencias de una búsqueda.
- **Enter / →**: abre una acción o el objeto seleccionado del JSON.
- **← / Esc**: vuelve atrás; desde el visor conserva la página y el resultado.
- **RePág / AvPág**: cambia de página de resultados o desplaza el visor.
- **Inicio / Fin**: extremos de la página actual; en el visor, inicio/final del objeto.
- En la pantalla de coincidencias, **+/a** añade una condición, **-/z** quita la última y
  **t** lista los valores más frecuentes de una propiedad entre ellas (Enter sobre un valor
  lo convierte en condición).
- En el visor, **r** alterna original/formateado y **e** exporta los bytes originales.
- En los campos de texto, ← / → editan; Enter acepta y Esc cancela.
- En el menú principal, Esc o `q` salen.

Puedes buscar eligiendo propiedad, operador y valor, o escribir una consulta completa. El
navegador muestra el total de coincidencias y carga **solo una página** de objetos cada
vez; las flechas cruzan los límites de página automáticamente. El subtítulo recuerda las
condiciones activas de la búsqueda. El visor no tiene el límite de 200 líneas del shell
textual y ajusta las líneas al ancho de la terminal.

Los menús incluyen apertura, indexación, índices guardados, propiedades, estadísticas,
valores frecuentes, verificación y ajustes. **Comandos del shell** conserva las
operaciones avanzadas; indexar y verificar muestran su salida textual y esperan Enter para
volver al menú. Rehacer un índice requiere confirmación. **Última búsqueda** vuelve a
cargar los resultados desde la primera página.

Abrir un JSON sigue sin indexarlo automáticamente. Usa **Indexar JSON** si no hay un índice
válido. Los formatos `json`, `csv` y `tsv` afectan a los comandos del shell, no al navegador
con flechas.

## Comandos del shell

| Comando | Función |
| --- | --- |
| `abrir "FICHERO"` / `open "FICHERO"` | Selecciona un JSON o la carpeta de un índice; las rutas de Windows conservan sus barras inversas. |
| `props` | Sin índice, descubre propiedades en los primeros 1000 objetos; con índice, muestra rutas indexadas. |
| `props N` | Descubre propiedades usando una muestra de N objetos del JSON. |
| `index [OPCIONES]` | Indexa la fuente y la deja lista para buscar; por defecto, todas las propiedades. |
| `indices` | Lista los índices guardados: objetos, tamaño, fragmentos y avisos (`FTS`, `incompleto`, `formato antiguo`). |
| `indices abrir\|borrar\|rehacer N` | Abre, elimina o rehace el índice número N de esa lista (más reciente primero; `N` empieza en 1). `rehacer` admite `--cpus`, `--jobs` y `--low-priority`. |
| `PROP OP VALOR` / `buscar CONSULTA` | Busca; admite varios filtros con el mismo conector `&` o `\|`. |
| `count [CONSULTA]` | Cuenta una consulta nueva o la última búsqueda sin cambiar sus hits ni su página. |
| `n` | Siguiente página de la búsqueda activa. |
| `limite N` | Tamaño de página entre 1 y 10000; borra hits y última búsqueda. |
| `formato table\|json\|csv\|tsv` | Formato persistente en la sesión, sin mensaje de confirmación. |
| `top PROP [N] [CONSULTA]` | Valores más frecuentes, en el formato seleccionado; con consulta, solo dentro de sus coincidencias. |
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

### Dónde vive el índice

Un índice es una **carpeta** con `manifest.json` y sus fragmentos `shard-XXX.jxdb`:
los objetos se reparten entre los fragmentos por id y las búsquedas los recorren todos.
El número de fragmentos lo fija `--jobs`; sin esa opción se eligen según los núcleos
disponibles (entre 2 y 8), y `--cpus N` recorta cuántos núcleos puede usar la
construcción.

Por defecto `index` escribe en el almacén, bajo `indexes\<nombre>-<hash>`:

```text
%LOCALAPPDATA%\json-explorer              (Windows; si falta, %APPDATA%)
$XDG_DATA_HOME/json-explorer              (Linux/macOS; si falta, ~/.local/share/json-explorer)
```

El nombre combina el nombre del JSON con un hash de su ruta canónica, y
`JXPLORER_DATA_DIR` cambia la raíz del almacén. `indices` lista esas carpetas desde la
más reciente y `indices abrir|borrar|rehacer N` actúa sobre ellas; el menú
**Índices guardados** hace lo mismo con flechas. Borrar un índice no toca el JSON:
solo obliga a indexar de nuevo.

`index --db CARPETA` guarda el índice donde tú quieras, y si abres esa carpeta y
vuelves a ejecutar `index` reutiliza su ruta. Un índice fuera del almacén **no se
encuentra solo** al abrir el JSON: hay que abrirlo pasándole su carpeta a `abrir` (o
como argumento inicial de `jexplorer`). La única carpeta junto al JSON que sí se busca
automáticamente —después del almacén— es `FICHERO.json.jindex`.

### Indexación

Estos comandos se ejecutan **dentro del shell**:

```text
index
index --select nombre --select ciudad
index --all --fts --batch 50000
index --all --cpus 2 --low-priority
index --db "C:\indices\datos.jindex"
index --force --all --fts
```

- `--select PROP` es repetible y limita las propiedades; `--all` las incluye todas.
- `--fts` añade FTS5 con tokenizador trigram para búsquedas de texto.
- `--batch N` establece los objetos por transacción; entre 1 y 1.000.000.
- `--jobs N` fija cuántos fragmentos se escriben (1 a 64); se indexan en paralelo.
- `--cpus N` limita a N los núcleos que puede usar la construcción (0, por defecto, usa
  todos): se ajustan los fragmentos y los hilos de índice para no pasar de ese número.
  `--cpus 1` es el modo «no molesto»; tarda bastante más a cambio de dejar el equipo libre.
  Si además fijas `--jobs`, ese mandato gana: `--cpus` solo recorta el reparto automático.
- `--low-priority` (o `--baja-prioridad`) baja la prioridad del proceso mientras indexa,
  de modo que las aplicaciones abiertas ganan siempre el CPU; al terminar se restaura.
- `indices rehacer N` repite la configuración guardada en el manifiesto y acepta estas
  mismas opciones: `indices rehacer 2 --cpus 4 --low-priority`. En el menú **Índices
  guardados**, «Rehacer gastando menos CPU» escribe ese comando por ti.
- `--db CARPETA` elige dónde se guarda el índice; la crea si falta y, sin esa opción,
  se usa el almacén (ver [Dónde vive el índice](#dónde-vive-el-índice)). Rehacer un índice
  abierto conserva su ruta.
- **Precaución con `--force`: rehace el índice desde cero.** Revisa su ruta y las
  propiedades/FTS deseadas antes de usarlo. No modifica el JSON, pero sustituye
  el índice anterior; es necesario cuando el índice es de un formato antiguo, y
  `indices rehacer <n>` hace exactamente eso sin escribir el comando.

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

Cada operador acepta también su nombre —`igual`, `distinto`, `contiene`, `empieza`,
`mayor`, `mayor_igual`, `menor`, `menor_igual`, `existe`, `falta`, `null`, `regex`, más los
equivalentes en inglés `eq`, `ne`, `contains`, `prefix`, `gt`, `gte`, `lt`, `lte`, `exists`,
`missing`—: `nombre igual Antonio` y `nombre = Antonio` son la misma consulta.

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

## Cómo está escrito este proyecto

Este repositorio es **full vibe coded**. Todo lo que hay dentro —`crates/core`,
`crates/cli`, sus pruebas, el CI y esta documentación— lo han escrito agentes de IA a
partir de especificaciones, reproches y decisiones de diseño del maintainer, que revisa
diffs en lugar de teclear funciones.

Se avisa aquí porque cambia lo que puedes esperar:

- La **suite de pruebas y `verify` son la red de seguridad**, no la memoria de un autor:
  hay comportamientos que nadie «recuerda» y que solo sostienen los tests.
- El código optimiza lo que se pidió medir, no lo que suena a buena práctica: ficheros
  grandes, dependencias mínimas y casi ningún comentario.
- Reportar un bug sigue mereciendo la pena: un issue con un JSON que reproduzca el fallo es
  la mejor especificación que un agente puede recibir, y se cierra igual de rápido.

El flujo concreto —qué se comprueba antes de un merge y cómo se propone un cambio— está en
[`CONTRIBUTING.md`](CONTRIBUTING.md).

## Desarrollo y contribuciones

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`crates/core` contiene scanner, parser, índice y búsquedas; `crates/cli` contiene
el shell y sus pruebas de integración. El CI comprueba formato, clippy con `-D warnings` y
la suite en Linux, Windows y macOS.

Las contribuciones son bienvenidas: abre un issue antes de cualquier cambio de
comportamiento y pasa fmt, clippy y test en local antes de empujar. Ver
[`CONTRIBUTING.md`](CONTRIBUTING.md) y [`CHANGELOG.md`](CHANGELOG.md).

## Licencia

MIT. Consulta [LICENSE](LICENSE).
