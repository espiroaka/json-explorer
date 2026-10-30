use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use jxcore::{IndexOptions, Op, SearchQuery};

mod shell;

#[derive(Parser)]
#[command(name = "jexplorer", version, about = "Explora y busca JSON gigantes sin cargarlos en memoria")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Lista las propiedades de los primeros objetos del fichero
    Props {
        file: PathBuf,
        /// Número de objetos a muestrear
        #[arg(long, default_value_t = 1000)]
        sample: usize,
    },
    /// Construye el índice de un fichero JSON
    Index {
        file: PathBuf,
        /// Propiedad a indexar (repetible)
        #[arg(long = "select", value_name = "PROP")]
        select: Vec<String>,
        /// Ruta del índice (por defecto <fichero>.jindex)
        #[arg(long)]
        db: Option<PathBuf>,
        /// Objetos por transacción
        #[arg(long, default_value_t = 20000)]
        batch: usize,
        /// Rehace el índice desde cero
        #[arg(long)]
        force: bool,
    },
    /// Busca objetos por el valor de una propiedad
    Search {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        path: String,
        /// eq | contains | prefix
        #[arg(long, default_value = "eq")]
        op: String,
        #[arg(long)]
        value: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Continúa la paginación a partir de este id
        #[arg(long)]
        after: Option<i64>,
    },
    /// Muestra un objeto por su id interno
    Get {
        #[arg(long)]
        db: PathBuf,
        id: i64,
        /// Muestra el objeto tal cual está en el fichero original
        #[arg(long)]
        raw: bool,
    },
    /// Exporta un objeto a un fichero
    Export {
        #[arg(long)]
        db: PathBuf,
        id: i64,
        #[arg(long)]
        out: PathBuf,
    },
    /// Estado del índice
    Stats {
        #[arg(long)]
        db: PathBuf,
    },
    /// Sesión interactiva sobre un índice
    Shell {
        #[arg(long)]
        db: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("\nerror: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Props { file, sample } => {
            let props = jxcore::discover_props(&file, sample)?;
            if props.is_empty() {
                println!("No he encontrado objetos dentro de {}", file.display());
                return Ok(());
            }
            let w = props.iter().map(|p| p.name.chars().count()).max().unwrap_or(9).max(9);
            println!("{:<w$}  {:>12}  tipos", "propiedad", "apariciones", w = w);
            for p in &props {
                println!("{:<w$}  {:>12}  {}", p.name, fmt(p.count), p.types.join(", "), w = w);
            }
            let names: Vec<String> = props.iter().map(|p| format!("--select {}", p.name)).collect();
            println!("\nSugerencia:\n  jexplorer index {} {}", file.display(), names.join(" "));
        }
        Cmd::Index { file, select, db, batch, force } => {
            let db = db.unwrap_or_else(|| jxcore::db_path_for(&file));
            eprintln!("Indexando {}\n      → {}", file.display(), db.display());
            let opts = IndexOptions { select, batch, force };
            let mut last = Instant::now();
            let summary = jxcore::build_index(&file, &db, &opts, |p| {
                if last.elapsed() < Duration::from_millis(200) {
                    return;
                }
                last = Instant::now();
                let mb = p.bytes as f64 / (1024.0 * 1024.0);
                let rate = if p.elapsed > 0.0 { mb / p.elapsed } else { 0.0 };
                let pct = if p.total_bytes > 0 {
                    p.bytes as f64 * 100.0 / p.total_bytes as f64
                } else {
                    100.0
                };
                eprint!(
                    "\r  {pct:>5.1}%  {} objetos  {rate:>6.0} MB/s  {}   ",
                    fmt(p.objects),
                    bytes(p.bytes)
                );
                let _ = std::io::stderr().flush();
            })?;
            eprintln!();
            println!("Objetos indexados:  {}", fmt(summary.objects));
            println!("Propiedades:        {}", summary.props.join(", "));
            println!("Tiempo:             {:.1} s", summary.elapsed);
            println!("Tamaño del índice:  {}", bytes(summary.db_bytes));
            if summary.skipped > 0 {
                println!("Aviso:              {} objetos ilegibles omitidos", fmt(summary.skipped));
            }
            if !summary.missing.is_empty() {
                println!(
                    "Aviso:              la propiedad {} no aparece en ningún objeto",
                    summary.missing.join(", ")
                );
            }
        }
        Cmd::Search { db, path, op, value, limit, after } => {
            let (conn, freshness) = jxcore::open_checked(&db)?;
            note_freshness(freshness);
            let q = SearchQuery {
                path: path.clone(),
                op: Op::parse(&op)?,
                value: value.clone(),
                limit,
                after,
            };
            let t = Instant::now();
            let hits = jxcore::search(&conn, &q)?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if hits.is_empty() {
                println!("Sin resultados para «{path} {op} {value}» ({ms:.0} ms)");
                return Ok(());
            }
            for h in &hits {
                println!(
                    "{:>8}  {:<26}  {} = {}   [{offset} · {length} B]",
                    h.id,
                    trim(&h.root_key, 26),
                    path,
                    trim(&h.value, 60),
                    offset = h.offset,
                    length = h.length
                );
            }
            let next = if hits.len() == limit {
                format!("   (siguiente página: --after {})", hits.last().unwrap().id)
            } else {
                String::new()
            };
            println!("\n{} resultado(s) en {ms:.0} ms{next}", fmt(hits.len() as u64));
        }
        Cmd::Get { db, id, raw } => {
            let (conn, freshness) = jxcore::open_checked(&db)?;
            note_freshness(freshness);
            let json = jxcore::index_json_path(&conn)?;
            let (off, len) = jxcore::object_span(&conn, id)?;
            let bytes = jxcore::read_object(&json, off, len)?;
            if raw {
                println!("{}", String::from_utf8_lossy(&bytes));
            } else {
                println!("{}", jxcore::pretty(&bytes)?);
            }
        }
        Cmd::Export { db, id, out } => {
            let (conn, freshness) = jxcore::open_checked(&db)?;
            note_freshness(freshness);
            let json = jxcore::index_json_path(&conn)?;
            let (off, len) = jxcore::object_span(&conn, id)?;
            let bytes = jxcore::read_object(&json, off, len)?;
            std::fs::write(&out, &bytes)?;
            println!("Exportado {} → {}  ({} B)", id, out.display(), bytes.len());
        }
        Cmd::Stats { db } => {
            let conn = jxcore::open_db(&db)?;
            if let Err(e) = jxcore::check_source(&conn) {
                eprintln!("aviso: {e}");
            }
            let s = jxcore::stats(&conn)?;
            println!("JSON:         {}", s.json_path.display());
            println!("Estado:       {}", s.status);
            println!("Tamaño JSON:  {}", bytes(s.size));
            println!("Objetos:      {}", fmt(s.objects));
            println!("Índice:       {}", bytes(std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0)));
            println!("\nPropiedades indexadas:");
            for (p, n) in &s.paths {
                println!("  {:<26} {:>12}", p, fmt(*n));
            }
        }
        Cmd::Shell { db } => shell::run(&db)?,
    }
    Ok(())
}

fn note_freshness(f: jxcore::Freshness) {
    if f != jxcore::Freshness::Ok {
        eprintln!("aviso: la fecha de modificación del JSON no coincide con la del índice; el contenido sí parece intacto.");
    }
}

pub(crate) fn fmt(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.bytes().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c as char);
    }
    out
}

pub(crate) fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 3 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

pub(crate) fn trim(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}
