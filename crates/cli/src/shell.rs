use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use jxcore::{Filter, Hit, Index, IndexOptions, Op, Phase, SearchQuery};
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
    /// Número de fragmentos y de hilos de indexación (0 = automático)
    #[arg(long, default_value_t = 0)]
    jobs: usize,
    /// Carpeta del índice (por defecto, el almacén de la aplicación)
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
    index: Index,
    dir: PathBuf,
    props: Vec<String>,
    hits: Vec<Hit>,
    last: Option<(Vec<Filter>, bool)>,
}

fn local_index_dir(json: &Path) -> PathBuf {
    let mut text = json.as_os_str().to_os_string();
    text.push(".jindex");
    PathBuf::from(text)
}

pub fn run(file: Option<&Path>) -> Result<()> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        return ui::run(file);
    }
    println!(
        "jexplorer {} · shell interactivo",
        env!("CARGO_PKG_VERSION")
    );
    println!("Escribe `abrir <fichero.json o carpeta de índice>`, `ayuda` o `q` para salir.\n");
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
        if file.is_dir() {
            if !jxcore::is_index_dir(file) {
                bail!("la carpeta {} no contiene un índice", file.display());
            }
            let session = Session::open(file)?;
            self.json = Some(session.index.json_path());
            self.session = Some(session);
            return Ok(());
        }
        if !file.is_file() {
            bail!("no existe el fichero {}", file.display());
        }
        let mut header = [0u8; 16];
        let read = File::open(file)?.read(&mut header)?;
        let old_index = file
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jindex"))
            || (read == header.len() && &header == b"SQLite format 3\0");
        if old_index {
            bail!(
                "`{}` es un índice del formato antiguo de un solo fichero y ya no se puede abrir.\nAbre el JSON original y ejecuta `index --force` para crear el índice nuevo.",
                file.display()
            );
        }

        let mut candidates = Vec::new();
        match jxcore::find_for_json(file) {
            Ok(Some(stored)) => candidates.push(stored.dir),
            Ok(None) => {}
            Err(error) => eprintln!("aviso: {}", terminal_text(format!("{error:#}"))),
        }
        let local = local_index_dir(file);
        if jxcore::is_index_dir(&local) {
            candidates.push(local);
        }
        for dir in candidates {
            match Session::open(&dir) {
                Ok(session) => {
                    self.json = Some(session.index.json_path());
                    self.session = Some(session);
                    return Ok(());
                }
                Err(error) => eprintln!("aviso: {}", terminal_text(format!("{error:#}"))),
            }
        }
        discover(file, 1000)?;
        self.json = Some(file.to_path_buf());
        self.session = None;
        println!(
            "JSON: {}\nUsa `index` para indexar todas las propiedades, o `index --fts` para añadir FTS.",
            terminal_text(file.display().to_string())
        );
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
            .clone()
            .context("abre un JSON primero con `abrir <fichero>`")?;
        let dir = match args.db {
            Some(dir) => dir,
            None => match &self.session {
                Some(session) if session.index.json_path() == source => session.dir.clone(),
                _ => jxcore::dir_for_json(&source)?,
            },
        };
        let opts = IndexOptions {
            all: args.all || args.select.is_empty(),
            select: args.select,
            fts: args.fts,
            batch: args.batch,
            jobs: args.jobs,
            force: args.force,
        };
        self.build(&source, &dir, opts)
    }

    fn reindex(&mut self, dir: &Path) -> Result<()> {
        let manifest = jxcore::read_manifest(dir)?;
        let source = PathBuf::from(&manifest.source);
        if !source.is_file() {
            bail!(
                "no encuentro el JSON original en {}; devuélvelo a su sitio o reindexa desde el JSON",
                manifest.source
            );
        }
        let opts = IndexOptions {
            all: manifest.all,
            select: if manifest.all {
                Vec::new()
            } else {
                manifest.props.clone()
            },
            fts: manifest.fts,
            batch: 20_000,
            jobs: 0,
            force: true,
        };
        self.json = Some(source.clone());
        self.build(&source, dir, opts)
    }

    fn build(&mut self, source: &Path, dir: &Path, opts: IndexOptions) -> Result<()> {
        if dir == source || (dir.is_file() && same_file::is_same_file(dir, source)?) {
            bail!("el índice no puede sobrescribir el JSON original");
        }
        let previous = self.session.take();
        let previous_dir = previous.as_ref().map(|s| s.dir.clone());
        drop(previous);
        eprintln!(
            "Indexando {}\n      → {}",
            terminal_text(source.display().to_string()),
            terminal_text(dir.display().to_string())
        );
        let mut last = Instant::now();
        let mut last_phase: Option<Phase> = None;
        let mut announced_fts = false;
        let mut announced_indexes = false;
        let result = jxcore::build_index(source, dir, &opts, move |p| {
            let changed = last_phase != Some(p.phase);
            if changed {
                last_phase = Some(p.phase);
                last = Instant::now();
                match p.phase {
                    Phase::Ingest => {}
                    Phase::Fts => {
                        if !announced_fts {
                            announced_fts = true;
                            eprintln!("\n  Creando índice de texto completo (FTS)…");
                        }
                    }
                    Phase::Indexes => {
                        if !announced_indexes {
                            announced_indexes = true;
                            eprintln!("\n  Creando índices de búsqueda…");
                        }
                    }
                }
            } else if last.elapsed() < Duration::from_millis(200) {
                return;
            } else {
                last = Instant::now();
            }
            match p.phase {
                Phase::Ingest => {
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
                    let pct = if p.total_bytes > 0 {
                        p.objects as f64 * 100.0 / p.total_bytes as f64
                    } else {
                        100.0
                    };
                    eprint!(
                        "\r  FTS {pct:>5.1}%  {} de {} objetos  {:.1} s   ",
                        fmt(p.objects),
                        fmt(p.total_bytes),
                        p.elapsed
                    );
                }
                Phase::Indexes => {
                    eprint!("\r  Índices {}/{}  {:.1} s   ", p.step, p.steps, p.elapsed);
                }
            }
            let _ = io::stderr().flush();
        });
        eprintln!();
        let summary = match result {
            Ok(summary) => summary,
            Err(e) => {
                if let Some(previous_dir) = previous_dir {
                    if let Ok(session) = Session::open(&previous_dir) {
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
        println!("Fragmentos:         {}", summary.shards);
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
        self.session = Some(Session::open(dir)?);
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
            "indices" | "índices" | "guardados" => self.indices(rest)?,
            "borrar" | "eliminar" => self.indices(&format!("borrar {rest}"))?,
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

    fn indices(&mut self, rest: &str) -> Result<()> {
        let rest = rest.trim();
        if rest.is_empty() {
            return self.list_indices();
        }
        let (head, arg) = split_first(rest).unwrap_or((rest, ""));
        match head.to_ascii_lowercase().as_str() {
            "abrir" | "open" => {
                let entry = self.stored_index(parse_index_pos(arg)?)?;
                self.open(&entry.dir)
            }
            "borrar" | "eliminar" | "delete" => {
                let entry = self.stored_index(parse_index_pos(arg)?)?;
                if self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.dir == entry.dir)
                {
                    self.session = None;
                }
                jxcore::remove_index(&entry.dir)?;
                println!(
                    "borrado el índice de {} ({})",
                    terminal_text(entry.manifest.source),
                    entry.dir.display()
                );
                Ok(())
            }
            "rehacer" | "reindexar" => {
                let entry = self.stored_index(parse_index_pos(arg)?)?;
                self.reindex(&entry.dir)
            }
            other if other.parse::<usize>().is_ok() => {
                let entry = self.stored_index(parse_index_pos(rest)?)?;
                self.open(&entry.dir)
            }
            _ => bail!("uso: indices [abrir|borrar|rehacer <n>]"),
        }
    }

    fn list_indices(&self) -> Result<()> {
        let entries = jxcore::list_indices()?;
        if entries.is_empty() {
            println!(
                "no hay índices guardados en {}",
                terminal_text(jxcore::indexes_root().display().to_string())
            );
            return Ok(());
        }
        println!(
            "Índices guardados en {}",
            terminal_text(jxcore::indexes_root().display().to_string())
        );
        for (position, entry) in entries.iter().enumerate() {
            let manifest = &entry.manifest;
            let mode = if manifest.all {
                "todas".to_string()
            } else {
                manifest.props.join(", ")
            };
            println!(
                "{:>2}. {} objetos · {}{}{}{}",
                position + 1,
                fmt(manifest.objects),
                bytes(entry.bytes),
                if manifest.fts { " · FTS" } else { "" },
                if manifest.status == "complete" {
                    ""
                } else {
                    " · incompleto"
                },
                if outdated(manifest) {
                    " · formato antiguo"
                } else {
                    ""
                }
            );
            println!(
                "    {}  [{}]",
                trim(&terminal_text(&manifest.source), 100),
                terminal_text(&mode)
            );
        }
        println!("\nUsa `indices abrir <n>`, `indices borrar <n>` o `indices rehacer <n>`.");
        if entries.iter().any(|entry| outdated(&entry.manifest)) {
            println!(
                "Los de «formato antiguo» no se pueden abrir; rehazlos con `indices rehacer <n>`."
            );
        }
        Ok(())
    }

    fn stored_index(&self, position: usize) -> Result<jxcore::StoredIndex> {
        let entries = jxcore::list_indices()?;
        entries
            .into_iter()
            .nth(position.wrapping_sub(1))
            .with_context(|| format!("no hay un índice guardado con el número {position}"))
    }
}

impl Session {
    fn open(dir: &Path) -> Result<Self> {
        let (index, freshness) = Index::open_checked(dir)?;
        if freshness != jxcore::Freshness::Ok {
            eprintln!(
                "aviso: la fecha de modificación del JSON no coincide con la del índice; el contenido sigue intacto."
            );
        }
        let info = index.stats();
        let props: Vec<String> = info.paths.iter().map(|(p, _)| p.clone()).collect();
        println!(
            "JSON:  {}",
            terminal_text(info.json_path.display().to_string())
        );
        println!(
            "       {} objetos · índice {} · {} fragmentos · propiedades: {}",
            fmt(info.objects),
            bytes(info.index_bytes),
            info.shards,
            if props.is_empty() {
                "ninguna".into()
            } else {
                terminal_text(props.join(", "))
            }
        );
        println!("Escribe una consulta como `nombre = Antonio`, o `ayuda`.\n");
        Ok(Self {
            index,
            dir: dir.to_path_buf(),
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
        let hits = jxcore::search(&self.index, &query)?;
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
        let n = jxcore::count(&self.index, &q)?;
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
        let json = self.index.json_path();
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
        let json = self.index.json_path();
        let (offset, length) = self.index.object_span(id)?;
        let data = jxcore::read_object(&json, offset, length)?;
        print_capped(&jxcore::pretty(&data)?);
        Ok(())
    }

    fn by_key(&self, rest: &str) -> Result<()> {
        let key = unquote(rest.trim())?;
        if key.is_empty() {
            bail!("uso: key <clave-raíz>");
        }
        let json = self.index.json_path();
        let (offset, length) = self.index.object_span_by_key(&key)?;
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
        let json = self.index.json_path();
        let target = Path::new(file);
        if target == json
            || target.starts_with(&self.dir)
            || (target.exists()
                && (same_file::is_same_file(target, &json)?
                    || same_file::is_same_file(target, &self.dir).unwrap_or(false)))
        {
            bail!("no puedes exportar sobre el JSON original o el índice activo");
        }
        let data = jxcore::read_object(&json, hit.offset as u64, hit.length as u64)?;
        std::fs::write(file, &data)?;
        Ok(data.len())
    }

    fn top(&self, rest: &str, output: OutputOptions) -> Result<()> {
        let tokens = tokenize(rest)?;
        if tokens.is_empty() {
            bail!("uso: top <propiedad> [n] [consulta]");
        }
        let path = &tokens[0].text;
        self.require_path(path)?;
        let mut from = 1;
        let limit = match tokens.get(1) {
            Some(token) if !token.quoted && token.text.parse::<usize>().is_ok() => {
                from = 2;
                token.text.parse()?
            }
            _ => 10,
        };
        check_limit(limit)?;
        let query = if tokens.len() > from {
            let (filters, any) = parse_query_from_tokens(&tokens[from..])?;
            for filter in &filters {
                self.require_path(&filter.path)?;
            }
            Some(jxcore::SearchQuery {
                filters,
                any,
                limit: output.limit,
                after: None,
            })
        } else {
            None
        };
        let groups = match &query {
            Some(query) => jxcore::group_matching(&self.index, path, query, limit)?,
            None => jxcore::group_by(&self.index, path, limit)?,
        };
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
                if let Some(query) = &query {
                    println!("{} coincidencias", fmt(jxcore::count(&self.index, query)?));
                }
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
        let info = self.index.stats();
        if info.paths.is_empty() {
            println!("el índice no tiene propiedades");
        }
        for (p, n) in &info.paths {
            println!("  {:<26} {:>12}", terminal_text(p), fmt(*n));
        }
        Ok(())
    }

    fn stats(&self) -> Result<()> {
        let info = self.index.stats();
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
        println!(
            "Índice:  {} · {} fragmentos",
            bytes(info.index_bytes),
            info.shards
        );
        println!("Carpeta: {}", terminal_text(info.dir.display().to_string()));
        Ok(())
    }

    fn verify(&self, sample: usize) -> Result<()> {
        self.index.check_source()?;
        let report = self.index.verify(sample)?;
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
abrir <fichero>  abrir JSON o carpeta de índice (open)
props [n]        propiedades indexadas, o muestra del JSON si aún no hay índice
index            indexar todas las propiedades del JSON abierto
                 --select PROP (repetible), --all, --fts, --batch N, --jobs N
                 --db CARPETA para elegir dónde guardar; --force para rehacer
indices          índices guardados; `indices abrir|borrar|rehacer <n>`
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
                si añades una consulta, cuenta solo en sus coincidencias
e <n> <fichero> exportar el resultado n a un fichero
verify [n]      verificar el índice (por defecto 1000; 0 = todos)
stats           datos del índice
ayuda           esta ayuda
q               salir"
    );
}

/// Índices escritos con una versión del formato anterior al actual: solo sirven para rehacerlos.
fn outdated(manifest: &jxcore::Manifest) -> bool {
    manifest.format != jxcore::FORMAT_VERSION
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
    parse_query_from_tokens(&tokenize(s)?)
}

/// Igual que `parse_query`, pero sobre tokens ya separados: evita recomponer
/// las comillas cuando la consulta es la cola de otro comando.
fn parse_query_from_tokens(tokens: &[Token]) -> Result<(Vec<Filter>, bool)> {
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

fn parse_index_pos(s: &str) -> Result<usize> {
    s.trim()
        .parse()
        .context("uso: <número de índice>, empezando por 1")
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

    /// Reparte los argumentos de `top <propiedad> [n] [consulta]` como lo hace el comando.
    fn divide_top(rest: &str) -> (usize, Vec<String>, bool) {
        let tokens = tokenize(rest).unwrap();
        let mut from = 1;
        let limit = match tokens.get(1) {
            Some(token) if !token.quoted && token.text.parse::<usize>().is_ok() => {
                from = 2;
                token.text.parse().unwrap()
            }
            _ => 10,
        };
        let (filters, any) = if tokens.len() > from {
            parse_query_from_tokens(&tokens[from..]).unwrap()
        } else {
            (Vec::new(), false)
        };
        (
            limit,
            filters.iter().map(jxcore::Filter::describe).collect(),
            any,
        )
    }

    #[test]
    fn top_reconstruye_la_consulta_sin_perder_comillas() {
        assert_eq!(divide_top("provincia"), (10, vec![], false));
        assert_eq!(divide_top("provincia 3"), (3, vec![], false));
        assert_eq!(
            divide_top("provincia 3 sexo = M & nombre ~ \"San Miguel\""),
            (
                3,
                vec!["sexo = M".to_string(), "nombre ~ San Miguel".to_string()],
                false
            )
        );
        // un segundo token que no es número ya forma parte de la consulta
        assert_eq!(
            divide_top("provincia sexo = M | edad > 30"),
            (
                10,
                vec!["sexo = M".to_string(), "edad > 30".to_string()],
                true
            )
        );
        // & y | literales entre comillas no parten la consulta
        assert_eq!(
            divide_top("nombre 2 valor = \"a & b\""),
            (2, vec!["valor = a & b".to_string()], false)
        );
    }
}
