use std::fs;
use std::path::{Path, PathBuf};

use jxcore::{Filter, IndexOptions, Op, SearchQuery};
use rusqlite::Connection;
use tempfile::TempDir;

const DATA: &str = r#"{
  "a": {"nombre":"Antonio","edad":42,"estado":"activo","activo":true,"dato":null,"ciudad":"Toledo"},
  "b": {"nombre":"Antón","edad":7,"estado":"baja","activo":false,"ciudad":"Cuenca"},
  "c": {"nombre":"MARÍA","edad":30,"estado":"activo","activo":true,"ciudad":"Madrid"},
  "d": {"nombre":"José","edad":100,"ciudad":"Madrid","dato":"x"}
}"#;

fn setup_with(all: bool, fts: bool) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("data.json");
    fs::write(&json, DATA).unwrap();
    let db = jxcore::db_path_for(&json);
    let select = if all {
        Vec::new()
    } else {
        ["nombre", "edad", "estado", "activo", "dato", "ciudad"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    };
    let opts = IndexOptions {
        select,
        all,
        fts,
        batch: 2,
        force: false,
    };
    jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
    (dir, db)
}

fn setup() -> (TempDir, PathBuf) {
    setup_with(false, true)
}

fn open(db: &Path) -> Connection {
    jxcore::open_checked(db).unwrap().0
}

fn ids(conn: &Connection, filters: Vec<Filter>, any: bool) -> Vec<i64> {
    let q = SearchQuery {
        filters,
        any,
        limit: 100,
        after: None,
    };
    jxcore::search(conn, &q)
        .unwrap()
        .iter()
        .map(|h| h.id)
        .collect()
}

fn one(conn: &Connection, path: &str, op: Op, value: Option<&str>, fold: bool) -> Vec<i64> {
    ids(
        conn,
        vec![Filter::new(path, op, value.map(str::to_string), fold)],
        false,
    )
}

fn val(s: &str) -> Option<&str> {
    Some(s)
}

#[test]
fn incluye_el_id_cero_y_pagina() {
    let (_dir, db) = setup();
    let conn = open(&db);

    assert_eq!(one(&conn, "nombre", Op::Eq, val("Antonio"), false), vec![0]);
    assert_eq!(
        one(&conn, "estado", Op::Eq, val("activo"), false),
        vec![0, 2]
    );

    let q = SearchQuery {
        filters: vec![Filter::new("estado", Op::Eq, val("activo"), false)],
        any: false,
        limit: 100,
        after: Some(0),
    };
    let hits = jxcore::search(&conn, &q).unwrap();
    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);
}

#[test]
fn rangos_nulos_existencias_y_regex() {
    let (_dir, db) = setup();
    let conn = open(&db);

    assert_eq!(one(&conn, "edad", Op::Gt, val("30"), false), vec![0, 3]);
    assert_eq!(one(&conn, "edad", Op::Gte, val("30"), false), vec![0, 2, 3]);
    assert_eq!(one(&conn, "edad", Op::Lt, val("30"), false), vec![1]);
    assert_eq!(one(&conn, "edad", Op::Lte, val("30"), false), vec![1, 2]);
    assert_eq!(one(&conn, "dato", Op::Null, None, false), vec![0]);
    assert_eq!(one(&conn, "dato", Op::Exists, None, false), vec![0, 3]);
    assert_eq!(one(&conn, "dato", Op::Missing, None, false), vec![1, 2]);
    assert_eq!(one(&conn, "estado", Op::Ne, val("activo"), false), vec![1]);
    assert_eq!(
        one(&conn, "nombre", Op::Regex, val("^Ant"), false),
        vec![0, 1]
    );
    assert_eq!(
        one(&conn, "nombre", Op::Contains, val("Ant"), false),
        vec![0, 1]
    );
    assert_eq!(
        one(&conn, "nombre", Op::Contains, val("ant"), false),
        Vec::<i64>::new()
    );
}

#[test]
fn fold_y_fts() {
    let (_dir, db) = setup();
    let conn = open(&db);

    assert_eq!(one(&conn, "nombre", Op::Eq, val("anton"), true), vec![1]);
    assert_eq!(one(&conn, "nombre", Op::Eq, val("maria"), true), vec![2]);
    assert_eq!(
        one(&conn, "nombre", Op::Eq, val("maria"), false),
        Vec::<i64>::new()
    );
    assert_eq!(
        one(&conn, "nombre", Op::Prefix, val("ant"), true),
        vec![0, 1]
    );
    assert_eq!(
        one(&conn, "nombre", Op::Contains, val("ari"), true),
        vec![2]
    );
    assert_eq!(
        one(&conn, "ciudad", Op::Contains, val("tole"), true),
        vec![0]
    );
}

#[test]
fn fts_descarta_coincidencias_parciales_de_trigramas() {
    const GRAMS: &str = r#"{
  "x": {"nombre":"Mario Arias"},
  "y": {"nombre":"María"}
}"#;
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("grams.json");
    fs::write(&json, GRAMS).unwrap();
    let db = jxcore::db_path_for(&json);
    let opts = IndexOptions {
        select: Vec::new(),
        all: true,
        fts: true,
        batch: 2,
        force: false,
    };
    jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
    let conn = open(&db);

    assert_eq!(
        one(&conn, "nombre", Op::Contains, val("maria"), true),
        vec![1]
    );
    assert_eq!(
        one(&conn, "nombre", Op::Contains, val("mario"), true),
        vec![0]
    );
}

#[test]
fn eq_y_prefix_cubren_texto_numeros_booleanos_y_nulos() {
    let (_dir, db) = setup_with(true, false);
    let conn = open(&db);

    assert_eq!(
        one(&conn, "estado", Op::Eq, val("ACTIVO"), true),
        vec![0, 2]
    );
    assert_eq!(
        one(&conn, "estado", Op::Eq, val("ACTIVO"), false),
        Vec::<i64>::new()
    );
    assert_eq!(one(&conn, "nombre", Op::Eq, val("ANTÓN"), true), vec![1]);
    assert_eq!(
        one(&conn, "nombre", Op::Eq, val("antón"), false),
        Vec::<i64>::new()
    );

    assert_eq!(one(&conn, "edad", Op::Eq, val("42"), true), vec![0]);
    assert_eq!(
        one(&conn, "edad", Op::Eq, val("42.0"), true),
        Vec::<i64>::new()
    );
    assert_eq!(one(&conn, "activo", Op::Eq, val("TRUE"), true), vec![0, 2]);
    assert_eq!(
        one(&conn, "activo", Op::Eq, val("TRUE"), false),
        Vec::<i64>::new()
    );
    assert_eq!(one(&conn, "dato", Op::Eq, val("NULL"), true), vec![0]);

    assert_eq!(
        one(&conn, "nombre", Op::Prefix, val("antó"), true),
        vec![0, 1]
    );
    assert_eq!(
        one(&conn, "nombre", Op::Prefix, val("Ant"), false),
        vec![0, 1]
    );
    assert_eq!(
        one(&conn, "nombre", Op::Prefix, val("ant"), false),
        Vec::<i64>::new()
    );
    assert_eq!(
        one(&conn, "nombre", Op::Prefix, val("Antó"), false),
        vec![1]
    );
    assert_eq!(one(&conn, "edad", Op::Prefix, val("4"), true), vec![0]);
    assert_eq!(one(&conn, "edad", Op::Prefix, val("1"), true), vec![3]);
    assert_eq!(
        one(&conn, "activo", Op::Prefix, val("tr"), true),
        vec![0, 2]
    );
    assert_eq!(one(&conn, "dato", Op::Prefix, val("nu"), true), vec![0]);

    let q = SearchQuery::single(
        "estado",
        Op::Eq,
        Some("activo".to_string()),
        true,
        100,
        None,
    );
    assert_eq!(jxcore::count(&conn, &q).unwrap(), 2);
}

#[test]
fn eq_y_prefix_sobre_objetos_y_arrays() {
    const NESTED: &str = r#"{
  "x": {"id": 1, "meta": {"clave": "Valor"}, "tags": ["Rojo", "azul"]},
  "y": {"id": 2, "meta": null, "tags": []}
}"#;
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("nested.json");
    fs::write(&json, NESTED).unwrap();
    let db = jxcore::db_path_for(&json);
    let opts = IndexOptions {
        select: Vec::new(),
        all: true,
        fts: false,
        batch: 2,
        force: false,
    };
    jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
    let conn = open(&db);

    assert_eq!(
        one(&conn, "meta", Op::Eq, val(r#"{"clave": "Valor"}"#), true),
        vec![0]
    );
    assert_eq!(
        one(&conn, "meta", Op::Eq, val(r#"{"clave": "valor"}"#), false),
        Vec::<i64>::new()
    );
    assert_eq!(one(&conn, "meta", Op::Eq, val("null"), true), vec![1]);
    assert_eq!(one(&conn, "tags", Op::Prefix, val(r#"["R"#), true), vec![0]);
    assert_eq!(
        one(&conn, "tags", Op::Prefix, val(r#"["r"#), false),
        Vec::<i64>::new()
    );
    assert_eq!(one(&conn, "meta", Op::Exists, None, false), vec![0, 1]);
}

#[test]
fn filtros_and_or_y_count() {
    let (_dir, db) = setup();
    let conn = open(&db);

    let and = vec![
        Filter::new("edad", Op::Gt, val("30"), false),
        Filter::new("estado", Op::Eq, val("activo"), false),
    ];
    assert_eq!(ids(&conn, and, false), vec![0]);

    let or = vec![
        Filter::new("estado", Op::Eq, val("baja"), false),
        Filter::new("ciudad", Op::Eq, val("Madrid"), false),
    ];
    assert_eq!(ids(&conn, or, true), vec![1, 2, 3]);

    let q = SearchQuery {
        filters: vec![Filter::new("estado", Op::Eq, val("activo"), false)],
        any: false,
        limit: 20,
        after: None,
    };
    assert_eq!(jxcore::count(&conn, &q).unwrap(), 2);

    let unknown = vec![
        Filter::new("no_existe", Op::Eq, val("x"), false),
        Filter::new("estado", Op::Eq, val("activo"), false),
    ];
    assert_eq!(ids(&conn, unknown, true), vec![0, 2]);
}

#[test]
fn cursor_se_aplica_a_todos_los_filtros_or() {
    let (_dir, db) = setup();
    let conn = open(&db);
    let mut query = SearchQuery {
        filters: vec![
            Filter::new("estado", Op::Eq, val("activo"), false),
            Filter::new("ciudad", Op::Eq, val("Madrid"), false),
        ],
        any: true,
        limit: 1,
        after: None,
    };
    for id in [0, 2, 3] {
        assert_eq!(jxcore::count(&conn, &query).unwrap(), 3);
        let hits = jxcore::search(&conn, &query).unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![id]);
        query.after = Some(id);
    }
    assert!(jxcore::search(&conn, &query).unwrap().is_empty());
}

#[test]
fn groupby_y_acceso_por_clave() {
    let (_dir, db) = setup();
    let conn = open(&db);

    let grupos = jxcore::group_by(&conn, "estado", 10).unwrap();
    assert_eq!(grupos.len(), 2);
    assert_eq!(grupos[0].value, "activo");
    assert_eq!(grupos[0].count, 2);
    assert_eq!(grupos[1].value, "baja");

    let json = jxcore::index_json_path(&conn).unwrap();
    let (off, len) = jxcore::object_span_by_key(&conn, "b").unwrap();
    let bytes = jxcore::read_object(&json, off, len).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["nombre"], "Antón");
}

#[test]
fn verify_limpio() {
    let (_dir, db) = setup();
    let conn = open(&db);
    let report = jxcore::verify(&conn, 0).unwrap();
    assert!(report.is_ok(), "{:?}", report.errors);
    assert_eq!(report.objects, 4);
    assert_eq!(report.checked, 4);
    assert!(report.checked_props > 0);
}

#[test]
fn indexa_todas_las_propiedades() {
    let (_dir, db) = setup_with(true, false);
    let conn = open(&db);
    assert_eq!(
        one(&conn, "ciudad", Op::Eq, val("Madrid"), false),
        vec![2, 3]
    );
    let stats = jxcore::stats(&conn).unwrap();
    assert!(stats.all);
    assert!(!stats.fts);
    assert!(stats.paths.iter().any(|(p, _)| p == "nombre"));
    assert!(stats.paths.iter().any(|(p, _)| p == "ciudad"));
}

#[test]
fn scanner_reanuda_desde_un_offset() {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("resume.json");
    fs::write(&json, DATA).unwrap();

    let mut full = Vec::new();
    {
        let mut scanner = jxcore::parser::RootScanner::open(&json, 0).unwrap();
        let mut key = String::new();
        while let Some(span) = scanner.next_into(&mut key).unwrap() {
            full.push((key.clone(), span.start, span.len));
        }
    }

    let mut first = Vec::new();
    let resume_at;
    {
        let mut scanner = jxcore::parser::RootScanner::open(&json, 0).unwrap();
        let mut key = String::new();
        for _ in 0..2 {
            let span = scanner.next_into(&mut key).unwrap().unwrap();
            first.push((key.clone(), span.start, span.len));
        }
        resume_at = scanner.position();
    }

    let mut second = Vec::new();
    {
        let mut scanner = jxcore::parser::RootScanner::open(&json, resume_at).unwrap();
        let mut key = String::new();
        while let Some(span) = scanner.next_into(&mut key).unwrap() {
            second.push((key.clone(), span.start, span.len));
        }
    }

    first.extend(second);
    assert_eq!(first, full);
}

#[test]
fn indexacion_reanuda_lotes_y_finalizacion_sin_perder_propiedades() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let data = r#"{"a":{"primera":1,"nombre":"Árbol","edad":1},"b":{"nombre":"B","edad":2},"c":{"tardia":true,"nombre":"Último","edad":3}}"#;
    for all in [false, true] {
        for fts in [false, true] {
            for stop_at in [2, 3] {
                let dir = tempfile::tempdir().unwrap();
                let json = dir.path().join("resume.json");
                fs::write(&json, data).unwrap();
                let db = jxcore::db_path_for(&json);
                let opts = IndexOptions {
                    select: if all {
                        Vec::new()
                    } else {
                        vec![
                            "primera".into(),
                            "nombre".into(),
                            "tardia".into(),
                            "ausente".into(),
                        ]
                    },
                    all,
                    fts,
                    batch: 2,
                    force: false,
                };
                let interrupted = catch_unwind(AssertUnwindSafe(|| {
                    jxcore::build_index(&json, &db, &opts, |p| {
                        if p.objects >= stop_at {
                            panic!("interrupcion simulada tras un lote confirmado");
                        }
                    })
                    .unwrap();
                }));
                assert!(interrupted.is_err());
                {
                    let conn = jxcore::open_db(&db).unwrap();
                    let state = jxcore::index::read_state(&conn).unwrap().unwrap();
                    assert_eq!(state.status, "running");
                    assert_eq!(state.objects, stop_at);
                    assert_eq!(state.next_id, stop_at as i64);
                    assert_eq!(
                        state.resume_offset,
                        jxcore::object_span(&conn, stop_at as i64 - 1)
                            .map(|(offset, length)| offset + length)
                            .unwrap()
                    );
                }
                let summary = jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
                assert_eq!(summary.objects, 3);
                if all {
                    assert_eq!(summary.props, ["edad", "nombre", "primera", "tardia"]);
                    assert!(summary.missing.is_empty());
                } else {
                    assert_eq!(summary.props, opts.select);
                    assert_eq!(summary.missing, ["ausente"]);
                }
                let conn = open(&db);
                assert!(jxcore::verify(&conn, 0).unwrap().is_ok());
                assert_eq!(one(&conn, "primera", Op::Exists, None, false), vec![0]);
                assert_eq!(one(&conn, "nombre", Op::Eq, val("arbol"), true), vec![0]);
                assert_eq!(
                    one(&conn, "nombre", Op::Contains, val("arb"), true),
                    vec![0]
                );
                assert_eq!(one(&conn, "tardia", Op::Exists, None, false), vec![2]);
                if fts {
                    let count: i64 = conn
                        .query_row("SELECT count(*) FROM fts", [], |r| r.get(0))
                        .unwrap();
                    assert_eq!(count, 3);
                }
            }
        }
    }
}

#[test]
fn indices_secundarios_se_crean_al_final_incluso_con_lotes_exactos_y_json_vacio() {
    for data in ["{}", DATA] {
        for batch in [1, 2, 3, 8] {
            let dir = tempfile::tempdir().unwrap();
            let json = dir.path().join("bulk.json");
            fs::write(&json, data).unwrap();
            let db = jxcore::db_path_for(&json);
            let mut opts = IndexOptions {
                select: Vec::new(),
                all: true,
                fts: false,
                batch,
                force: false,
            };
            let summary = jxcore::build_index(&json, &db, &opts, |p| {
                if p.phase != jxcore::Phase::Ingest {
                    return;
                }
                let conn = jxcore::open_db_ro(&db).unwrap();
                let count: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(count, 0);
            })
            .unwrap();
            assert_eq!(summary.objects, if data == "{}" { 0 } else { 4 });
            {
                let conn = open(&db);
                let count: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(count, 2);
                assert!(jxcore::verify(&conn, 0).unwrap().is_ok());
            }
            opts.force = true;
            let rebuilt = jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
            assert_eq!(rebuilt.objects, summary.objects);
            assert_eq!(rebuilt.props, summary.props);
            assert!(jxcore::verify(&open(&db), 0).unwrap().is_ok());
        }
    }
}

#[test]
fn fallo_al_crear_indices_no_publica_complete_y_permite_reanudar() {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("finalize.json");
    fs::write(&json, DATA).unwrap();
    let db = jxcore::db_path_for(&json);
    let opts = IndexOptions {
        select: Vec::new(),
        all: true,
        fts: true,
        batch: 2,
        force: false,
    };
    let mut injected = false;
    let result = jxcore::build_index(&json, &db, &opts, |p| {
        if p.objects == 4 && !injected {
            let conn = jxcore::open_db(&db).unwrap();
            conn.execute_batch("CREATE TABLE idx_props_search(dummy INTEGER);")
                .unwrap();
            injected = true;
        }
    });
    assert!(result.is_err());
    {
        let conn = jxcore::open_db(&db).unwrap();
        let state = jxcore::index::read_state(&conn).unwrap().unwrap();
        assert_eq!(state.status, "running");
        assert_eq!(state.objects, 4);
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        conn.execute_batch("DROP TABLE idx_props_search;").unwrap();
        conn.execute(
            "UPDATE meta SET value=?1 WHERE key='resume_offset'",
            [DATA.len().to_string()],
        )
        .unwrap();
    }
    let summary = jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
    assert_eq!(summary.objects, 4);
    assert_eq!(summary.props.len(), 6);
    let conn = open(&db);
    assert!(jxcore::verify(&conn, 0).unwrap().is_ok());
    assert_eq!(
        one(&conn, "nombre", Op::Contains, val("ari"), true),
        vec![2]
    );
}
