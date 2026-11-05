# Changelog

Formato [Keep a Changelog](https://keepachangelog.com/es/1.1.0/); versionado
[SemVer](https://semver.org/lang/es/). Los cambios en `master` aún sin etiquetar aparecen en
`Sin publicar`.

## [Sin publicar]

Nada todavía.

## [0.2.0] — 2026-11-05

Reescritura de la interfaz, del almacenamiento del índice y del camino de consulta.

### Añadido

- Navegador interactivo con flechas: menús de apertura, indexación, índices guardados,
  propiedades, estadísticas, valores frecuentes, verificación y ajustes; visor que ajusta las
  líneas al ancho de la terminal, sin el límite de 200 líneas del shell.
- Índices repartidos en **fragmentos** (`manifest.json` + `shard-XXX.jxdb`) dentro de una
  carpeta, escritos en paralelo: `--jobs` elige cuántos (1–64) y, sin él, se reparten según
  los núcleos disponibles (2–8).
- Control del coste de la construcción: `--cpus N`, `--low-priority` / `--baja-prioridad`.
- FTS5 con tokenizador trigram mediante `index --fts`.
- Operadores nuevos: `!=`, `>`, `>=`, `<`, `<=`, `?`, `!?`, `null`, `~r` y las variantes sin
  mayúsculas ni acentos `=i`, `!=i`, `~i`, `^i`.
- Consultas compuestas con `&` o `|`, `count [CONSULTA]` sin tocar los hits de la búsqueda
  activa, y refinar resultados dentro de la última búsqueda (`+/a` y `-/z` en el navegador).
- `formato table|json|csv|tsv`, `top PROP [N] [CONSULTA]`, `key CLAVE`, `verify [N]` y
  `limite N`.
- Almacén de índices por defecto (`%LOCALAPPDATA%\json-explorer`,
  `$XDG_DATA_HOME/json-explorer`, configurable con `JXPLORER_DATA_DIR`) con `indices` y
  `indices abrir|borrar|rehacer N`.
- La primera página de resultados se muestra mientras se calcula el recuento exacto.
- CI con `cargo fmt --check`, `clippy -D warnings`, `rustdoc -D warnings` y `cargo test` en
  Linux, Windows y macOS.
- `--select`, `--all`, `--batch`, `--db` y `--force` se siguen escribiendo igual, pero ahora
  **dentro** del shell.

### Cambiado

- `jexplorer` sin subcomandos: un único argumento opcional (JSON o carpeta de índice), o el
  shell vacío.
- Indexar recorre el JSON en streaming con ventanas de 8 MB; la construcción paralela de
  fragmentos baja alrededor de 3× el tiempo y solapa FTS con la escritura.
- Las búsquedas consultan todos los fragmentos a la vez en lugar de uno tras otro.
- Los valores `null` no entran en el índice de propiedades y los valores anidados se guardan
  como texto JSON truncado a 4 KB.
- Los objetos de más de 8 MB se registran con clave y posición, pero **sin propiedades**.
- Los errores de comando se escriben por stderr y la sesión continúa; `q` o EOF salen con
  código 0 salvo fallo al abrir el fichero inicial.

### Quitado

- La interfaz de subcomandos externos (`jexplorer props FICHERO`, `jexplorer index FICHERO`,
  `jexplorer search --db …`) y `--db` como argumento externo.
- El índice de un solo fichero `<fichero>.jindex` como formato de escritura. Se detecta al
  abrirlo, con un error que explica cómo rehacerlo; la carpeta `FICHERO.json.jindex` junto al
  JSON sigue buscándose automáticamente.

## [0.1.0] — 2026-10-30

Primera versión. Interfaz de subcomandos (`props`, `index`, `search` y lectura de objetos),
escáner del objeto raíz en streaming, índice SQLite de un solo fichero con las propiedades de
primer nivel, operadores `eq` / `contains` / `prefix`, paginación por id y shell textual con
history.
