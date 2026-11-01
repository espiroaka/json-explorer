use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use jxcore::{Filter, Freshness, Index, IndexOptions, Op, Phase, SearchQuery};
use tempfile::TempDir;

const DATA: &str = r#"{
  "a": {"nombre":"Antonio","edad":42,"estado":"activo","activo":true,"dato":null,"ciudad":"Toledo"},
  "b": {"nombre":"Antón","edad":7,"estado":"baja","activo":false,"ciudad":"Cuenca"},
  "c": {"nombre":"MARÍA","edad":30,"estado":"activo","activo":true,"ciudad":"Madrid"},
  "d": {"nombre":"José","edad":100,"ciudad":"Madrid","dato":"x"}
}"#;

fn options(all: bool, fts: bool, batch: usize, jobs: usize) -> IndexOptions {
    IndexOptions {
        select: if all {
            Vec::new()
        } else {
            ["nombre", "edad", "estado", "activo", "dato", "ciudad"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        },
        all,
        fts,
        batch,
        jobs,
        force: false,
    }
}

fn build_json(dir: &Path, name: &str, data: &str, opts: &IndexOptions) -> (PathBuf, PathBuf) {
    let json = dir.join(name);
    fs::write(&json, data).unwrap();
    let db = dir.join(format!("{name}.indice"));
    jxcore::build_index(&json, &db, opts, |_| {}).unwrap();
    (json, db)
}

fn setup_with(all: bool, fts: bool) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let (_, db) = build_json(dir.path(), "data.json", DATA, &options(all, fts, 2, 2));
    (dir, db)
}

fn setup() -> (TempDir, PathBuf) {
    setup_with(false, true)
}

fn open(db: &Path) -> Index {
    Index::open(db).unwrap()
}

fn ids(index: &Index, filters: Vec<Filter>, any: bool) -> Vec<i64> {
    let q = SearchQuery {
        filters,
        any,
        limit: 100,
        after: None,
    };
    jxcore::search(index, &q)
        .unwrap()
        .iter()
        .map(|h| h.id)
        .collect()
}

fn one(index: &Index, path: &str, op: Op, value: Option<&str>, fold: bool) -> Vec<i64> {
    ids(
        index,
        vec![Filter::new(path, op, value.map(str::to_string), fold)],
        false,
    )
}

fn val(s: &str) -> Option<&str> {
    Some(s)
}

fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn shard_paths(index: &Index) -> Vec<PathBuf> {
    index
        .manifest
        .shards
        .iter()
        .map(|shard| index.dir.join(&shard.file))
        .collect()
}

fn path_counts(index: &Index) -> HashMap<String, u64> {
    let mut counts: HashMap<String, u64> = HashMap::new();
    for conn in index.shards() {
        let mut stmt = conn
            .prepare(
                "SELECT pa.path, count(*) FROM props p JOIN paths pa ON pa.id=p.path_id GROUP BY pa.path",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })
            .unwrap();
        for row in rows {
            let (path, count) = row.unwrap();
            *counts.entry(path).or_insert(0) += count;
        }
    }
    counts
}

#[test]
fn incluye_el_id_cero_y_pagina() {
    let (_dir, db) = setup();
    let index = open(&db);

    assert_eq!(
        one(&index, "nombre", Op::Eq, val("Antonio"), false),
        vec![0]
    );
    assert_eq!(
        one(&index, "estado", Op::Eq, val("activo"), false),
        vec![0, 2]
    );

    let q = SearchQuery {
        filters: vec![Filter::new("estado", Op::Eq, val("activo"), false)],
        any: false,
        limit: 100,
        after: Some(0),
    };
    let hits = jxcore::search(&index, &q).unwrap();
    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);
}

#[test]
fn rangos_nulos_existencias_y_regex() {
    let (_dir, db) = setup();
    let index = open(&db);

    assert_eq!(one(&index, "edad", Op::Gt, val("30"), false), vec![0, 3]);
    assert_eq!(
        one(&index, "edad", Op::Gte, val("30"), false),
        vec![0, 2, 3]
    );
    assert_eq!(one(&index, "edad", Op::Lt, val("30"), false), vec![1]);
    assert_eq!(one(&index, "edad", Op::Lte, val("30"), false), vec![1, 2]);
    assert_eq!(one(&index, "dato", Op::Null, None, false), vec![0]);
    assert_eq!(one(&index, "dato", Op::Exists, None, false), vec![0, 3]);
    assert_eq!(one(&index, "dato", Op::Missing, None, false), vec![1, 2]);
    assert_eq!(one(&index, "estado", Op::Ne, val("activo"), false), vec![1]);
    assert_eq!(
        one(&index, "nombre", Op::Regex, val("^Ant"), false),
        vec![0, 1]
    );
    assert_eq!(
        one(&index, "nombre", Op::Contains, val("Ant"), false),
        vec![0, 1]
    );
    assert_eq!(
        one(&index, "nombre", Op::Contains, val("ant"), false),
        Vec::<i64>::new()
    );
}

#[test]
fn fold_y_fts() {
    let (_dir, db) = setup();
    let index = open(&db);

    assert_eq!(one(&index, "nombre", Op::Eq, val("anton"), true), vec![1]);
    assert_eq!(one(&index, "nombre", Op::Eq, val("maria"), true), vec![2]);
    assert_eq!(
        one(&index, "nombre", Op::Eq, val("maria"), false),
        Vec::<i64>::new()
    );
    assert_eq!(
        one(&index, "nombre", Op::Prefix, val("ant"), true),
        vec![0, 1]
    );
    assert_eq!(
        one(&index, "nombre", Op::Contains, val("ari"), true),
        vec![2]
    );
    assert_eq!(
        one(&index, "ciudad", Op::Contains, val("tole"), true),
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
    let (_, db) = build_json(dir.path(), "grams.json", GRAMS, &options(true, true, 2, 2));
    let index = open(&db);

    assert_eq!(
        one(&index, "nombre", Op::Contains, val("maria"), true),
        vec![1]
    );
    assert_eq!(
        one(&index, "nombre", Op::Contains, val("mario"), true),
        vec![0]
    );
}

#[test]
fn el_indice_nuevo_usa_fts_contentless() {
    let (_dir, db) = setup();
    let index = open(&db);
    for conn in index.shards() {
        let mut stmt = conn.prepare("PRAGMA table_info(fts)").unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(names, ["text"]);
    }
}

#[test]
fn cada_fragmento_es_un_unico_fichero_sin_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(true, true, 2, 2);
    let (json, db) = build_json(dir.path(), "unico.json", DATA, &opts);
    let sidecars = |shard: &Path| -> Vec<PathBuf> {
        ["-wal", "-shm", "-journal"]
            .iter()
            .map(|suffix| PathBuf::from(format!("{}{suffix}", shard.display())))
            .collect()
    };
    let check = |db: &Path| {
        let manifest = jxcore::read_manifest(db).unwrap();
        assert_eq!(manifest.shards.len(), 2);
        for shard in &manifest.shards {
            let path = db.join(&shard.file);
            assert!(path.is_file());
            for sidecar in sidecars(&path) {
                assert!(
                    !sidecar.exists(),
                    "no debería existir {}",
                    sidecar.display()
                );
            }
        }
        let index = open(db);
        for conn in index.shards() {
            let mode: String = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "delete");
        }
    };

    check(&db);

    // Un index rechazado (ya indexado, sin --force) no debe dejar ficheros extra.
    assert!(jxcore::build_index(&json, &db, &opts, |_| {}).is_err());
    check(&db);

    opts.force = true;
    jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
    check(&db);
}

#[test]
fn eq_y_prefix_cubren_texto_numeros_booleanos_y_nulos() {
    let (_dir, db) = setup_with(true, false);
    let index = open(&db);

    assert_eq!(
        one(&index, "estado", Op::Eq, val("ACTIVO"), true),
        vec![0, 2]
    );
    assert_eq!(
        one(&index, "estado", Op::Eq, val("ACTIVO"), false),
        Vec::<i64>::new()
    );
    assert_eq!(one(&index, "nombre", Op::Eq, val("ANTÓN"), true), vec![1]);
    assert_eq!(
        one(&index, "nombre", Op::Eq, val("antón"), false),
        Vec::<i64>::new()
    );

    assert_eq!(one(&index, "edad", Op::Eq, val("42"), true), vec![0]);
    assert_eq!(
        one(&index, "edad", Op::Eq, val("42.0"), true),
        Vec::<i64>::new()
    );
    assert_eq!(one(&index, "activo", Op::Eq, val("TRUE"), true), vec![0, 2]);
    assert_eq!(
        one(&index, "activo", Op::Eq, val("TRUE"), false),
        Vec::<i64>::new()
    );
    assert_eq!(one(&index, "dato", Op::Eq, val("NULL"), true), vec![0]);

    assert_eq!(
        one(&index, "nombre", Op::Prefix, val("antó"), true),
        vec![0, 1]
    );
    assert_eq!(
        one(&index, "nombre", Op::Prefix, val("Ant"), false),
        vec![0, 1]
    );
    assert_eq!(
        one(&index, "nombre", Op::Prefix, val("ant"), false),
        Vec::<i64>::new()
    );
    assert_eq!(
        one(&index, "nombre", Op::Prefix, val("Antó"), false),
        vec![1]
    );
    assert_eq!(one(&index, "edad", Op::Prefix, val("4"), true), vec![0]);
    assert_eq!(one(&index, "edad", Op::Prefix, val("1"), true), vec![3]);
    assert_eq!(
        one(&index, "activo", Op::Prefix, val("tr"), true),
        vec![0, 2]
    );
    assert_eq!(one(&index, "dato", Op::Prefix, val("nu"), true), vec![0]);

    let q = SearchQuery::single(
        "estado",
        Op::Eq,
        Some("activo".to_string()),
        true,
        100,
        None,
    );
    assert_eq!(jxcore::count(&index, &q).unwrap(), 2);
}

#[test]
fn eq_y_prefix_sobre_objetos_y_arrays() {
    const NESTED: &str = r#"{
  "x": {"id": 1, "meta": {"clave": "Valor"}, "tags": ["Rojo", "azul"]},
  "y": {"id": 2, "meta": null, "tags": []}
}"#;
    let dir = tempfile::tempdir().unwrap();
    let (_, db) = build_json(
        dir.path(),
        "nested.json",
        NESTED,
        &options(true, false, 2, 2),
    );
    let index = open(&db);

    assert_eq!(
        one(&index, "meta", Op::Eq, val(r#"{"clave": "Valor"}"#), true),
        vec![0]
    );
    assert_eq!(
        one(&index, "meta", Op::Eq, val(r#"{"clave": "valor"}"#), false),
        Vec::<i64>::new()
    );
    assert_eq!(one(&index, "meta", Op::Eq, val("null"), true), vec![1]);
    assert_eq!(
        one(&index, "tags", Op::Prefix, val(r#"["R"#), true),
        vec![0]
    );
    assert_eq!(
        one(&index, "tags", Op::Prefix, val(r#"["r"#), false),
        Vec::<i64>::new()
    );
    assert_eq!(one(&index, "meta", Op::Exists, None, false), vec![0, 1]);
}

#[test]
fn filtros_and_or_y_count() {
    let (_dir, db) = setup();
    let index = open(&db);

    let and = vec![
        Filter::new("edad", Op::Gt, val("30"), false),
        Filter::new("estado", Op::Eq, val("activo"), false),
    ];
    assert_eq!(ids(&index, and, false), vec![0]);

    let or = vec![
        Filter::new("estado", Op::Eq, val("baja"), false),
        Filter::new("ciudad", Op::Eq, val("Madrid"), false),
    ];
    assert_eq!(ids(&index, or, true), vec![1, 2, 3]);

    let q = SearchQuery {
        filters: vec![Filter::new("estado", Op::Eq, val("activo"), false)],
        any: false,
        limit: 20,
        after: None,
    };
    assert_eq!(jxcore::count(&index, &q).unwrap(), 2);

    let unknown = vec![
        Filter::new("no_existe", Op::Eq, val("x"), false),
        Filter::new("estado", Op::Eq, val("activo"), false),
    ];
    assert_eq!(ids(&index, unknown, true), vec![0, 2]);
}

#[test]
fn and_con_propiedad_ausente_en_un_fragmento() {
    let dir = tempfile::tempdir().unwrap();
    // Con cuatro fragmentos cada objeto queda solo, así que el fragmento de
    // `d` no conoce `estado` y el de `a` no conoce `dato`.
    let (_, db) = build_json(dir.path(), "ausente.json", DATA, &options(true, true, 2, 4));
    let index = open(&db);
    let and = vec![
        Filter::new("estado", Op::Eq, val("activo"), false),
        Filter::new("ciudad", Op::Eq, val("Madrid"), false),
    ];
    assert_eq!(ids(&index, and.clone(), false), vec![2]);
    let q = SearchQuery {
        filters: and,
        any: false,
        limit: 10,
        after: None,
    };
    assert_eq!(jxcore::count(&index, &q).unwrap(), 1);

    let and = vec![
        Filter::new("nombre", Op::Contains, val("ari"), true),
        Filter::new("estado", Op::Eq, val("activo"), false),
        Filter::new("ciudad", Op::Eq, val("Madrid"), false),
    ];
    assert_eq!(ids(&index, and.clone(), false), vec![2]);
    let q = SearchQuery {
        filters: and,
        any: false,
        limit: 10,
        after: None,
    };
    assert_eq!(jxcore::count(&index, &q).unwrap(), 1);
}

#[test]
fn cursor_se_aplica_a_todos_los_filtros_or() {
    let (_dir, db) = setup();
    let index = open(&db);
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
        assert_eq!(jxcore::count(&index, &query).unwrap(), 3);
        let hits = jxcore::search(&index, &query).unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![id]);
        query.after = Some(id);
    }
    assert!(jxcore::search(&index, &query).unwrap().is_empty());
}

#[test]
fn and_multifiltro_con_contains_y_eq() {
    let (_dir, db) = setup();
    let index = open(&db);

    let q = SearchQuery {
        filters: vec![
            Filter::new("ciudad", Op::Contains, val("madrid"), true),
            Filter::new("estado", Op::Eq, val("activo"), false),
        ],
        any: false,
        limit: 10,
        after: None,
    };
    assert_eq!(jxcore::count(&index, &q).unwrap(), 1);
    let hits = jxcore::search(&index, &q).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, 2);
    assert_eq!(hits[0].root_key, "c");
    assert_eq!(hits[0].value, "Madrid");
    assert_eq!(hits[0].kind_name(), "string");

    let and = vec![
        Filter::new("nombre", Op::Contains, val("ant"), true),
        Filter::new("edad", Op::Gt, val("30"), false),
    ];
    assert_eq!(ids(&index, and, false), vec![0]);

    let and = vec![
        Filter::new("nombre", Op::Contains, val("ant"), true),
        Filter::new("ciudad", Op::Contains, val("cuenca"), true),
    ];
    assert_eq!(ids(&index, and, false), vec![1]);

    let and = vec![
        Filter::new("ciudad", Op::Eq, val("Madrid"), false),
        Filter::new("edad", Op::Gte, val("100"), false),
    ];
    assert_eq!(ids(&index, and, false), vec![3]);

    // Contains de menos de tres caracteres: conduce el Eq y el Contains usa instr.
    let and = vec![
        Filter::new("nombre", Op::Contains, val("ón"), true),
        Filter::new("ciudad", Op::Eq, val("Cuenca"), false),
    ];
    assert_eq!(ids(&index, and, false), vec![1]);

    let and = vec![
        Filter::new("ciudad", Op::Contains, val("madrid"), true),
        Filter::new("estado", Op::Eq, val("baja"), false),
    ];
    assert_eq!(ids(&index, and.clone(), false), Vec::<i64>::new());
    let q = SearchQuery {
        filters: and,
        any: false,
        limit: 10,
        after: None,
    };
    assert_eq!(jxcore::count(&index, &q).unwrap(), 0);

    // El cursor pagina por id de objeto con el plan AND.
    let mut q = SearchQuery {
        filters: vec![
            Filter::new("ciudad", Op::Contains, val("madrid"), true),
            Filter::new("edad", Op::Gt, val("7"), false),
        ],
        any: false,
        limit: 1,
        after: None,
    };
    for id in [2, 3] {
        assert_eq!(jxcore::count(&index, &q).unwrap(), 2);
        let hits = jxcore::search(&index, &q).unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![id]);
        q.after = Some(id);
    }
    assert!(jxcore::search(&index, &q).unwrap().is_empty());
}

#[test]
fn stats_refleja_manifiesto_y_fragmentos() {
    let (_dir, db) = setup_with(true, false);
    let index = open(&db);
    let stats = index.stats();
    let expected = path_counts(&index);
    assert_eq!(stats.objects, 4);
    assert_eq!(stats.shards, 2);
    assert_eq!(index.shards().len(), 2);
    assert!(stats.all);
    assert!(!stats.fts);
    assert_eq!(stats.status, "complete");
    assert_eq!(stats.dir, db);
    assert_eq!(stats.json_path, index.json_path());
    assert_eq!(
        stats.paths.iter().cloned().collect::<HashMap<_, _>>(),
        expected
    );
    assert_eq!(expected["nombre"], 4);
    assert_eq!(expected["dato"], 2);

    let manifest = jxcore::read_manifest(&db).unwrap();
    assert_eq!(manifest.path_counts, stats.paths);
    assert_eq!(manifest.objects, 4);
    assert_eq!(manifest.shard_count, 2);
    assert_eq!(manifest.shards.len(), 2);
    let bytes: u64 = shard_paths(&index)
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum();
    assert_eq!(stats.index_bytes, bytes);
}

#[test]
fn groupby_y_acceso_por_clave() {
    let (_dir, db) = setup();
    let index = open(&db);

    let grupos = jxcore::group_by(&index, "estado", 10).unwrap();
    assert_eq!(grupos.len(), 2);
    assert_eq!(grupos[0].value, "activo");
    assert_eq!(grupos[0].count, 2);
    assert_eq!(grupos[1].value, "baja");

    let (off, len) = index.object_span_by_key("b").unwrap();
    assert_eq!(index.object_span(1).unwrap(), (off, len));
    let json = index.json_path();
    let bytes = jxcore::read_object(&json, off, len).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["nombre"], "Antón");
}

#[test]
fn agrupa_dentro_de_las_coincidencias() {
    let (_dir, db) = setup();
    let index = open(&db);
    let query = move |filters: Vec<Filter>, any: bool| SearchQuery {
        filters,
        any,
        limit: 100,
        after: None,
    };
    let tabla = |groups: Vec<jxcore::Group>| -> Vec<(String, u64)> {
        groups.into_iter().map(|g| (g.value, g.count)).collect()
    };

    assert_eq!(
        tabla(jxcore::group_by(&index, "ciudad", 10).unwrap()),
        vec![
            ("Madrid".to_string(), 2),
            ("Cuenca".to_string(), 1),
            ("Toledo".to_string(), 1)
        ]
    );

    let mayores = jxcore::group_matching(
        &index,
        "ciudad",
        &query(
            vec![Filter::new("edad", Op::Gt, Some("20".to_string()), false)],
            false,
        ),
        10,
    )
    .unwrap();
    assert_eq!(
        tabla(mayores),
        vec![("Madrid".to_string(), 2), ("Toledo".to_string(), 1)]
    );

    let mezclada = jxcore::group_matching(
        &index,
        "ciudad",
        &query(
            vec![
                Filter::new("estado", Op::Eq, Some("baja".to_string()), true),
                Filter::new("nombre", Op::Eq, Some("MARÍA".to_string()), false),
            ],
            true,
        ),
        10,
    )
    .unwrap();
    assert_eq!(
        tabla(mezclada),
        vec![("Cuenca".to_string(), 1), ("Madrid".to_string(), 1)]
    );
}

#[test]
fn omite_el_valor_cuando_coincide_con_norm() {
    let (_dir, db) = setup();
    let index = open(&db);

    let mut omitidos = 0i64;
    let mut duplicados = 0i64;
    for conn in index.shards() {
        omitidos += conn
            .query_row("SELECT count(*) FROM props WHERE value IS NULL", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap();
        duplicados += conn
            .query_row(
                "SELECT count(*) FROM props WHERE value IS NOT NULL AND value = norm",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        // "nombre" siempre lleva mayúsculas o acentos: nunca es redundante.
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM props p JOIN paths pa ON pa.id=p.path_id
                 WHERE pa.path='nombre' AND p.value IS NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        // "estado" ya está plegado: su valor nunca se duplica.
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM props p JOIN paths pa ON pa.id=p.path_id
                 WHERE pa.path='estado' AND p.value IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
    assert!(omitidos > 0);
    assert_eq!(duplicados, 0);

    let valores = |filters: Vec<Filter>| -> Vec<String> {
        jxcore::search(
            &index,
            &SearchQuery {
                filters,
                any: false,
                limit: 10,
                after: None,
            },
        )
        .unwrap()
        .iter()
        .map(|hit| hit.value.clone())
        .collect()
    };
    assert_eq!(
        valores(vec![Filter::new(
            "nombre",
            Op::Eq,
            Some("MARÍA".to_string()),
            false
        )]),
        vec!["MARÍA"]
    );
    assert_eq!(
        valores(vec![Filter::new(
            "nombre",
            Op::Eq,
            Some("maria".to_string()),
            true
        )]),
        vec!["MARÍA"]
    );
    let mut numeros = valores(vec![Filter::new(
        "edad",
        Op::Gt,
        Some("20".to_string()),
        false,
    )]);
    numeros.sort();
    assert_eq!(numeros, vec!["100", "30", "42"]);
}

#[test]
fn verify_limpio() {
    let (_dir, db) = setup();
    let index = open(&db);
    let report = index.verify(0).unwrap();
    assert!(report.is_ok(), "{:?}", report.errors);
    assert_eq!(report.objects, 4);
    assert_eq!(report.checked, 4);
    assert!(report.checked_props > 0);
}

#[test]
fn verify_detecta_fragmento_alterado() {
    let (_dir, db) = setup_with(true, false);
    let index = open(&db);
    let shard = db.join(&index.manifest.shards[1].file);
    drop(index);
    {
        let conn = jxcore::open_db(&shard).unwrap();
        conn.execute("UPDATE objects SET root_key = 'zzz' WHERE id = 1", [])
            .unwrap();
    }
    let report = open(&db).verify(0).unwrap();
    assert_eq!(report.objects, 4);
    assert_eq!(report.bad_keys, 1);
    assert_eq!(report.bad_spans, 0);
    assert!(!report.is_ok());
    assert!(report.errors.iter().any(|error| error.contains("zzz")));
}

#[test]
fn indexa_todas_las_propiedades() {
    let (_dir, db) = setup_with(true, false);
    let index = open(&db);
    assert_eq!(
        one(&index, "ciudad", Op::Eq, val("Madrid"), false),
        vec![2, 3]
    );
    let stats = index.stats();
    assert!(stats.all);
    assert!(!stats.fts);
    assert!(stats.paths.iter().any(|(p, _)| p == "nombre"));
    assert!(stats.paths.iter().any(|(p, _)| p == "ciudad"));

    let props = jxcore::discover_props(&index.json_path(), 1000).unwrap();
    assert_eq!(props.len(), 6);
    // Orden: por número de apariciones y, a igualdad, por nombre.
    assert_eq!(props[0].name, "ciudad");
    assert_eq!(props[0].count, 4);
    assert!(!props[0].types.is_empty());
}

#[test]
fn objetos_se_reparten_por_id_modulo() {
    let dir = tempfile::tempdir().unwrap();
    let (_, db) = build_json(
        dir.path(),
        "reparto.json",
        DATA,
        &options(true, false, 2, 3),
    );
    let index = open(&db);
    assert_eq!(index.shards().len(), 3);
    let mut per_shard = Vec::new();
    for conn in index.shards() {
        let mut stmt = conn.prepare("SELECT id FROM objects ORDER BY id").unwrap();
        let ids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        per_shard.push(ids);
    }
    assert_eq!(per_shard, vec![vec![0, 3], vec![1], vec![2]]);
    let zero: i64 = index
        .shard(3)
        .query_row("SELECT count(*) FROM objects WHERE id = 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(zero, 1);
}

#[test]
fn busqueda_ordena_y_pagina_entre_fragmentos() {
    let dir = tempfile::tempdir().unwrap();
    let (_, db) = build_json(dir.path(), "muchos.json", DATA, &options(true, true, 1, 8));
    let index = open(&db);
    assert_eq!(index.shards().len(), 8);
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
        assert_eq!(jxcore::count(&index, &query).unwrap(), 3);
        let hits = jxcore::search(&index, &query).unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![id]);
        query.after = Some(id);
    }
    assert!(jxcore::search(&index, &query).unwrap().is_empty());
    let all = ids(
        &index,
        vec![Filter::new("nombre", Op::Exists, None::<String>, false)],
        false,
    );
    assert_eq!(all, vec![0, 1, 2, 3]);
}

#[test]
fn check_source_detecta_cambios() {
    let (_dir, db) = setup_with(true, false);
    let index = open(&db);
    let json = index.json_path();
    assert_eq!(index.check_source().unwrap(), Freshness::Ok);
    let (_, freshness) = jxcore::Index::open_checked(&db).unwrap();
    assert_eq!(freshness, Freshness::Ok);

    fs::write(&json, format!("{DATA}\n ")).unwrap();
    let error = index.check_source().unwrap_err().to_string();
    assert!(error.contains("ha cambiado"), "{error}");
    assert!(jxcore::Index::open_checked(&db).is_err());

    fs::write(&json, DATA).unwrap();
    assert!(index.check_source().is_ok());
}

#[test]
fn rehacer_indice_incompleto_sin_force() {
    let (dir, db) = setup_with(true, false);
    let json = dir.path().join("data.json");
    let manifest_path = db.join(jxcore::store::MANIFEST);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["status"] = serde_json::json!("building");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    assert!(Index::open(&db).is_err());

    let summary = jxcore::build_index(&json, &db, &options(true, false, 2, 2), |_| {}).unwrap();
    assert_eq!(summary.objects, 4);
    let index = open(&db);
    assert!(index.verify(0).unwrap().is_ok());
    assert_eq!(
        one(&index, "estado", Op::Eq, val("activo"), false),
        vec![0, 2]
    );
}

#[test]
fn un_build_con_opciones_distintas_no_toca_el_indice_existente() {
    let (dir, db) = setup_with(true, true);
    let json = dir.path().join("data.json");
    let before = snapshot(&db);
    let mut other = options(false, true, 2, 2);
    other.select = vec!["nombre".into()];
    let error = match jxcore::build_index(&json, &db, &other, |_| {}) {
        Ok(_) => panic!("el índice existente debía rechazar otras opciones"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("otras opciones"), "{error}");
    assert_eq!(snapshot(&db), before);
    let index = open(&db);
    assert!(index.verify(0).unwrap().is_ok());
    assert_eq!(
        one(&index, "nombre", Op::Eq, val("Antonio"), false),
        vec![0]
    );
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
fn lotes_pequenos_y_fts_conservan_propiedades() {
    let data = r#"{"a":{"primera":1,"nombre":"Árbol","edad":1},"b":{"nombre":"B","edad":2},"c":{"tardia":true,"nombre":"Último","edad":3}}"#;
    for all in [false, true] {
        for fts in [false, true] {
            for batch in [1, 2, 3] {
                let dir = tempfile::tempdir().unwrap();
                let json = dir.path().join("resume.json");
                fs::write(&json, data).unwrap();
                let db = dir.path().join("indice");
                let mut opts = IndexOptions {
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
                    batch,
                    jobs: 2,
                    force: false,
                };
                let summary = jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
                assert_eq!(summary.objects, 3);
                assert_eq!(summary.shards, 2);
                assert_eq!(summary.fts, fts);
                assert_eq!(summary.all, all);
                for shard in &jxcore::read_manifest(&db).unwrap().shards {
                    let path = db.join(&shard.file);
                    for suffix in ["-wal", "-shm", "-journal"] {
                        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
                        assert!(
                            !sidecar.exists(),
                            "no debería existir {}",
                            sidecar.display()
                        );
                    }
                }
                if all {
                    assert_eq!(summary.props, ["edad", "nombre", "primera", "tardia"]);
                    assert!(summary.missing.is_empty());
                } else {
                    assert_eq!(summary.props, opts.select);
                    assert_eq!(summary.missing, ["ausente"]);
                }
                let index = open(&db);
                assert!(index.verify(0).unwrap().is_ok());
                assert_eq!(one(&index, "primera", Op::Exists, None, false), vec![0]);
                assert_eq!(one(&index, "nombre", Op::Eq, val("arbol"), true), vec![0]);
                assert_eq!(
                    one(&index, "nombre", Op::Contains, val("arb"), true),
                    vec![0]
                );
                assert_eq!(one(&index, "tardia", Op::Exists, None, false), vec![2]);
                assert_eq!(
                    path_counts(&index)
                        .iter()
                        .find(|(p, _)| p.as_str() == "nombre")
                        .map(|(_, n)| *n),
                    Some(3)
                );
                if fts {
                    let docs: i64 = index
                        .shards()
                        .iter()
                        .map(|conn| {
                            conn.query_row(
                                "SELECT count(*) FROM fts WHERE fts MATCH ?1",
                                ["\"arb\""],
                                |r| r.get::<_, i64>(0),
                            )
                            .unwrap()
                        })
                        .sum();
                    assert_eq!(docs, 1);
                }
                drop(index);
                // Windows no permite reemplazar un fragmento abierto.
                opts.force = true;
                let rebuilt = jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
                assert_eq!(rebuilt.objects, 3);
                assert!(open(&db).verify(0).unwrap().is_ok());
            }
        }
    }
}

#[test]
fn cada_fragmento_tiene_indices_y_se_rehace_con_force() {
    for data in ["{}", DATA] {
        for batch in [1, 2, 8] {
            let dir = tempfile::tempdir().unwrap();
            let json = dir.path().join("bulk.json");
            fs::write(&json, data).unwrap();
            let db = dir.path().join("indice");
            let mut opts = IndexOptions {
                select: Vec::new(),
                all: true,
                fts: false,
                batch,
                jobs: 2,
                force: false,
            };
            let summary = jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
            assert_eq!(summary.objects, if data == "{}" { 0 } else { 4 });
            {
                let index = open(&db);
                assert_eq!(index.shards().len(), 2);
                for conn in index.shards() {
                    let count: i64 = conn
                        .query_row(
                            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert_eq!(count, 2);
                }
                assert!(index.verify(0).unwrap().is_ok());
            }
            opts.force = true;
            let rebuilt = jxcore::build_index(&json, &db, &opts, |_| {}).unwrap();
            assert_eq!(rebuilt.objects, summary.objects);
            assert_eq!(rebuilt.props, summary.props);
            assert!(open(&db).verify(0).unwrap().is_ok());
        }
    }
}

#[test]
fn fases_de_progreso() {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("fases.json");
    fs::write(&json, DATA).unwrap();
    let db = dir.path().join("indice");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    jxcore::build_index(&json, &db, &options(true, true, 1, 2), move |p| {
        recorder
            .lock()
            .unwrap()
            .push((p.phase, p.objects, p.step, p.steps))
    })
    .unwrap();
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|(phase, objects, _, _)| *phase == Phase::Ingest && *objects == 4)
    );
    assert!(seen.iter().any(|(phase, _, _, _)| *phase == Phase::Fts));
    assert!(
        seen.iter()
            .any(|(phase, _, step, steps)| *phase == Phase::Indexes && *steps == 4 && *step == 4)
    );
}

#[test]
fn opciones_invalidas_no_crean_indice() {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("data.json");
    fs::write(&json, DATA).unwrap();

    let db = dir.path().join("indice");
    let mut opts = options(true, false, 2, 2);
    opts.jobs = 0;
    assert!(jxcore::build_index(&json, &db, &opts, |_| {}).is_ok());
    let db = dir.path().join("indice2");
    opts.jobs = usize::MAX;
    assert!(jxcore::build_index(&json, &db, &opts, |_| {}).is_err());
    assert!(!db.exists());

    let mut opts = options(false, false, 2, 2);
    opts.select.clear();
    assert!(jxcore::build_index(&json, &db, &opts, |_| {}).is_err());
    opts.all = true;
    opts.select = vec!["nombre".into()];
    assert!(jxcore::build_index(&json, &db, &opts, |_| {}).is_err());
    assert!(!db.exists());

    let mut opts = options(true, false, 0, 2);
    opts.batch = 0;
    assert!(jxcore::build_index(&json, &db, &opts, |_| {}).is_err());
    let occupied = dir.path().join("fichero");
    fs::write(&occupied, b"x").unwrap();
    let opts = options(true, false, 2, 2);
    assert!(jxcore::build_index(&json, &occupied, &opts, |_| {}).is_err());
    assert!(!db.exists());
}
