## Qué cambia

Una frase. Enlaza el issue si lo hay (`Fixes #…`), o explica por qué no hacía falta.

## Cómo se comprueba

```text
jx> abrir "…"
jx> …
```

Qué mide el cambio de rendimiento, si lo hay, y sobre qué fichero. Un «es más rápido» sin
números se pide de nuevo.

## Checklist

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Hay un test que falla sin este cambio y pasa con él
- [ ] El README refleja el cambio (comandos, operadores, opciones de `index`, límites o
      rutas del almacén)
- [ ] Ninguna salida nueva expone rutas de usuario, volcados de pila ni códigos opacos
- [ ] Sin dependencias nuevas, o con su justificación arriba
