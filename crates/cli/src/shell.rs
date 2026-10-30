use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use jxcore::{Filter, Hit, IndexOptions, Op, Phase, SearchQuery};
use rusqlite::Connection;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use serde_json::json;

use crate::{bytes, fmt, trim};

#[path = "ui.rs"]
mod ui;

const PAGE: usize = 20;
const MAX_VIEW_LINES: usize = 200;
const MAX_RECENT_FILES: i64 = 20;

fn recent_storage_path() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("APPDATA no está disponible")?;
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("state"))
        })
        .context("XDG_STATE_HOME y HOME no están disponibles")?;
    Ok(base.join("jexplorer").join("recientes.sqlite3"))
}

fn recent_files() -> Result<Vec<PathBuf>> {
    recent_files_at(&recent_storage_path()?)
}

fn recent_files_at(storage: &Path) -> Result<Vec<PathBuf>> {
    match std::fs::metadata(storage) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    let conn = Connection::open_with_flags(storage, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT path FROM recent_files ORDER BY opened DESC LIMIT ?1")?;
    let paths = stmt.query_map([MAX_RECENT_FILES], |row| {
        row.get::<_, String>(0).map(PathBuf::from)
    })?;
    Ok(paths.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn record_recent_file_at(storage: &Path, file: &Path) -> Result<()> {
    let canonical = std::fs::canonicalize(file)?;
    let path = canonical.to_str().context("la ruta no es Unicode válido")?;
    if let Some(parent) = storage
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(storage)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS recent_files (
            path TEXT PRIMARY KEY,
            opened INTEGER NOT NULL
        );",
    )?;
    tx.execute(
        "INSERT INTO recent_files (path, opened)
         VALUES (?1, (SELECT COALESCE(MAX(opened), 0) + 1 FROM recent_files))
         ON CONFLICT(path) DO UPDATE SET opened = excluded.opened",
        [path],
    )?;
    tx.execute(
        "DELETE FROM recent_files WHERE path IN (
            SELECT path FROM recent_files ORDER BY opened DESC LIMIT -1 OFFSET ?1
        )",
        [MAX_RECENT_FILES],
    )?;
    tx.commit()?;
    Ok(())
}

fn terminal_text(text: impl AsRef<str>) -> String {
    if io::stdout().is_terminal() {
        ui::clean(text.as_ref())
    } else {
        text.as_ref().to_owned()
    }
}

#[derive(Parser)]
#[command(
    name = "index",
    about = "Indexa el JSON abierto; todas las propiedades por defecto"
)]
struct IndexArgs {
    #[arg(long = "select", value_name = "PROP")]
    select: Vec<String>,
    #[arg(long, conflicts_with = "select")]
    all: bool,
    #[arg(long)]
    fts: bool,
    #[arg(long, default_value_t = 20000)]
    batch: usize,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    force: bool,
}

enum Next {
    Continue,
    Quit,
}

#[derive(Clone, Copy)]
struct OutputOptions {
    limit: usize,
    format: &'static str,
}

struct Shell {
    json: Option<PathBuf>,
    session: Option<Session>,
    output: OutputOptions,
}

struct Session {
    conn: Connection,
    db: PathBuf,
    props: Vec<String>,
    hits: Vec<Hit>,
    last: Option<(Vec<Filter>, bool)>,
}

pub fn run(file: Option<&Path>) -> Result<()> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        return ui::run(file);
    }
    println!(
        "jexplorer {} · shell interactivo",
        env!("CARGO_PKG_VERSION")
    );
    println!("Escribe `abrir <fichero.json o .jindex>`, `ayuda` o `q` para salir.\n");
    let mut shell = Shell {
        json: None,
        session: None,
        output: OutputOptions {
            limit: PAGE,
            format: "table",
        },
    };
    if let Some(file) = file {
        shell.open(file)?;
    }
    let mut editor = if io::stdin().is_terminal() {
        match DefaultEditor::new() {
            Ok(mut e) => {
                if let Some(path) = history_path() {
                    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
                    let _ = e.load_history(&path);
                }
                Some(e)
            }
            Err(_) => None,
        }
    } else {
        None
    };

    loop {
        let Some(line) = read_line(&mut editor, "jx> ")? else {
            break;
        };
        if let Some(e) = editor.as_mut() {
            let _ = e.add_history_entry(line.as_str());
        }
        match shell.exec(&line) {
            Ok(Next::Quit) => break,
            Ok(Next::Continue) => {}
            Err(e) => eprintln!("error: {e:#}"),
        }
    }

    if let (Some(e), Some(path)) = (editor.as_mut(), history_path()) {
        let _ = e.save_history(&path);
    }
    Ok(())
}

fn history_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|d| d.join("jexplorer").join("historial.txt"))
}

fn read_line(editor: &mut Option<DefaultEditor>, prompt: &str) -> Result<Option<String>> {
    match editor {
        Some(e) => match e.readline(prompt) {
            Ok(line) => Ok(Some(line)),
            Err(ReadlineError::Interrupted) => Ok(Some(String::new())),
            Err(ReadlineError::Eof) => Ok(None),
            Err(err) => Err(err.into()),
        },
        None => {
            let mut line = String::new();
            match io::stdin().read_line(&mut line)? {
                0 => Ok(None),
                _ => Ok(Some(line)),
            }
        }
    }
}

impl Shell {
    fn open(&mut self, file: &Path) -> Result<()> {
        self.open_file(file)?;
        if let Err(error) =
            recent_storage_path().and_then(|storage| record_recent_file_at(&storage, file))
        {
            eprintln!(
                "aviso: no se pudo guardar el archivo en abiertos recientemente: {}",
                ui::clean(&format!("{error:#}"))
            );
        }
        Ok(())
    }

    fn open_file(&mut self, file: &Path) -> Result<()> {
        if !file.is_file() {
            bail!("no existe el fichero {}", file.display());
        }
        let mut header = [0u8; 16];
        let read = File::open(file)?.read(&mut header)?;
        let is_index = file
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jindex"))
            || (read == header.len() && &header == b"SQLite format 3\0");
        if is_index {
            let session = Session::open(file)?;
            let source = jxcore::index_json_path(&session.conn)?;
            self.json = Some(source);
            self.session = Some(session);
        } else {
            let db = jxcore::db_path_for(file);
            if db.is_file() {
                match Session::open(&db) {
                    Ok(session) => {
                        self.json = Some(jxcore::index_json_path(&session.conn)?);
                        self.session = Some(session);
                        return Ok(());
                    }
                    Err(e) => {
                        eprintln!("aviso: {}", terminal_text(format!("{e:#}")));
                        println!(
                            "Usa `index` para reanudar o `index --force` para rehacer el índice."
                        );
                    }
                }
            }
            discover(file, 1000)?;
            self.json = Some(file.to_path_buf());
            self.session = None;
            println!(
                "JSON: {}\nUsa `index` para indexar todas las propiedades, o `index --fts` para añadir FTS.",
                terminal_text(file.display().to_string())
            );
        }
        Ok(())
    }

    fn index(&mut self, rest: &str) -> Result<()> {
        let tokens = tokenize(rest)?;
        let args = match IndexArgs::try_parse_from(
            std::iter::once("index").chain(tokens.iter().map(|t| t.text.as_str())),
        ) {
            Ok(args) => args,
            Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => {
                print!("{e}");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if args.batch == 0 {
            bail!("--batch debe ser mayor que 0");
        }
        let source = self
            .json
            .as_ref()
            .context("abre un JSON primero con `abrir <fichero>`")?;
        let db = args.db.unwrap_or_else(|| {
            self.session
                .as_ref()
                .map(|s| s.db.clone())
                .unwrap_or_else(|| jxcore::db_path_for(source))
        });
        let opts = IndexOptions {
            all: args.all || args.select.is_empty(),
            select: args.select,
            fts: args.fts,
            batch: args.batch,
            force: args.force,
        };
        if db == *source || (db.exists() && same_file::is_same_file(&db, source)?) {
            bail!("el índice no puede sobrescribir el JSON original");
        }
        let previous = self.session.take();
        let previous_db = previous.as_ref().map(|s| s.db.clone());
        drop(previous);
        eprintln!(
            "Indexando {}\n      → {}",
            terminal_text(source.display().to_string()),
            terminal_text(db.display().to_string())
        );
        let mut last = Instant::now();
        let result = jxcore::build_index(source, &db, &opts, |p| {
            match p.phase {
                Phase::Ingest => {
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
                }
                Phase::Fts => {
                    if p.step == 0 {
                        eprintln!("\n  Creando índice de texto completo (FTS)…");
                    } else {
                        eprint!("\r  FTS {}/{}  {:.1} s   ", p.step, p.steps, p.elapsed);
                    }
                }
                Phase::Indexes => {
                    if p.step == 0 {
                        eprintln!("\n  Creando índices de búsqueda…");
                    } else {
                        eprint!("\r  Índices {}/{}  {:.1} s   ", p.step, p.steps, p.elapsed);
                    }
                }
            }
            let _ = io::stderr().flush();
        });
        eprintln!();
        let summary = match result {
            Ok(summary) => summary,
            Err(e) => {
                if let Some(previous_db) = previous_db {
                    if let Ok(session) = Session::open(&previous_db) {
                        self.session = Some(session);
                    }
                }
                return Err(e);
            }
        };
        println!("Objetos indexados:  {}", fmt(summary.objects));
        println!(
            "Propiedades:        {}",
            terminal_text(summary.props.join(", "))
        );
        println!("Tiempo:             {:.1} s", summary.elapsed);
        println!("Tamaño del índice:  {}", bytes(summary.db_bytes));
        if summary.skipped > 0 {
            println!("Aviso: {} objetos ilegibles omitidos", fmt(summary.skipped));
        }
        if summary.oversized > 0 {
            println!(
                "Aviso: {} objetos mayores de 8 MB indexados sin propiedades",
                fmt(summary.oversized)
            );
        }
        if !summary.missing.is_empty() {
            println!(
                "Aviso: propiedades sin apariciones: {}",
                terminal_text(summary.missing.join(", "))
            );
        }
        self.session = Some(Session::open(&db)?);
        Ok(())
    }

    fn exec(&mut self, line: &str) -> Result<Next> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(Next::Continue);
        }
        let output = self.output;
        if looks_like_query(line) {
            self.session()?.query(line, output)?;
            return Ok(Next::Continue);
        }
        let (head, rest) = split_first(line).unwrap_or((line, ""));
        match head.to_ascii_lowercase().as_str() {
            "q" | "quit" | "exit" | "salir" => return Ok(Next::Quit),
            "h" | "help" | "ayuda" | "?" => print_help(),
            "abrir" | "open" | "o" => {
                let file = unquote(rest.trim())?;
                if file.is_empty() {
                    bail!("uso: abrir <fichero.json o .jindex>");
                }
                self.open(Path::new(&file))?;
            }
            "index" | "indexar" => self.index(rest)?,
            "props" => {
                if rest.is_empty() && self.session.is_some() {
                    self.session()?.props()?;
                } else {
                    let source = self
                        .json
                        .as_ref()
                        .context("abre un JSON primero con `abrir <fichero>`")?;
                    discover(source, parse_sample(rest)?)?;
                }
            }
            "n" | "next" | "sig" | "siguiente" => self.session()?.next_page(output)?,
            "v" | "ver" => self.session()?.view(parse_pos(rest)?, false)?,
            "r" | "raw" | "crudo" => self.session()?.view(parse_pos(rest)?, true)?,
            "e" | "export" | "exportar" => self.session()?.export(rest)?,
            "g" | "get" | "id" => self.session()?.by_id(parse_id(rest)?)?,
            "k" | "key" | "clave" => self.session()?.by_key(rest)?,
            "top" | "agrupar" | "groupby" | "grupos" => self.session()?.top(rest, output)?,
            "stats" => self.session()?.stats()?,
            "verify" | "verificar" => self.session()?.verify(parse_sample(rest)?)?,
            "count" | "contar" => self.session()?.count(rest, output)?,
            "formato" | "format" => self.output.format = format_mode(rest)?,
            "limite" | "limit" => {
                let limit: usize = rest.parse().context("uso: limite <n>")?;
                check_limit(limit)?;
                self.output.limit = limit;
                if let Some(session) = self.session.as_mut() {
                    session.hits.clear();
                    session.last = None;
                }
            }
            "s" | "search" | "buscar" => self.session()?.query(rest, output)?,
            _ => self.session()?.query(line, output)?,
        }
        Ok(Next::Continue)
    }

    fn session(&mut self) -> Result<&mut Session> {
        self.session.as_mut().context(
            "no hay un índice abierto; usa `abrir <fichero>` y, si es un JSON sin índice, `index`",
        )
    }
}

impl Session {
    fn open(db: &Path) -> Result<Self> {
        let (conn, freshness) = jxcore::open_checked(db)?;
        if freshness != jxcore::Freshness::Ok {
            eprintln!(
                "aviso: la fecha de modificación del JSON no coincide con la del índice; el contenido sigue intacto."
            );
        }
        let info = jxcore::stats(&conn)?;
        if info.status != "complete" {
            bail!(
                "el índice está incompleto; abre el JSON original y ejecuta `index` con las mismas opciones para reanudar"
            );
        }
        let props: Vec<String> = info.paths.iter().map(|(p, _)| p.clone()).collect();
        println!(
            "JSON:  {}",
            terminal_text(info.json_path.display().to_string())
        );
        println!(
            "       {} objetos · índice {} · propiedades: {}",
            fmt(info.objects),
            bytes(std::fs::metadata(db)?.len()),
            if props.is_empty() {
                "ninguna".into()
            } else {
                terminal_text(props.join(", "))
            }
        );
        println!("Escribe una consulta como `nombre = Antonio`, o `ayuda`.\n");
        Ok(Self {
            conn,
            db: db.to_path_buf(),
            props,
            hits: Vec::new(),
            last: None,
        })
    }

    fn require_path(&self, path: &str) -> Result<()> {
        if !self.props.iter().any(|p| p == path) {
            let lista = if self.props.is_empty() {
                "el índice no tiene ninguna propiedad".to_string()
            } else {
                format!("disponibles: {}", self.props.join(", "))
            };
            bail!("la propiedad `{path}` no está indexada; {lista}");
        }
        Ok(())
    }

    fn query(&mut self, rest: &str, output: OutputOptions) -> Result<()> {
        let (filters, any) = parse_query(rest)?;
        self.run_page(filters, any, None, true, output)
    }

    fn run_page(
        &mut self,
        filters: Vec<Filter>,
        any: bool,
        after: Option<i64>,
        fresh: bool,
        output: OutputOptions,
    ) -> Result<()> {
        for f in &filters {
            self.require_path(&f.path)?;
        }
        let query = SearchQuery {
            filters,
            any,
            limit: output.limit,
            after,
        };
        let t = Instant::now();
        let hits = jxcore::search(&self.conn, &query)?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        self.print_hits(&hits, &query.filters, ms, output)?;
        if fresh {
            self.last = Some((query.filters, any));
        }
        self.hits = hits;
        Ok(())
    }

    fn print_hits(
        &self,
        hits: &[Hit],
        filters: &[Filter],
        ms: f64,
        output: OutputOptions,
    ) -> Result<()> {
        match output.format {
            "json" => {
                let rows: Vec<_> = hits
                    .iter()
                    .map(|h| {
                        json!({
                            "id": h.id, "key": h.root_key, "type": h.kind_name(),
                            "value": h.value, "offset": h.offset, "length": h.length,
                        })
                    })
                    .collect();
                let next = if hits.len() == output.limit {
                    hits.last().map(|h| h.id)
                } else {
                    None
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "count": hits.len(), "ms": ms, "next_after": next, "hits": rows })
                    )?
                );
            }
            "csv" | "tsv" => {
                let delim = if output.format == "csv" { ',' } else { '\t' };
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
            _ => {
                if hits.is_empty() {
                    println!("sin resultados ({ms:.0} ms)");
                    return Ok(());
                }
                let label = filters
                    .iter()
                    .find(|f| !matches!(f.op, Op::Exists | Op::Missing))
                    .map(|f| f.path.as_str())
                    .unwrap_or("");
                for (i, h) in hits.iter().enumerate() {
                    let value = if h.value.is_empty() {
                        "—".to_string()
                    } else {
                        trim(&terminal_text(h.value.replace(['\r', '\n'], " ")), 44)
                    };
                    println!(
                        "{:>3}  {:>9}  {:<26} {:<44} [{} · {} B]",
                        i + 1,
                        h.id,
                        trim(&terminal_text(&h.root_key), 26),
                        format!("{} = {value}", terminal_text(label)),
                        h.offset,
                        h.length
                    );
                }
                let hint = if hits.len() == output.limit {
                    "   `n` para más, `ver <n>` para abrir"
                } else {
                    "   `ver <n>` para abrir"
                };
                println!("\n{} resultado(s) en {ms:.0} ms{hint}", hits.len());
            }
        }
        Ok(())
    }

    fn count(&self, rest: &str, output: OutputOptions) -> Result<()> {
        let (filters, any) = if rest.is_empty() {
            self.last
                .clone()
                .context("no hay ninguna búsqueda activa; uso: count <consulta>")?
        } else {
            parse_query(rest)?
        };
        for filter in &filters {
            self.require_path(&filter.path)?;
        }
        let q = SearchQuery {
            filters,
            any,
            limit: output.limit,
            after: None,
        };
        let n = jxcore::count(&self.conn, &q)?;
        if output.format == "json" {
            println!("{}", json!({ "count": n }));
        } else {
            println!("{n}");
        }
        Ok(())
    }

    fn next_page(&mut self, output: OutputOptions) -> Result<()> {
        let Some((filters, any)) = self.last.clone() else {
            println!("no hay ninguna búsqueda activa");
            return Ok(());
        };
        if self.hits.len() < output.limit {
            println!("no hay más resultados");
            return Ok(());
        }
        let after = self.hits.last().map(|h| h.id);
        self.run_page(filters, any, after, false, output)
    }

    fn view(&self, pos: usize, raw: bool) -> Result<()> {
        let hit = self
            .hits
            .get(pos.wrapping_sub(1))
            .ok_or_else(|| anyhow!("no hay un resultado {pos}; haz una búsqueda primero"))?;
        let json = jxcore::index_json_path(&self.conn)?;
        let data = jxcore::read_object(&json, hit.offset as u64, hit.length as u64)?;
        let text = if raw {
            String::from_utf8_lossy(&data).into_owned()
        } else {
            jxcore::pretty(&data)?
        };
        print_capped(&text);
        Ok(())
    }

    fn by_id(&self, id: i64) -> Result<()> {
        let json = jxcore::index_json_path(&self.conn)?;
        let (offset, length) = jxcore::object_span(&self.conn, id)?;
        let data = jxcore::read_object(&json, offset, length)?;
        print_capped(&jxcore::pretty(&data)?);
        Ok(())
    }

    fn by_key(&self, rest: &str) -> Result<()> {
        let key = unquote(rest.trim())?;
        if key.is_empty() {
            bail!("uso: key <clave-raíz>");
        }
        let json = jxcore::index_json_path(&self.conn)?;
        let (offset, length) = jxcore::object_span_by_key(&self.conn, &key)?;
        let data = jxcore::read_object(&json, offset, length)?;
        print_capped(&jxcore::pretty(&data)?);
        Ok(())
    }

    fn export(&self, rest: &str) -> Result<()> {
        let Some((pos, file)) = split_first(rest) else {
            bail!("uso: e <n> <fichero>");
        };
        let file = unquote(file.trim())?;
        if file.is_empty() {
            bail!("falta la ruta del fichero de salida");
        }
        let pos: usize = pos.parse().context("el número de resultado no es válido")?;
        let hit = self
            .hits
            .get(pos.wrapping_sub(1))
            .ok_or_else(|| anyhow!("no hay un resultado {pos}; haz una búsqueda primero"))?;
        let length = self.export_hit(hit, &file)?;
        println!("exportado el resultado {pos} a {file} ({length} B)");
        Ok(())
    }

    fn export_hit(&self, hit: &Hit, file: &str) -> Result<usize> {
        let json = jxcore::index_json_path(&self.conn)?;
        if Path::new(file) == json
            || Path::new(file) == self.db
            || (Path::new(file).exists()
                && (same_file::is_same_file(file, &json)?
                    || same_file::is_same_file(file, &self.db)?))
        {
            bail!("no puedes exportar sobre el JSON original o el índice activo");
        }
        let data = jxcore::read_object(&json, hit.offset as u64, hit.length as u64)?;
        std::fs::write(file, &data)?;
        Ok(data.len())
    }

    fn top(&self, rest: &str, output: OutputOptions) -> Result<()> {
        let tokens = tokenize(rest)?;
        if tokens.is_empty() || tokens.len() > 2 {
            bail!("uso: top <propiedad> [n]");
        }
        let path = &tokens[0].text;
        self.require_path(path)?;
        let limit = match tokens.get(1) {
            Some(token) => token.text.parse().context("el límite no es un número")?,
            None => 10,
        };
        check_limit(limit)?;
        let groups = jxcore::group_by(&self.conn, path, limit)?;
        match output.format {
            "json" => {
                let rows: Vec<_> = groups.iter().map(|g| json!({ "value": g.value, "type": jxcore::type_name(g.kind), "count": g.count })).collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            }
            "csv" | "tsv" => {
                let delim = if output.format == "csv" { ',' } else { '\t' };
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
                println!("{:<w$}  {:<10}  {:>12}", "valor", "tipo", "apariciones");
                for g in &groups {
                    println!(
                        "{:<w$}  {:<10}  {:>12}",
                        trim(&terminal_text(&g.value), w),
                        jxcore::type_name(g.kind),
                        fmt(g.count)
                    );
                }
            }
        }
        Ok(())
    }

    fn props(&self) -> Result<()> {
        let info = jxcore::stats(&self.conn)?;
        if info.paths.is_empty() {
            println!("el índice no tiene propiedades");
        }
        for (p, n) in &info.paths {
            println!("  {:<26} {:>12}", terminal_text(p), fmt(*n));
        }
        Ok(())
    }

    fn stats(&self) -> Result<()> {
        let info = jxcore::stats(&self.conn)?;
        println!(
            "JSON:    {}",
            terminal_text(info.json_path.display().to_string())
        );
        println!("Estado:  {}", info.status);
        println!(
            "Modo:    {}",
            if info.all {
                "todas las propiedades"
            } else {
                "seleccionadas"
            }
        );
        println!("FTS:     {}", if info.fts { "sí" } else { "no" });
        println!(
            "Tamaño:  {} · {} objetos",
            bytes(info.size),
            fmt(info.objects)
        );
        println!("Índice:  {}", bytes(std::fs::metadata(&self.db)?.len()));
        Ok(())
    }

    fn verify(&self, sample: usize) -> Result<()> {
        jxcore::check_source(&self.conn)?;
        let report = jxcore::verify(&self.conn, sample)?;
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
            println!("  - {}", terminal_text(e));
        }
        if !report.is_ok() {
            bail!("la verificación ha encontrado problemas");
        }
        println!("Verificación correcta.");
        Ok(())
    }
}

fn discover(file: &Path, sample: usize) -> Result<()> {
    let props = jxcore::discover_props(file, sample)?;
    if props.is_empty() {
        println!("No he encontrado propiedades en los objetos muestreados.");
        return Ok(());
    }
    let w = props
        .iter()
        .map(|p| p.name.chars().count())
        .max()
        .unwrap_or(9)
        .max(9);
    println!("{:<w$}  {:>12}  tipos", "propiedad", "apariciones");
    for p in props {
        println!(
            "{:<w$}  {:>12}  {}",
            terminal_text(p.name),
            fmt(p.count),
            p.types.join(", ")
        );
    }
    Ok(())
}

fn print_help() {
    println!(
        "\
abrir <fichero>  abrir JSON o índice .jindex (open)
props [n]        propiedades indexadas, o muestra del JSON si aún no hay índice
index            indexar todas las propiedades del JSON abierto
                 --select PROP (repetible), --all, --fts, --batch N, --db RUTA
                 --force para rehacer; `index --help` para ver opciones
consulta         nombre = Antonio       valor exacto
                 nombre =i antonio      sin mayúsculas ni acentos
                 nombre != Antonio      distinto (!=i con plegado)
                 nombre ~ Ant           contiene (~i con plegado)
                 nombre ^ Ant           empieza por (^i con plegado)
                 edad > 40              también >=, < y <=
                 email ? / email !?     existe / ausente
                 email null             es nulo
                 nombre ~r ^Ant         expresión regular
                 edad > 30 & estado = activo
                 ciudad = Madrid | ciudad = Toledo
                 separa & y | con espacios; no los mezcles; entrecomilla valores literales
count [consulta] contar coincidencias (sin consulta, cuenta la última búsqueda)
limite <n>       tamaño de página (1 a 10000; reinicia la búsqueda)
formato <tipo>   table, json, csv o tsv para búsquedas y agrupaciones
n               siguiente página
ver <n>         abrir el resultado n formateado (v)
raw <n>         abrir el resultado n tal cual está en el fichero (r)
id <id>         abrir por id interno
key <clave>     abrir por clave raíz (k)
top <prop> [n]  valores más frecuentes
e <n> <fichero> exportar el resultado n a un fichero
verify [n]      verificar el índice (por defecto 1000; 0 = todos)
stats           datos del índice
ayuda           esta ayuda
q               salir"
    );
}

fn print_capped(text: &str) {
    for (i, line) in text.lines().enumerate() {
        if i == MAX_VIEW_LINES {
            println!(
                "… vista truncada a {MAX_VIEW_LINES} líneas; usa `e <n> fichero.json` para volcarlo entero"
            );
            return;
        }
        println!("{line}");
    }
}

fn looks_like_query(line: &str) -> bool {
    let Some((_, rest)) = split_first(line) else {
        return false;
    };
    let Some((op, _)) = split_first(rest) else {
        return false;
    };
    parse_op_fold(op).is_ok()
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

struct Token {
    text: String,
    quoted: bool,
}

fn tokenize(s: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();
    while chars.peek().is_some() {
        if chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
            continue;
        }
        let mut token = Token {
            text: String::new(),
            quoted: false,
        };
        let mut in_quotes = false;
        while let Some(&c) = chars.peek() {
            if !in_quotes && c.is_whitespace() {
                break;
            }
            chars.next();
            if c == '"' {
                token.quoted = true;
                if in_quotes && chars.peek() == Some(&'"') {
                    token.text.push('"');
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            } else {
                token.text.push(c);
            }
        }
        if in_quotes {
            bail!("faltan comillas de cierre");
        }
        tokens.push(token);
    }
    Ok(tokens)
}

fn parse_query(s: &str) -> Result<(Vec<Filter>, bool)> {
    let tokens = tokenize(s)?;
    let mut filters = Vec::new();
    let mut connector = None;
    let mut start = 0;
    for (i, token) in tokens.iter().enumerate() {
        let is_or = token.text == "|";
        if !token.quoted && (is_or || token.text == "&") {
            if connector.is_some_and(|any| any != is_or) {
                bail!("no mezcles & y | en la misma consulta");
            }
            connector = Some(is_or);
            filters.push(parse_filter(&tokens[start..i])?);
            start = i + 1;
        }
    }
    filters.push(parse_filter(&tokens[start..])?);
    Ok((filters, connector.unwrap_or(false)))
}

fn parse_filter(tokens: &[Token]) -> Result<Filter> {
    if tokens.len() < 2 {
        bail!("uso: <propiedad> <operador> <valor>, por ejemplo `nombre = Antonio`");
    }
    let path = &tokens[0].text;
    let (op, fold) = parse_op_fold(&tokens[1].text)?;
    let value = if op.needs_value() {
        if tokens.len() < 3 {
            bail!("falta el valor a buscar");
        }
        Some(
            tokens[2..]
                .iter()
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        )
    } else {
        if tokens.len() > 2 {
            bail!("el operador `{}` no admite valor", op.symbol());
        }
        None
    };
    Ok(Filter::new(path, op, value, fold))
}

fn parse_op_fold(s: &str) -> Result<(Op, bool)> {
    match s.to_ascii_lowercase().as_str() {
        "=i" | "eqi" => Ok((Op::Eq, true)),
        "!=i" | "nei" => Ok((Op::Ne, true)),
        "~i" | "contains_i" => Ok((Op::Contains, true)),
        "^i" | "prefix_i" => Ok((Op::Prefix, true)),
        _ => Ok((Op::parse(s)?, false)),
    }
}

fn parse_pos(s: &str) -> Result<usize> {
    s.trim()
        .parse()
        .context("uso: ver <n>, donde n es el número de resultado")
}

fn parse_id(s: &str) -> Result<i64> {
    s.trim()
        .parse()
        .context("uso: id <id>, donde id es el identificador interno del objeto")
}

fn parse_sample(s: &str) -> Result<usize> {
    if s.is_empty() {
        Ok(1000)
    } else {
        s.parse()
            .context("la muestra debe ser un número entero no negativo")
    }
}

fn check_limit(n: usize) -> Result<()> {
    if !(1..=10_000).contains(&n) {
        bail!("el límite debe estar entre 1 y 10000");
    }
    Ok(())
}

fn format_mode(s: &str) -> Result<&'static str> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "table" | "tabla" => "table",
        "json" => "json",
        "csv" => "csv",
        "tsv" => "tsv",
        _ => bail!("formato desconocido `{s}`; usa table, json, csv o tsv"),
    })
}

fn csv_field(s: &str, delim: char) -> String {
    let s = terminal_text(s);
    if s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

fn unquote(s: &str) -> Result<String> {
    if s.starts_with('"') {
        let tokens = tokenize(s)?;
        if tokens.len() != 1 {
            bail!("entrecomilla la ruta o el valor completo");
        }
        Ok(tokens.into_iter().next().unwrap().text)
    } else {
        Ok(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recientes_ausentes_no_crean_almacenamiento_y_errores_no_se_ocultan() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("estado").join("recientes.sqlite3");
        assert!(recent_files_at(&storage).unwrap().is_empty());
        assert!(!storage.parent().unwrap().exists());
        assert!(record_recent_file_at(&storage, &dir.path().join("ausente.json")).is_err());
        assert!(!storage.parent().unwrap().exists());
        let invalid = dir.path().join("roto.sqlite3");
        std::fs::write(&invalid, "no es SQLite").unwrap();
        assert!(recent_files_at(&invalid).is_err());
    }

    #[test]
    fn recientes_orden_canonico_deduplicacion_persistencia_y_rutas_desaparecidas() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("estado").join("recientes.sqlite3");
        let files: Vec<_> = [
            "datos con espacios.json",
            "María 東京.json",
            "índice.jindex",
        ]
        .into_iter()
        .map(|name| {
            let file = dir.path().join(name);
            std::fs::write(&file, "{}").unwrap();
            file
        })
        .collect();
        for file in &files {
            record_recent_file_at(&storage, file).unwrap();
        }
        let canonical: Vec<_> = files
            .iter()
            .map(|file| file.canonicalize().unwrap())
            .collect();
        assert_eq!(
            recent_files_at(&storage).unwrap(),
            vec![
                canonical[2].clone(),
                canonical[1].clone(),
                canonical[0].clone()
            ]
        );
        let alias = dir.path().join(".").join(files[1].file_name().unwrap());
        record_recent_file_at(&storage, &alias).unwrap();
        let expected = vec![
            canonical[1].clone(),
            canonical[2].clone(),
            canonical[0].clone(),
        ];
        assert_eq!(recent_files_at(&storage).unwrap(), expected);
        let conn = Connection::open(&storage).unwrap();
        let ordinals: Vec<i64> = conn
            .prepare("SELECT opened FROM recent_files ORDER BY opened DESC")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ordinals, vec![4, 3, 1]);
        drop(conn);
        std::fs::remove_file(&files[1]).unwrap();
        record_recent_file_at(&storage, &files[0]).unwrap();
        assert_eq!(
            recent_files_at(&storage).unwrap(),
            vec![
                canonical[0].clone(),
                canonical[1].clone(),
                canonical[2].clone()
            ]
        );
    }

    #[test]
    fn recientes_limite_veinte_y_ordinals_siguen_creciendo() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("recientes.sqlite3");
        let mut files = Vec::new();
        for i in 0..25 {
            let file = dir.path().join(format!("archivo {i}.json"));
            std::fs::write(&file, "{}").unwrap();
            record_recent_file_at(&storage, &file).unwrap();
            files.push(file.canonicalize().unwrap());
        }
        assert_eq!(
            recent_files_at(&storage).unwrap(),
            files[5..].iter().rev().cloned().collect::<Vec<_>>()
        );
        record_recent_file_at(&storage, &files[0]).unwrap();
        let expected: Vec<_> = std::iter::once(files[0].clone())
            .chain(files[6..].iter().rev().cloned())
            .collect();
        assert_eq!(recent_files_at(&storage).unwrap(), expected);
        let conn = Connection::open(&storage).unwrap();
        assert_eq!(
            conn.query_row("SELECT MAX(opened) FROM recent_files", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            26
        );
    }

    #[test]
    fn recientes_insercion_y_borrado_son_atomicos() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("recientes.sqlite3");
        for i in 0..20 {
            let file = dir.path().join(format!("{i}.json"));
            std::fs::write(&file, "{}").unwrap();
            record_recent_file_at(&storage, &file).unwrap();
        }
        let previous = recent_files_at(&storage).unwrap();
        let conn = Connection::open(&storage).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_delete BEFORE DELETE ON recent_files
             BEGIN SELECT RAISE(ABORT, 'no borrar'); END;",
        )
        .unwrap();
        drop(conn);
        let file = dir.path().join("nuevo.json");
        std::fs::write(&file, "{}").unwrap();
        assert!(record_recent_file_at(&storage, &file).is_err());
        assert_eq!(recent_files_at(&storage).unwrap(), previous);
        let conn = Connection::open(&storage).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM recent_files", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            20
        );
    }

    #[cfg(unix)]
    #[test]
    fn recientes_conservan_backslashes_literales() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("recientes.sqlite3");
        let file = dir.path().join(r"datos\María con espacios.json");
        std::fs::write(&file, "{}").unwrap();
        record_recent_file_at(&storage, &file).unwrap();
        assert_eq!(
            recent_files_at(&storage).unwrap(),
            vec![file.canonicalize().unwrap()]
        );
    }

    #[test]
    fn consultas_combinadas_y_valores_literales() {
        let (filters, any) = parse_query("edad > 30 & nombre =i María").unwrap();
        assert!(!any);
        assert_eq!(filters.len(), 2);
        assert!(filters[1].fold);
        let (filters, any) = parse_query("nombre = \"& |\" | estado null").unwrap();
        assert!(any);
        assert_eq!(filters[0].value.as_deref(), Some("& |"));
        assert_eq!(filters[1].op, Op::Null);
        let (filters, _) = parse_query("nombre = \"\"").unwrap();
        assert_eq!(filters[0].value.as_deref(), Some(""));
        let (filters, any) = parse_query("nombre = AND OR").unwrap();
        assert!(!any);
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].value.as_deref(), Some("AND OR"));
        let (filters, any) = parse_query(r#"nombre ~r "^(A|B)&C$" | edad > 30"#).unwrap();
        assert!(any);
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0].value.as_deref(), Some("^(A|B)&C$"));
    }

    #[test]
    fn rechaza_consultas_incompletas_o_ambiguas() {
        for input in [
            "",
            "edad >",
            "edad > 1 &",
            "edad > 1 |",
            "& edad > 1",
            "| edad > 1",
            "edad > 1 & & edad < 9",
            "edad > 1 | | edad < 9",
            "estado null x",
            "edad > 1 & edad < 9 | nombre = x",
        ] {
            assert!(parse_query(input).is_err(), "{input}");
        }
    }

    #[test]
    fn conserva_rutas_windows_y_comillas() {
        assert_eq!(
            unquote(r#""C:\Datos personales\archivo.json""#).unwrap(),
            r"C:\Datos personales\archivo.json"
        );
        let (filters, _) = parse_query(r#"nombre = "Dijo ""hola""""#).unwrap();
        assert_eq!(filters[0].value.as_deref(), Some("Dijo \"hola\""));
        assert!(tokenize("\"sin cierre").is_err());
    }
}
