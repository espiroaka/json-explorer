use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const DATA: &str = r#"{
  "a": {"nombre":"Antonio","edad":42,"estado":"activo","activo":true,"dato":null,"ciudad":"Toledo"},
  "b": {"nombre":"Antón","edad":7,"estado":"baja","activo":false,"ciudad":"Cuenca"},
  "c": {"nombre":"MARÍA","edad":30,"estado":"activo","activo":true,"ciudad":"Madrid"},
  "d": {"nombre":"José","edad":100,"ciudad":"Madrid","dato":"x"}
}"#;

struct Ctx {
    _dir: TempDir,
    json: PathBuf,
    db: PathBuf,
}

fn ctx() -> Ctx {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("data.json");
    fs::write(&json, DATA).unwrap();
    let db = PathBuf::from(format!("{}.jindex", json.display()));
    let out = run_raw(&[
        "index",
        json.to_str().unwrap(),
        "--all",
        "--fts",
        "--batch",
        "2",
    ]);
    ok(out);
    Ctx {
        _dir: dir,
        json,
        db,
    }
}

fn run_raw(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jexplorer"))
        .args(args)
        .output()
        .unwrap()
}

fn run(ctx: &Ctx, args: &[&str]) -> Output {
    let mut fixed = Vec::with_capacity(args.len() + 2);
    fixed.push(args[0]);
    fixed.push("--db");
    fixed.push(ctx.db.to_str().unwrap());
    fixed.extend_from_slice(&args[1..]);
    run_raw(&fixed)
}

fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn props_e_indexacion() {
    let ctx = ctx();
    let out = ok(run_raw(&["props", ctx.json.to_str().unwrap()]));
    assert!(out.contains("nombre"));
    assert!(out.contains("--all"));

    let stats = ok(run(&ctx, &["stats"]));
    assert!(stats.contains("FTS:          sí"));
    assert!(stats.contains("todas las propiedades"));
}

#[test]
fn search_json_y_paginacion() {
    let ctx = ctx();
    let out = ok(run(
        &ctx,
        &[
            "search", "--path", "nombre", "--op", "eq", "--value", "Antonio", "--format", "json",
        ],
    ));
    let doc: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["count"], 1);
    assert_eq!(doc["hits"][0]["id"], 0);
    assert_eq!(doc["hits"][0]["type"], "string");

    let out = ok(run(
        &ctx,
        &[
            "search", "--path", "estado", "--op", "eq", "--value", "activo", "--format", "json",
            "--after", "0",
        ],
    ));
    let doc: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["count"], 1);
    assert_eq!(doc["hits"][0]["id"], 2);
}

#[test]
fn filtros_fold_y_count() {
    let ctx = ctx();
    let out = ok(run(
        &ctx,
        &[
            "search",
            "--filter",
            "edad > 30",
            "--filter",
            "estado = activo",
            "--format",
            "json",
        ],
    ));
    let doc: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["count"], 1);
    assert_eq!(doc["hits"][0]["id"], 0);

    let out = ok(run(
        &ctx,
        &[
            "search", "--path", "nombre", "--op", "contains", "--value", "ari", "--fold",
            "--format", "json",
        ],
    ));
    let doc: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["count"], 1);
    assert_eq!(doc["hits"][0]["id"], 2);

    let out = ok(run(
        &ctx,
        &[
            "search", "--path", "estado", "--op", "eq", "--value", "activo", "--count",
        ],
    ));
    assert_eq!(out.trim(), "2");

    let out = ok(run(
        &ctx,
        &[
            "search",
            "--filter",
            "dato null",
            "--count",
            "--format",
            "json",
        ],
    ));
    let doc: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["count"], 1);
}

#[test]
fn groupby_get_y_verify() {
    let ctx = ctx();
    let out = ok(run(
        &ctx,
        &["groupby", "--path", "estado", "--format", "json"],
    ));
    let rows: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(rows[0]["value"], "activo");
    assert_eq!(rows[0]["count"], 2);

    let out = ok(run(&ctx, &["get", "--key", "b", "--raw"]));
    assert!(out.contains("Antón"));

    let out = ok(run(&ctx, &["verify", "--sample", "0"]));
    assert!(out.contains("Verificación correcta."));
}

#[test]
fn propiedad_desconocida_es_un_error() {
    let ctx = ctx();
    let out = run(&ctx, &["search", "--path", "no_existe", "--value", "x"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no está indexada"));
    assert!(err.contains("nombre"));

    let out = run(&ctx, &["groupby", "--path", "no_existe"]);
    assert!(!out.status.success());
}

#[test]
fn salida_csv() {
    let ctx = ctx();
    let out = ok(run(
        &ctx,
        &[
            "search", "--path", "ciudad", "--op", "eq", "--value", "Madrid", "--format", "csv",
        ],
    ));
    let mut lines = out.lines();
    assert_eq!(lines.next().unwrap(), "id,key,type,value,offset,length");
    assert_eq!(lines.count(), 2);
}
