use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use jxcore::{Filter, IndexOptions, Op, SearchQuery};
use serde_json::json;

mod shell;

#[derive(Parser)]
#[command(
    name = "jexplorer",
    version,
    about = "Explora y busca JSON gigantes sin cargarlos en memoria"
)]
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
        /// Indexa todas las propiedades de primer nivel
        #[arg(long, conflicts_with = "select")]
        all: bool,
        /// Añade un índice FTS5 para búsquedas de texto rápidas
        #[arg(long)]
        fts: bool,
        /// Ruta del índice (por defecto `<fichero>.jindex`)
        #[arg(long)]
        db: Option<PathBuf>,
        /// Objetos por transacción
        #[arg(long, default_value_t = 20000)]
        batch: usize,
        /// Rehace el índice desde cero
        #[arg(long)]
        force: bool,
    },
    /// Busca objetos por el valor de una o varias propiedades
    Search {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        path: Option<String>,
        /// eq | ne | contains | prefix | gt | gte | lt | lte | exists | missing | null | regex
        #[arg(long, default_value = "eq")]
        op: String,
        #[arg(long)]
        value: Option<String>,
        /// Búsqueda sin distinguir mayúsculas ni acentos
        #[arg(long)]
        fold: bool,
        /// Filtro adicional "propiedad operador valor" (repetible)
        #[arg(long = "filter", value_name = "PROP OP VALOR")]
        filters: Vec<String>,
        /// Combina los filtros con OR en lugar de AND
        #[arg(long)]
        any: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Continúa la paginación a partir de este id
        #[arg(long)]
        after: Option<i64>,
        /// Solo cuenta los resultados
        #[arg(long)]
        count: bool,
        /// table | json | csv | tsv
        #[arg(long, default_value = "table")]
        format: String,
    },
    /// Muestra un objeto por su id interno o por su clave raíz
    Get {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        id: Option<i64>,
        #[arg(long)]
        key: Option<String>,
        /// Muestra el objeto tal cual está en el fichero original
        #[arg(long)]
        raw: bool,
    },
    /// Exporta un objeto a un fichero
    Export {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        id: Option<i64>,
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Agrupa los valores de una propiedad y muestra los más frecuentes
    #[command(name = "groupby", visible_alias = "group-by")]
    GroupBy {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        path: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// table | json | csv | tsv
        #[arg(long, default_value = "table")]
        format: String,
    },
    /// Comprueba que el índice coincide con el fichero original
    Verify {
        #[arg(long)]
        db: PathBuf,
        /// Objetos a comprobar en profundidad; 0 = todos
        #[arg(long, default_value_t = 1000)]
        sample: usize,
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

enum Selector {
    Id(i64),
    Key(String),
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
            let w = props
                .iter()
                .map(|p| p.name.chars().count())
                .max()
                .unwrap_or(9)
                .max(9);
            println!("{:<w$}  {:>12}  tipos", "propiedad", "apariciones", w = w);
            for p in &props {
                println!(
                    "{:<w$}  {:>12}  {}",
                    p.name,
                    fmt(p.count),
                    p.types.join(", "),
                    w = w
                );
            }
            let names: Vec<String> = props
                .iter()
                .map(|p| format!("--select {}", p.name))
                .collect();
            println!("\nSugerencia:\n  jexplorer index {} --all", file.display());
            println!(
                "  o bien:\n  jexplorer index {} {}",
                file.display(),
                names.join(" ")
            );
        }
        Cmd::Index {
            file,
            select,
            all,
            fts,
            db,
            batch,
            force,
        } => {
            let db = db.unwrap_or_else(|| jxcore::db_path_for(&file));
            eprintln!("Indexando {}\n      → {}", file.display(), db.display());
            let opts = IndexOptions {
                select,
                all,
                fts,
                batch,
                force,
            };
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
            let props = if summary.props.is_empty() {
                "ninguna".to_string()
            } else if summary.props.len() > 20 {
                format!(
                    "{} (+{} más)",
                    summary.props[..20].join(", "),
                    summary.props.len() - 20
                )
            } else {
                summary.props.join(", ")
            };
            println!("Propiedades:        {props}");
            if summary.all {
                println!("Modo:               todas las propiedades");
            }
            if summary.fts {
                println!("FTS:                activado");
            }
            println!("Tiempo:             {:.1} s", summary.elapsed);
            println!("Tamaño del índice:  {}", bytes(summary.db_bytes));
            if summary.skipped > 0 {
                println!(
                    "Aviso:              {} objetos ilegibles omitidos",
                    fmt(summary.skipped)
                );
            }
            if summary.oversized > 0 {
                println!(
                    "Aviso:              {} objetos mayores de 8 MB indexados sin propiedades",
                    fmt(summary.oversized)
                );
            }
            if !summary.missing.is_empty() {
                println!(
                    "Aviso:              la propiedad {} no aparece en ningún objeto",
                    summary.missing.join(", ")
                );
            }
        }
        Cmd::Search {
            db,
            path,
            op,
            value,
            fold,
            filters,
            any,
            limit,
            after,
            count,
            format,
        } => {
            let format = format_mode(&format)?;
            check_limit(limit)?;
            let mut parsed = Vec::new();
            if let Some(path) = path {
                let op = Op::parse(&op)?;
                let value = if op.needs_value() {
                    Some(value.ok_or_else(|| {
                        anyhow::anyhow!("--value es obligatorio para el operador `{}`", op.symbol())
                    })?)
                } else {
                    None
                };
                parsed.push(Filter::new(path, op, value, fold));
            } else if value.is_some() {
                bail!("--value requiere --path; también puedes usar --filter");
            }
            for raw in filters {
                parsed.push(parse_filter(&raw, fold)?);
            }
            if parsed.is_empty() {
                bail!("indica --path/--op/--value o al menos un --filter");
            }

            let wanted: Vec<String> = parsed.iter().map(|f| f.path.clone()).collect();
            let query = SearchQuery {
                filters: parsed,
                any,
                limit,
                after,
            };
            let (conn, freshness) = jxcore::open_checked(&db)?;
            note_freshness(freshness);
            require_paths(&jxcore::stats(&conn)?, &wanted)?;

            if count {
                let n = jxcore::count(&conn, &query)?;
                if format == "json" {
                    println!("{}", serde_json::to_string_pretty(&json!({ "count": n }))?);
                } else {
                    println!("{n}");
                }
                return Ok(());
            }

            let t = Instant::now();
            let hits = jxcore::search(&conn, &query)?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            match format {
                "json" => print_hits_json(&hits, ms, limit),
                "csv" => print_hits_delimited(&hits, ','),
                "tsv" => print_hits_delimited(&hits, '\t'),
                _ => print_hits_table(&hits, &query.filters, ms, limit),
            }
        }
        Cmd::Get { db, id, key, raw } => {
            let selector = selector(id, key)?;
            let (conn, freshness) = jxcore::open_checked(&db)?;
            note_freshness(freshness);
            let json = jxcore::index_json_path(&conn)?;
            let (off, len) = span_for(&conn, &selector)?;
            let bytes = jxcore::read_object(&json, off, len)?;
            if raw {
                println!("{}", String::from_utf8_lossy(&bytes));
            } else {
                println!("{}", jxcore::pretty(&bytes)?);
            }
        }
        Cmd::Export { db, id, key, out } => {
            let selector = selector(id, key)?;
            let (conn, freshness) = jxcore::open_checked(&db)?;
            note_freshness(freshness);
            let json = jxcore::index_json_path(&conn)?;
            let (off, len) = span_for(&conn, &selector)?;
            let bytes = jxcore::read_object(&json, off, len)?;
            std::fs::write(&out, &bytes)?;
            println!(
                "Exportado {} → {}  ({} B)",
                selector.label(),
                out.display(),
                bytes.len()
            );
        }
        Cmd::GroupBy {
            db,
            path,
            limit,
            format,
        } => {
            let format = format_mode(&format)?;
            check_limit(limit)?;
            let (conn, freshness) = jxcore::open_checked(&db)?;
            note_freshness(freshness);
            require_paths(&jxcore::stats(&conn)?, &[path.as_str()])?;
            let groups = jxcore::group_by(&conn, &path, limit)?;
            if groups.is_empty() {
                println!("sin valores indexados para `{path}`");
                return Ok(());
            }
            match format {
                "json" => {
                    let rows: Vec<_> = groups
                        .iter()
                        .map(|g| {
                            json!({
                                "value": g.value,
                                "type": jxcore::type_name(g.kind),
                                "count": g.count,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                }
                "csv" | "tsv" => {
                    let delim = if format == "csv" { ',' } else { '\t' };
                    println!("value{delim}type{delim}count");
                    for g in &groups {
                        println!(
                            "{}{delim}{}{delim}{}",
                            csv_field(&g.value, delim),
                            jxcore::type_name(g.kind),
                            g.count
                        );
                    }
                }
                _ => {
                    let w = groups
                        .iter()
                        .map(|g| g.value.chars().count())
                        .max()
                        .unwrap_or(12)
                        .clamp(12, 60);
                    println!(
                        "{:<w$}  {:<10}  {:>12}",
                        "valor",
                        "tipo",
                        "apariciones",
                        w = w
                    );
                    for g in &groups {
                        println!(
                            "{:<w$}  {:<10}  {:>12}",
                            trim(&g.value, w),
                            jxcore::type_name(g.kind),
                            fmt(g.count),
                            w = w
                        );
                    }
                }
            }
        }
        Cmd::Verify { db, sample } => {
            let (conn, _) = jxcore::open_checked(&db)?;
            let report = jxcore::verify(&conn, sample)?;
            println!("Objetos comprobados:      {}", fmt(report.objects));
            println!("En profundidad:           {}", fmt(report.checked));
            println!("Propiedades comprobadas:  {}", fmt(report.checked_props));
            if report.oversized > 0 {
                println!("Omitidos (> 8 MB):        {}", fmt(report.oversized));
            }
            println!("Claves incorrectas:       {}", fmt(report.bad_keys));
            println!("Offsets incorrectos:      {}", fmt(report.bad_spans));
            println!("Objetos no parseables:    {}", fmt(report.bad_json));
            println!("Propiedades incorrectas:  {}", fmt(report.bad_props));
            for e in &report.errors {
                println!("  - {e}");
            }
            if !report.is_ok() {
                bail!("la verificación ha encontrado problemas");
            }
            println!("Verificación correcta.");
        }
        Cmd::Stats { db } => {
            let conn = jxcore::open_db_ro(&db)?;
            if let Err(e) = jxcore::check_source(&conn) {
                eprintln!("aviso: {e}");
            }
            let s = jxcore::stats(&conn)?;
            println!("JSON:         {}", s.json_path.display());
            println!("Estado:       {}", s.status);
            println!(
                "Modo:         {}",
                if s.all {
                    "todas las propiedades"
                } else {
                    "propiedades seleccionadas"
                }
            );
            println!("FTS:          {}", if s.fts { "sí" } else { "no" });
            println!("Tamaño JSON:  {}", bytes(s.size));
            println!("Objetos:      {}", fmt(s.objects));
            println!(
                "Índice:       {}",
                bytes(std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0))
            );
            println!("\nPropiedades indexadas:");
            for (p, n) in &s.paths {
                println!("  {:<26} {:>12}", p, fmt(*n));
            }
        }
        Cmd::Shell { db } => shell::run(&db)?,
    }
    Ok(())
}

impl Selector {
    fn label(&self) -> String {
        match self {
            Selector::Id(id) => format!("id {id}"),
            Selector::Key(key) => format!("clave `{key}`"),
        }
    }
}

fn selector(id: Option<i64>, key: Option<String>) -> Result<Selector> {
    match (id, key) {
        (Some(id), None) => Ok(Selector::Id(id)),
        (None, Some(key)) => Ok(Selector::Key(key)),
        (Some(_), Some(_)) => bail!("usa --id o --key, no ambos"),
        (None, None) => bail!("indica --id o --key"),
    }
}

fn span_for(conn: &rusqlite::Connection, selector: &Selector) -> Result<(u64, u64)> {
    match selector {
        Selector::Id(id) => jxcore::object_span(conn, *id),
        Selector::Key(key) => jxcore::object_span_by_key(conn, key),
    }
}

fn parse_filter(raw: &str, fold: bool) -> Result<Filter> {
    let (path, rest) = split_first(raw)
        .ok_or_else(|| anyhow::anyhow!("filtro vacío; usa \"propiedad operador valor\""))?;
    if rest.is_empty() {
        bail!("falta el operador en el filtro `{raw}`");
    }
    let (op_str, value) = split_first(rest).unwrap_or((rest, ""));
    let op = Op::parse(op_str)?;
    let value = if op.needs_value() {
        if value.is_empty() {
            bail!("el operador `{op_str}` necesita un valor en el filtro `{raw}`");
        }
        Some(value.to_string())
    } else {
        None
    };
    Ok(Filter::new(path, op, value, fold))
}

fn format_mode(s: &str) -> Result<&'static str> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "table" | "tabla" => "table",
        "json" => "json",
        "csv" => "csv",
        "tsv" => "tsv",
        other => bail!("formato desconocido `{other}`; usa table, json, csv o tsv"),
    })
}

fn check_limit(n: usize) -> Result<()> {
    if !(1..=10_000).contains(&n) {
        bail!("el límite debe estar entre 1 y 10000");
    }
    Ok(())
}

fn require_paths(info: &jxcore::Stats, wanted: &[impl AsRef<str>]) -> Result<()> {
    let available: Vec<&str> = info.paths.iter().map(|(p, _)| p.as_str()).collect();
    for path in wanted {
        let path = path.as_ref();
        if !available.contains(&path) {
            let lista = if available.is_empty() {
                "el índice no tiene propiedades".to_string()
            } else {
                format!("disponibles: {}", available.join(", "))
            };
            bail!("la propiedad `{path}` no está indexada; {lista}");
        }
    }
    Ok(())
}

fn display_path(filters: &[Filter]) -> &str {
    filters
        .iter()
        .find(|f| !matches!(f.op, Op::Exists | Op::Missing))
        .map(|f| f.path.as_str())
        .unwrap_or("")
}

fn print_hits_table(hits: &[jxcore::Hit], filters: &[Filter], ms: f64, limit: usize) {
    if hits.is_empty() {
        println!("Sin resultados ({ms:.0} ms)");
        return;
    }
    let path = display_path(filters);
    for h in hits {
        let value = if h.value.is_empty() {
            "—".to_string()
        } else {
            trim(&h.value, 60)
        };
        println!(
            "{:>8}  {:<26}  {} = {}   [{} · {} B]",
            h.id,
            trim(&h.root_key, 26),
            path,
            value,
            h.offset,
            h.length
        );
    }
    let next = if hits.len() == limit {
        format!("   (siguiente página: --after {})", hits.last().unwrap().id)
    } else {
        String::new()
    };
    println!(
        "\n{} resultado(s) en {ms:.0} ms{next}",
        fmt(hits.len() as u64)
    );
}

fn print_hits_json(hits: &[jxcore::Hit], ms: f64, limit: usize) {
    let next = if hits.len() == limit {
        hits.last().map(|h| h.id)
    } else {
        None
    };
    let rows: Vec<_> = hits
        .iter()
        .map(|h| {
            json!({
                "id": h.id,
                "key": h.root_key,
                "type": h.kind_name(),
                "value": h.value,
                "offset": h.offset,
                "length": h.length,
            })
        })
        .collect();
    let doc = json!({
        "count": hits.len(),
        "ms": ms,
        "next_after": next,
        "hits": rows,
    });
    match serde_json::to_string_pretty(&doc) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error al serializar: {e}"),
    }
}

fn print_hits_delimited(hits: &[jxcore::Hit], delim: char) {
    println!("id{delim}key{delim}type{delim}value{delim}offset{delim}length");
    for h in hits {
        println!(
            "{}{delim}{}{delim}{}{delim}{}{delim}{}{delim}{}",
            h.id,
            csv_field(&h.root_key, delim),
            h.kind_name(),
            csv_field(&h.value, delim),
            h.offset,
            h.length
        );
    }
}

fn csv_field(s: &str, delim: char) -> String {
    if s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn split_first(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim()),
        None => (s, ""),
    })
}

fn note_freshness(f: jxcore::Freshness) {
    if f != jxcore::Freshness::Ok {
        eprintln!(
            "aviso: la fecha de modificación del JSON no coincide con la del índice; el contenido sí parece intacto."
        );
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
