# Contribuir

Gracias por leer hasta aquí. Este proyecto es **full vibe coded** (ver el README): el
código lo escriben agentes de IA y el maintainer revisa los diffs. Eso no baja el listón de
lo que se acepta, pero cambia en qué se fija la revisión.

## Antes de nada

- Requiere Rust 1.87 o superior y un linker de C (SQLite se compila embebido).
- Todo cambio de **comportamiento** se comenta antes en un issue. Un PR que redefine qué
  hace `index`, cómo se resuelve una consulta o dónde se guarda un índice se cierra sin
  revisión a menos que haya discusión previa.
- Los cambios de rendimiento se justifican con una medición en un fichero real, no con una
  idea. `cargo test` demuestra corrección; no demuestra velocidad.

## Comprobaciones locales

El CI ejecuta exactamente esto, en Linux, Windows y macOS, y falla con cualquier warning:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps   # con RUSTDOCFLAGS=-D warnings
```

`unsafe_code` está denegado a nivel de workspace y `clippy::all` eleva a error: si tu
cambio lo esquivas con un `#[allow]`, explica en el PR por qué era necesario.

## Qué se revisa de un cambio

1. **Que el JSON de entrada no se toca.** Este proyecto no escribe sobre el fichero
   original ni lo normaliza. Cualquier cosa que se acerque a eso necesita un argumento muy
   bueno.
2. **Un test que falle antes del cambio y pase después.** `crates/core/tests/integration.rs`
   y `crates/cli/tests/cli.rs` son la referencia; las pruebas de CLI conducen sesiones del
   shell por stdin y miran la salida literal.
3. **Ruta de error legible.** Los mensajes van en español, concretos y sin volcados de
   pila: «la propiedad `ciudad` no está indexada; disponibles: …» y no `Error: E0042`.
4. **Que el README se actualice en el mismo commit.** El README es la especificación viva:
   operadores, opciones de `index`, límites (4 KB, 8 MB, `--jobs` 1–64) y rutas del almacén
   están descritos ahí. Si tocas comportamiento sin tocar README, el PR se queda bloqueado.
5. **Dependencias nuevas con motivo.** SQLite va embebido y se evitan crates que solo
   ahorren cinco líneas.

## Estilo

- Sin comentarios por defecto. Uno solo se justifica cuando explica una restricción no
  obvia, un invariante o el porqué de un workaround.
- No se añaden shims de compatibilidad «por si acaso»: un índice de formato antiguo se
  rehace con `--force`, no se lee con dos código paths.
- Los límites que el usuario ve (`limite`, `--jobs`, `--batch`) se documentan con su rango
  en la tabla de comandos del README.

## Proponer un cambio

1. Issue con: versión de `jexplorer --version`, SO, tamaño y forma del JSON, y el comando
   exacto que falla. Si puedes adjuntar (o pegar) un recorte de 200 objetos que reproduzca
   el problema, entra en la cola directa.
2. Un solo cambio lógico por PR; enlaza el issue.
3. `cargo fmt --all` antes del commit. Mensajes de commit en imperativo, una línea, y el
   cuerpo solo si el porqué no se deduce del diff.
4. No toques `CHANGELOG.md`: se rellena al cerrar la versión.

## Empezar por algo pequeño

Los issues marcados como `good first issue` son zonas donde ya se falló una vez: mensajes,
tablas, casos extremos del parser de consultas. Son los más fáciles de verificar y los que
más se agradecen. Si no ves ninguno abierto, pide uno en un issue.
