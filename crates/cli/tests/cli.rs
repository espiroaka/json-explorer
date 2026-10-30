use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const DATA: &str = r#"{
  "a": {"nombre":"Antonio","edad":42,"estado":"activo","activo":true,"dato":null,"ciudad":"Toledo","etiqueta":"rock & roll","ruta":"C:\\datos con espacios\\origen.json","codigo":"A42"},
  "b": {"nombre":"Antón","edad":7,"estado":"baja","activo":false,"ciudad":"Cuenca","etiqueta":"uno | dos"},
  "c": {"nombre":"MARÍA","edad":30,"estado":"activo","activo":true,"ciudad":"Madrid"},
  "d": {"nombre":"José","edad":100,"ciudad":"Madrid","dato":"x"}
}"#;

struct Ctx {
    dir: TempDir,
    json: PathBuf,
    db: PathBuf,
}

fn ctx_with(data: &str) -> Ctx {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("datos con espacios.json");
    fs::write(&json, data).unwrap();
    let db = PathBuf::from(format!("{}.jindex", json.display()));
    Ctx { dir, json, db }
}

fn ctx() -> Ctx {
    ctx_with(DATA)
}

fn indexed_ctx() -> Ctx {
    let ctx = ctx();
    clean(&run_file(&ctx.json, "index --fts --batch 2\nq\n"));
    assert!(ctx.db.is_file());
    ctx
}

fn run_raw(args: &[&str], input: &str) -> Output {
    let state = tempfile::tempdir().unwrap();
    run_raw_with_state(args, input, state.path())
}

fn run_raw_with_state(args: &[&str], input: &str, state: &Path) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jexplorer"))
        .args(args)
        .env("APPDATA", state)
        .env("XDG_STATE_HOME", state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_owned();
    // Alimentar stdin en paralelo evita bloquearse si el shell llena stdout.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    out
}

fn run_file(file: &std::path::Path, input: &str) -> Output {
    run_raw(&[file.to_str().unwrap()], input)
}

fn stored_recent_paths(state: &Path) -> Vec<PathBuf> {
    let storage = state.join("jexplorer").join("recientes.sqlite3");
    if !storage.exists() {
        return Vec::new();
    }
    let conn =
        rusqlite::Connection::open_with_flags(storage, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    conn.prepare("SELECT path FROM recent_files ORDER BY opened DESC")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0).map(PathBuf::from))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn success(out: &Output) -> String {
    assert!(
        out.status.success(),
        "status: {}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn clean(out: &Output) -> String {
    let text = success(out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("error:"), "stderr: {err}");
    assert!(!err.contains("panicked"), "stderr: {err}");
    text
}

fn shell_error(out: &Output) -> String {
    let text = success(out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("error:"), "stderr: {err}\nstdout: {text}");
    assert!(!err.contains("panicked"), "stderr: {err}");
    text
}

// Saltar el banner no debe perder documentos JSON consecutivos.
fn json_documents(text: &str) -> Vec<Value> {
    let mut rest = text;
    let mut docs = Vec::new();
    while !rest.is_empty() {
        let candidate = rest.trim_start();
        if candidate.starts_with('{') || candidate.starts_with('[') {
            let mut stream = serde_json::Deserializer::from_str(candidate).into_iter::<Value>();
            if let Some(Ok(doc)) = stream.next() {
                docs.push(doc);
                rest = &candidate[stream.byte_offset()..];
                continue;
            }
        }
        rest = match rest.find('\n') {
            Some(end) => &rest[end + 1..],
            None => "",
        };
    }
    docs
}

fn searches(text: &str) -> Vec<Value> {
    json_documents(text)
        .into_iter()
        .filter(|doc| doc.get("hits").is_some())
        .collect()
}

fn hit_ids(doc: &Value) -> Vec<i64> {
    doc["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["id"].as_i64().unwrap())
        .collect()
}

fn assert_hits(doc: &Value, expected: &[i64]) {
    assert_eq!(hit_ids(doc), expected, "{doc}");
    assert_eq!(doc["count"].as_u64().unwrap(), expected.len() as u64);
    assert!(doc["ms"].is_number(), "{doc}");
    assert!(doc.get("next_after").is_some(), "{doc}");
    for hit in doc["hits"].as_array().unwrap() {
        assert!(hit["key"].is_string());
        assert!(hit["type"].is_string());
        assert!(hit["value"].is_string());
        assert!(hit["offset"].as_u64().is_some());
        assert!(hit["length"].as_u64().unwrap() > 0);
    }
}

#[test]
fn sesion_vacia_banner_ayuda_eof_y_q() {
    let eof = run_raw(&[], "");
    let banner = clean(&eof);
    assert!(banner.contains("abrir"));
    assert!(banner.contains("ayuda"));
    assert!(!banner.trim().is_empty());
    assert_eq!(eof.status.code(), Some(0));

    let help = clean(&run_raw(&[], "ayuda\nq\n"));
    for command in ["abrir", "index", "count", "limite", "formato", "verify"] {
        assert!(help.contains(command), "falta {command}: {help}");
    }
    let quit = clean(&run_raw(&[], "q\nprops\n"));
    assert_eq!(quit, banner, "q debe detener la lectura de comandos");

    let out = run_raw(&[], "props\nnombre = Antonio\nayuda\n");
    assert!(shell_error(&out).contains("abrir"));
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn clap_help_version_y_argumentos_antiguos() {
    let help = run_raw(&["--help"], "");
    let text = clean(&help);
    assert!(text.contains("Usage:"));
    assert!(text.contains("--help"));
    assert!(text.contains("--version"));
    assert!(text.contains("abrir"));
    assert!(!text.contains("Commands:"));
    assert!(!text.contains("--db"));
    assert!(help.stderr.is_empty());

    let version = clean(&run_raw(&["--version"], ""));
    assert!(version.starts_with("jexplorer "));
    assert_eq!(version.lines().count(), 1);

    let ctx = ctx();
    for command in [
        "index", "props", "search", "get", "export", "groupby", "verify", "stats", "shell",
    ] {
        let out = run_raw(&[command, ctx.json.to_str().unwrap()], "");
        assert!(
            !out.status.success(),
            "subcomando externo aceptado: {command}"
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("error:"));
    }
    for args in [
        vec!["--db", ctx.db.to_str().unwrap()],
        vec!["--format", "json"],
        vec!["--limit", "1"],
    ] {
        let out = run_raw(&args, "");
        assert!(
            !out.status.success(),
            "argumentos antiguos aceptados: {args:?}"
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("error:"));
    }
    assert!(!ctx.db.exists());
}

#[test]
fn fichero_inicial_inexistente_o_indice_invalido_sale_con_uno() {
    let ctx = ctx();
    let missing = ctx.dir.path().join("no existe.json");
    let invalid = ctx.dir.path().join("invalido.jindex");
    let malformed = ctx.dir.path().join("JSON roto.json");
    fs::write(&invalid, b"esto no es SQLite").unwrap();
    fs::write(&malformed, b"no es JSON").unwrap();
    for path in [&missing, &invalid, &malformed] {
        let state = tempfile::tempdir().unwrap();
        let out = run_raw_with_state(&[path.to_str().unwrap()], "", state.path());
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("error:"));
        assert!(!String::from_utf8_lossy(&out.stderr).contains("panicked"));
        assert!(stored_recent_paths(state.path()).is_empty());
        assert!(!state.path().join("jexplorer").exists());
    }
}

#[test]
fn abrir_json_no_indexa_automaticamente_y_props_descubre() {
    let ctx = ctx();
    let text = clean(&run_file(&ctx.json, "props\nprops 1\nq\n"));
    assert!(text.contains("nombre"));
    assert!(text.contains("edad"));
    assert!(text.contains("index"));
    assert!(!ctx.db.exists());

    let out = run_file(&ctx.json, "formato json\nnombre = Antonio\nprops\nq\n");
    let text = shell_error(&out);
    assert!(
        text.contains("nombre"),
        "el shell debe seguir después del error"
    );
    assert!(searches(&text).is_empty());
    assert!(!ctx.db.exists());
}

#[test]
fn props_muestra_solo_la_muestra_pedida_y_por_defecto_mil() {
    let mut data = String::from("{");
    for i in 0..1001 {
        if i != 0 {
            data.push(',');
        }
        let extra = match i {
            1 => ",\"solo_segundo\":true",
            1000 => ",\"solo_1001\":true",
            _ => "",
        };
        data.push_str(&format!("\"k{i:04}\":{{\"nombre\":\"objeto\"{extra}}}"));
    }
    data.push('}');
    let ctx = ctx_with(&data);
    let banner = clean(&run_file(&ctx.json, "q\n"));
    let default = clean(&run_file(&ctx.json, "props\nq\n"));
    let default = default.strip_prefix(&banner).unwrap();
    assert!(default.contains("solo_segundo"));
    assert!(!default.contains("solo_1001"));
    let one = clean(&run_file(&ctx.json, "props 1\nq\n"));
    let one = one.strip_prefix(&banner).unwrap();
    assert!(one.contains("nombre"));
    assert!(!one.contains("solo_segundo"));
    let all = clean(&run_file(&ctx.json, "props 1001\nq\n"));
    let all = all.strip_prefix(&banner).unwrap();
    assert!(all.contains("solo_1001"));
    assert!(!ctx.db.exists());
}

#[test]
fn index_por_defecto_todas_fts_y_reabrir_json_o_indice() {
    let ctx = ctx();
    let text = clean(&run_file(
        &ctx.json,
        "index --fts --batch 2\nstats\nprops\nformato json\nnombre = Antonio\nq\n",
    ));
    assert!(ctx.db.is_file());
    assert!(text.contains("FTS:     sí"));
    assert!(text.contains("Modo:    todas las propiedades"));
    assert!(text.contains("ciudad"));
    let docs = searches(&text);
    assert_eq!(docs.len(), 1);
    assert_hits(&docs[0], &[0]);
    assert_eq!(docs[0]["hits"][0]["key"], "a");
    assert_eq!(docs[0]["hits"][0]["type"], "string");

    for file in [&ctx.json, &ctx.db] {
        let text = clean(&run_file(file, "formato json\nnombre = Antonio\nq\n"));
        let docs = searches(&text);
        assert_eq!(docs.len(), 1);
        assert_hits(&docs[0], &[0]);
    }
}

#[test]
fn abrir_y_open_rutas_con_espacios_desde_sesion_vacia() {
    let ctx = ctx();
    let text = clean(&run_raw(
        &[],
        &format!(
            "abrir \"{}\"\nprops 2\nindex --all\nopen \"{}\"\nformato json\nbuscar nombre = Antonio\nq\n",
            ctx.json.display(),
            ctx.db.display()
        ),
    ));
    let docs = searches(&text);
    assert_eq!(docs.len(), 1);
    assert_hits(&docs[0], &[0]);
}

#[test]
fn recientes_registran_inicio_sidecar_y_comandos_con_rutas_canonicas_distintas() {
    let ctx = ctx();
    let state = tempfile::tempdir().unwrap();
    let unicode = ctx.dir.path().join("María 東京 con espacios.json");
    fs::write(&unicode, DATA).unwrap();
    clean(&run_raw_with_state(
        &[ctx.json.to_str().unwrap()],
        &format!("index --batch 2\nabrir \"{}\"\nq\n", unicode.display()),
        state.path(),
    ));
    let json_path = ctx.json.canonicalize().unwrap();
    let index_path = ctx.db.canonicalize().unwrap();
    let unicode_path = unicode.canonicalize().unwrap();
    assert_eq!(
        stored_recent_paths(state.path()),
        vec![unicode_path.clone(), json_path.clone()],
        "indexar no debe registrar una apertura del índice"
    );

    // Reabrir el JSON con sidecar válido toma el retorno anticipado de open_file.
    clean(&run_raw_with_state(
        &[ctx.json.to_str().unwrap()],
        "q\n",
        state.path(),
    ));
    assert_eq!(
        stored_recent_paths(state.path()),
        vec![json_path.clone(), unicode_path.clone()]
    );
    let alias = ctx.dir.path().join(".").join(unicode.file_name().unwrap());
    clean(&run_raw_with_state(
        &[],
        &format!(
            "open \"{}\"\nabrir \"{}\"\nopen \"{}\"\nq\n",
            ctx.db.display(),
            unicode.display(),
            alias.display()
        ),
        state.path(),
    ));
    assert_eq!(
        stored_recent_paths(state.path()),
        vec![unicode_path.clone(), index_path.clone(), json_path.clone()]
    );
    fs::remove_file(&unicode).unwrap();
    clean(&run_raw_with_state(
        &[ctx.db.to_str().unwrap()],
        "q\n",
        state.path(),
    ));
    assert_eq!(
        stored_recent_paths(state.path()),
        vec![index_path, unicode_path, json_path],
        "las rutas desaparecidas se conservan entre procesos"
    );
}

#[test]
fn fallo_guardando_recientes_avisa_sin_fallar_apertura_ni_consulta() {
    let ctx = indexed_ctx();
    let state = tempfile::tempdir().unwrap();
    let storage = state.path().join("jexplorer").join("recientes.sqlite3");
    // Una carpeta en lugar de la base fuerza un error de almacenamiento portable.
    fs::create_dir_all(&storage).unwrap();
    let out = run_raw_with_state(
        &[ctx.db.to_str().unwrap()],
        "formato json\nnombre = Antonio\nq\n",
        state.path(),
    );
    let text = clean(&out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("aviso: no se pudo guardar"), "{err}");
    assert!(!err.contains('\u{1b}'));
    let docs = searches(&text);
    assert_eq!(docs.len(), 1);
    assert_hits(&docs[0], &[0]);
}

#[cfg(unix)]
#[test]
fn aviso_recientes_sanitiza_rutas_con_controles_incluso_sin_terminal() {
    let ctx = ctx();
    let state = tempfile::tempdir().unwrap();
    let root = state.path().join("estado\u{1b}[31m\nmalicioso");
    fs::create_dir_all(root.join("jexplorer").join("recientes.sqlite3")).unwrap();
    let out = run_raw_with_state(&[ctx.json.to_str().unwrap()], "q\n", &root);
    clean(&out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("aviso: no se pudo guardar"), "{err}");
    assert!(!err.contains('\u{1b}'), "{err}");
    assert_eq!(
        err.lines().count(),
        1,
        "el aviso no debe inyectar líneas: {err}"
    );
}

#[test]
fn index_select_repetible_db_personalizada_y_force() {
    let ctx = ctx();
    let db = ctx.dir.path().join("indice elegido.jindex");
    let out = run_file(
        &ctx.json,
        &format!(
            "index --select nombre --select ciudad --batch 1 --db \"{}\"\nformato json\nnombre = Antonio\nciudad = Madrid\nedad > 30\nq\n",
            db.display()
        ),
    );
    let text = shell_error(&out);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no está indexada"));
    assert!(db.is_file());
    assert!(!ctx.db.exists());
    let docs = searches(&text);
    assert_eq!(docs.len(), 2);
    assert_hits(&docs[0], &[0]);
    assert_hits(&docs[1], &[2, 3]);

    let text = clean(&run_file(
        &db,
        "index --force --all --fts --batch 2\nstats\nformato json\nedad > 30\nq\n",
    ));
    assert!(text.contains("todas las propiedades"));
    assert!(text.contains("FTS:     sí"));
    let docs = searches(&text);
    assert_eq!(docs.len(), 1);
    assert_hits(&docs[0], &[0, 3]);
}

#[test]
fn json_con_sidecar_incompleto_o_obsoleto_no_lo_abre_ni_reindexa() {
    for incomplete in [true, false] {
        let ctx = indexed_ctx();
        if incomplete {
            let conn = rusqlite::Connection::open(&ctx.db).unwrap();
            assert_eq!(
                conn.execute("UPDATE meta SET value='running' WHERE key='status'", [])
                    .unwrap(),
                1
            );
        } else {
            fs::write(&ctx.json, format!("{DATA}\n ")).unwrap();
        }
        let before = fs::read(&ctx.db).unwrap();
        let out = run_file(&ctx.json, "props\nformato json\nnombre = Antonio\nq\n");
        let text = shell_error(&out);
        assert!(text.contains("index"));
        assert!(text.contains("nombre"));
        assert!(searches(&text).is_empty());
        assert_eq!(fs::read(&ctx.db).unwrap(), before);
    }
}

#[test]
fn paginacion_desde_id_cero_y_count_no_cambia_hits_ni_consulta() {
    let ctx = indexed_ctx();
    let text = clean(&run_file(
        &ctx.db,
        "limite 1\nformato json\nestado = activo\ncount\ncount nombre = José\nver 1\nn\ncount\nq\n",
    ));
    let docs = searches(&text);
    assert_eq!(docs.len(), 2);
    assert_hits(&docs[0], &[0]);
    assert_eq!(docs[0]["next_after"], 0);
    assert_hits(&docs[1], &[2]);
    let all = json_documents(&text);
    let counts: Vec<_> = all
        .iter()
        .filter(|doc| doc.get("hits").is_none() && doc.get("count").is_some())
        .collect();
    assert_eq!(counts.len(), 3);
    assert_eq!(*counts[0], serde_json::json!({"count": 2}));
    assert_eq!(*counts[1], serde_json::json!({"count": 1}));
    assert_eq!(*counts[2], serde_json::json!({"count": 2}));
    assert!(all.iter().any(|doc| doc["nombre"] == "Antonio"));
}

#[test]
fn operadores_conectores_valores_entrecomillados_y_backslashes_literales() {
    let ctx = indexed_ctx();
    let cases: &[(&str, &[i64])] = &[
        ("nombre = Antonio", &[0]),
        ("buscar nombre = Antonio", &[0]),
        ("nombre =i maria", &[2]),
        ("nombre ~i ari", &[2]),
        ("nombre ^i ANTO", &[0, 1]),
        ("nombre !=i antonio", &[1, 2, 3]),
        ("nombre != Antonio", &[1, 2, 3]),
        ("nombre ~ Ant", &[0, 1]),
        ("nombre ^ Ant", &[0, 1]),
        ("edad > 30", &[0, 3]),
        ("edad >= 30", &[0, 2, 3]),
        ("edad < 30", &[1]),
        ("edad <= 30", &[1, 2]),
        ("dato null", &[0]),
        ("dato ?", &[0, 3]),
        ("dato !?", &[1, 2]),
        ("nombre ~r \"^Ant(o|ó)\"", &[0, 1]),
        ("nombre ~r ^Ant(o|ó)", &[0, 1]),
        ("nombre ~r \"^(Antonio|José)$\" | edad < 10", &[0, 1, 3]),
        (r#"codigo ~r "^A\d+$""#, &[0]),
        ("edad > 30 & estado = activo", &[0]),
        ("edad >= 30 & estado = activo & ciudad = Toledo", &[0]),
        ("ciudad = Madrid | ciudad = Toledo", &[0, 2, 3]),
        (
            "nombre = Antonio | nombre = Antón | nombre = José",
            &[0, 1, 3],
        ),
        ("etiqueta = \"rock & roll\"", &[0]),
        ("etiqueta = \"uno | dos\"", &[1]),
        (
            "etiqueta = \"rock & roll\" | etiqueta = \"uno | dos\"",
            &[0, 1],
        ),
        (r"ruta = C:\datos con espacios\origen.json", &[0]),
        (r#"ruta = "C:\datos con espacios\origen.json""#, &[0]),
        ("nombre = Nadie", &[]),
    ];
    let mut input = String::from("formato json\n");
    for (query, _) in cases {
        input.push_str(query);
        input.push('\n');
    }
    input.push_str("q\n");
    let text = clean(&run_file(&ctx.db, &input));
    let docs = searches(&text);
    assert_eq!(docs.len(), cases.len(), "{text}");
    for (doc, (query, ids)) in docs.iter().zip(cases) {
        assert_eq!(hit_ids(doc).as_slice(), *ids, "consulta: {query}\n{doc}");
        assert_hits(doc, ids);
    }
}

#[test]
fn conectores_simbolicos_paginan_y_cuentan_sin_repetir_hits() {
    let ctx = indexed_ctx();
    let text = clean(&run_file(
        &ctx.db,
        "limite 1\nformato json\nestado = activo | ciudad = Madrid\nn\nn\ncount\ncount estado = activo & ciudad = Madrid\nn\nq\n",
    ));
    let docs = searches(&text);
    assert_eq!(docs.len(), 4);
    assert_hits(&docs[0], &[0]);
    assert_hits(&docs[1], &[2]);
    assert_hits(&docs[2], &[3]);
    assert_hits(&docs[3], &[]);
    let counts = json_documents(&text);
    assert!(counts.contains(&serde_json::json!({"count": 3})));
    assert!(counts.contains(&serde_json::json!({"count": 1})));
}

#[test]
fn errores_de_consulta_no_terminan_shell_ni_rompen_paginacion() {
    let ctx = indexed_ctx();
    let out = run_file(
        &ctx.db,
        "limite 1\nformato json\nestado = activo\nedad > 30 & estado = activo | ciudad = Madrid\nno_existe = x\nnombre ~r [\nn\nq\n",
    );
    let text = shell_error(&out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains('&') && err.contains('|'));
    assert!(err.contains("no está indexada"));
    assert!(err.contains("nombre"));
    assert!(err.matches("error:").count() >= 3, "{err}");
    let docs = searches(&text);
    assert_eq!(docs.len(), 2);
    assert_hits(&docs[0], &[0]);
    assert_hits(&docs[1], &[2]);
}

#[test]
fn index_flags_invalidos_no_panico_y_conserva_sesion() {
    let ctx = indexed_ctx();
    let invalid = [
        "index --desconocida",
        "index --batch 0",
        "index --batch nope",
        "index --batch",
        "index --select",
        "index --db",
    ];
    let mut input = String::from("limite 1\nformato json\nestado = activo\n");
    for command in invalid {
        input.push_str(command);
        input.push('\n');
    }
    input.push_str("ver 1\nn\nq\n");
    let out = run_file(&ctx.db, &input);
    let text = shell_error(&out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.matches("error:").count() >= invalid.len(), "{err}");
    let docs = searches(&text);
    assert_eq!(docs.len(), 2);
    assert_hits(&docs[0], &[0]);
    assert_hits(&docs[1], &[2]);
    assert!(
        json_documents(&text)
            .iter()
            .any(|doc| doc["nombre"] == "Antonio")
    );
}

#[test]
fn apertura_fallida_conserva_fuente_hits_y_pagina() {
    let ctx = indexed_ctx();
    let invalid = ctx.dir.path().join("indice roto.jindex");
    let missing = ctx.dir.path().join("JSON ausente.json");
    let malformed = ctx.dir.path().join("JSON malformado.json");
    fs::write(&invalid, b"no es un indice").unwrap();
    fs::write(&malformed, b"no es JSON").unwrap();
    let state = tempfile::tempdir().unwrap();
    let out = run_raw_with_state(
        &[ctx.db.to_str().unwrap()],
        &format!(
            "limite 1\nformato json\nestado = activo\nabrir \"{}\"\nopen \"{}\"\nabrir \"{}\"\nver 1\nn\nq\n",
            invalid.display(),
            missing.display(),
            malformed.display()
        ),
        state.path(),
    );
    assert_eq!(
        stored_recent_paths(state.path()),
        vec![ctx.db.canonicalize().unwrap()]
    );
    let text = shell_error(&out);
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .matches("error:")
            .count()
            >= 2
    );
    let docs = searches(&text);
    assert_eq!(docs.len(), 2);
    assert_hits(&docs[0], &[0]);
    assert_hits(&docs[1], &[2]);
    assert!(
        json_documents(&text)
            .iter()
            .any(|doc| doc["nombre"] == "Antonio")
    );
}

#[test]
fn cambiar_fuente_borra_hits_y_ultima_busqueda() {
    let first = indexed_ctx();
    for indexed in [false, true] {
        let second = ctx_with(r#"{"z":{"nombre":"Beatriz","edad":55}}"#);
        if indexed {
            clean(&run_file(&second.json, "index\nq\n"));
        }
        let export = second.dir.path().join("no debe exportarse.json");
        let index = if indexed { "" } else { "index\n" };
        let out = run_file(
            &first.db,
            &format!(
                "formato json\nnombre = Antonio\nabrir \"{}\"\ne 1 \"{}\"\ncount\nn\n{index}nombre = Beatriz\nq\n",
                second.json.display(),
                export.display()
            ),
        );
        let text = shell_error(&out);
        assert!(
            !export.exists(),
            "no deben quedar hits de la fuente anterior"
        );
        let docs = searches(&text);
        assert_eq!(
            docs.len(),
            2,
            "n no debe repetir la búsqueda anterior: {text}"
        );
        assert_hits(&docs[0], &[0]);
        assert_hits(&docs[1], &[0]);
        assert_eq!(docs[1]["hits"][0]["value"], "Beatriz");
        assert!(
            json_documents(&text)
                .iter()
                .all(|doc| { doc.get("hits").is_some() || doc.get("count").is_none() }),
            "count no debe reutilizar la consulta anterior: {text}"
        );
    }
}

#[test]
fn limite_valida_rango_y_borra_hits_y_busqueda() {
    let ctx = indexed_ctx();
    let export = ctx.dir.path().join("hits anteriores.json");
    let out = run_file(
        &ctx.db,
        &format!(
            "limite 10000\nlimite 1\nformato json\nestado = activo\nlimite 2\ne 1 \"{}\"\ncount\nn\nestado = activo\nlimite 0\nlimite 10001\nver 2\nq\n",
            export.display()
        ),
    );
    let text = shell_error(&out);
    assert!(!export.exists());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.matches("error:").count() >= 3, "{err}");
    let docs = searches(&text);
    assert_eq!(docs.len(), 2);
    assert_hits(&docs[0], &[0]);
    assert_hits(&docs[1], &[0, 2]);
    let all = json_documents(&text);
    assert!(all.iter().any(|doc| doc["nombre"] == "MARÍA"));
    assert!(
        all.iter()
            .all(|doc| doc.get("hits").is_some() || doc.get("count").is_none())
    );
}

#[test]
fn ver_raw_id_cero_key_export_bytes_y_verify() {
    let ctx = indexed_ctx();
    let export = ctx.dir.path().join("objeto exportado.json");
    let text = clean(&run_file(
        &ctx.db,
        &format!(
            "formato json\nnombre = Antonio\nver 1\nraw 1\nid 0\nkey b\ne 1 \"{}\"\nverify\nverify 0\nverify 2\nq\n",
            export.display()
        ),
    ));
    assert_eq!(text.matches("Verificación correcta.").count(), 3);
    assert!(text.contains("\n  \"nombre\": \"Antonio\""));
    let docs = json_documents(&text);
    assert_eq!(docs.len(), 5, "{text}");
    assert_hits(&docs[0], &[0]);
    for doc in &docs[1..4] {
        assert_eq!(doc["nombre"], "Antonio");
    }
    assert_eq!(docs[4]["nombre"], "Antón");
    let hit = &docs[0]["hits"][0];
    let offset = hit["offset"].as_u64().unwrap() as usize;
    let length = hit["length"].as_u64().unwrap() as usize;
    let source = fs::read(&ctx.json).unwrap();
    let exact = &source[offset..offset + length];
    assert_eq!(fs::read(&export).unwrap(), exact);
    assert!(text.contains(std::str::from_utf8(exact).unwrap()));
}

#[test]
fn top_json_y_count_texto_solo_numero() {
    let ctx = indexed_ctx();
    let text = clean(&run_file(
        &ctx.db,
        "formato json\ntop estado\ntop estado 1\nq\n",
    ));
    let docs = json_documents(&text);
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[0].as_array().unwrap().len(), 2);
    assert_eq!(
        docs[0][0],
        serde_json::json!({"value": "activo", "type": "string", "count": 2})
    );
    assert_eq!(
        docs[0][1],
        serde_json::json!({"value": "baja", "type": "string", "count": 1})
    );
    assert_eq!(docs[1].as_array().unwrap().len(), 1);
    assert_eq!(docs[1][0], docs[0][0]);

    // El prefijo idéntico es solo el banner; count no añade etiquetas ni tiempos.
    let banner = clean(&run_file(&ctx.db, "q\n"));
    for format in ["table", "csv", "tsv"] {
        let text = clean(&run_file(
            &ctx.db,
            &format!("formato {format}\ncount estado = activo\nq\n"),
        ));
        assert_eq!(text.strip_prefix(&banner).unwrap().trim(), "2");
    }
}

#[test]
fn formatos_persisten_sin_confirmacion_y_csv_tsv_se_leen_desde_stdout() {
    let ctx = indexed_ctx();
    let banner = clean(&run_file(&ctx.db, "q\n"));
    let formats = clean(&run_file(
        &ctx.db,
        "formato json\nformato csv\nformato tsv\nformato table\nq\n",
    ));
    assert_eq!(formats, banner, "formato no debe imprimir confirmación");

    for (format, separator) in [("csv", ','), ("tsv", '\t')] {
        let text = clean(&run_file(
            &ctx.db,
            &format!("formato {format}\nciudad = Madrid\nnombre = Antonio\nq\n"),
        ));
        let mut lines = text.lines().skip_while(|line| !line.starts_with("id"));
        let header = lines.next().unwrap();
        assert_eq!(
            header.split(separator).collect::<Vec<_>>(),
            ["id", "key", "type", "value", "offset", "length"]
        );
        // La muestra no requiere interpretar escapes CSV.
        for (id, key, value) in [(2, "c", "Madrid"), (3, "d", "Madrid")] {
            let fields: Vec<_> = lines.next().unwrap().split(separator).collect();
            assert_eq!(fields.len(), 6);
            assert_eq!(fields[0].parse::<u64>().unwrap(), id);
            assert_eq!(fields[1], key);
            assert_eq!(fields[2], "string");
            assert_eq!(fields[3], value);
            assert!(fields[4].parse::<u64>().is_ok());
            assert!(fields[5].parse::<u64>().unwrap() > 0);
        }
        assert_eq!(lines.next().unwrap(), header);
        let fields: Vec<_> = lines.next().unwrap().split(separator).collect();
        assert_eq!(&fields[..4], ["0", "a", "string", "Antonio"]);
        assert!(lines.next().is_none());

        let top = clean(&run_file(
            &ctx.db,
            &format!("formato {format}\ntop estado 1\nq\n"),
        ));
        let mut lines = top.lines().skip_while(|line| !line.starts_with("value"));
        assert_eq!(
            lines.next().unwrap().split(separator).collect::<Vec<_>>(),
            ["value", "type", "count"]
        );
        assert_eq!(
            lines.next().unwrap().split(separator).collect::<Vec<_>>(),
            ["activo", "string", "2"]
        );
        assert!(lines.next().is_none());
    }

    let table = clean(&run_file(&ctx.db, "nombre = Antonio\ntop estado 1\nq\n"));
    assert!(table.contains("nombre = Antonio"));
    assert!(table.contains("resultado(s)"));
    assert!(table.contains("apariciones"));
    assert!(table.contains("activo"));
}

#[test]
fn ajustes_se_conservan_antes_de_abrir_y_al_cambiar_fuente() {
    let first = indexed_ctx();
    let second = indexed_ctx();
    let text = clean(&run_raw(
        &[],
        &format!(
            "formato json\nlimite 1\nabrir \"{}\"\nestado = activo\nabrir \"{}\"\nestado = activo\nn\nq\n",
            first.db.display(),
            second.db.display()
        ),
    ));
    let docs = searches(&text);
    assert_eq!(docs.len(), 3);
    assert_hits(&docs[0], &[0]);
    assert_hits(&docs[1], &[0]);
    assert_hits(&docs[2], &[2]);
}

#[test]
fn no_permite_sobrescribir_fuente_o_indice_activo() {
    let ctx = indexed_ctx();
    let source = fs::read(&ctx.json).unwrap();
    let index = fs::read(&ctx.db).unwrap();
    let out = run_file(
        &ctx.db,
        &format!(
            "formato json\nnombre = Antonio\ne 1 \"{}\"\ne 1 \"{}\"\nindex --force --db \"{}\"\ncount\nq\n",
            ctx.json.display(),
            ctx.db.display(),
            ctx.json.display()
        ),
    );
    let text = shell_error(&out);
    assert_eq!(fs::read(&ctx.json).unwrap(), source);
    assert_eq!(fs::read(&ctx.db).unwrap(), index);
    assert!(
        json_documents(&text)
            .iter()
            .any(|doc| doc == &serde_json::json!({"count": 1}))
    );
}

#[test]
fn enlaces_duros_no_permiten_sobrescribir_fuente_o_indice() {
    let ctx = indexed_ctx();
    let source = fs::read(&ctx.json).unwrap();
    let index = fs::read(&ctx.db).unwrap();
    let source_link = ctx.dir.path().join("source-link.json");
    let index_link = ctx.dir.path().join("index-link.jindex");
    fs::hard_link(&ctx.json, &source_link).unwrap();
    fs::hard_link(&ctx.db, &index_link).unwrap();
    let out = run_file(
        &ctx.db,
        &format!(
            "nombre = Antonio\ne 1 \"{}\"\ne 1 \"{}\"\nindex --force --db \"{}\"\nq\n",
            source_link.display(),
            index_link.display(),
            source_link.display()
        ),
    );
    let _ = shell_error(&out);
    assert_eq!(fs::read(&ctx.json).unwrap(), source);
    assert_eq!(fs::read(&ctx.db).unwrap(), index);
}

#[cfg(windows)]
#[test]
fn rutas_windows_con_backslashes_para_abrir_index_y_exportar() {
    let ctx = ctx();
    let db = ctx.dir.path().join("indice Windows.jindex");
    let export = ctx.dir.path().join("salida Windows.json");
    let native = |path: &std::path::Path| path.to_str().unwrap().replace('/', "\\");
    let text = clean(&run_raw(
        &[],
        &format!(
            "abrir \"{}\"\nindex --db \"{}\"\nopen \"{}\"\nformato json\nnombre = Antonio\ne 1 \"{}\"\nq\n",
            native(&ctx.json),
            native(&db),
            native(&db),
            native(&export)
        ),
    ));
    let docs = searches(&text);
    assert_eq!(docs.len(), 1);
    assert_hits(&docs[0], &[0]);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(export).unwrap()).unwrap()["nombre"],
        "Antonio"
    );
}
