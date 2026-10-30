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
