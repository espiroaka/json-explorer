use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};

use crate::parser::{CHUNK, MemberIdx, RootScanner, object_members};
use crate::text::fold;

pub const FORMAT_VERSION: i64 = 4;
pub const MAX_STORED_NESTED: usize = 4096;

pub const T_STRING: i64 = 0;
pub const T_NUMBER: i64 = 1;
pub const T_BOOL: i64 = 2;
pub const T_NULL: i64 = 3;
pub const T_OBJECT: i64 = 4;
pub const T_ARRAY: i64 = 5;

pub fn type_name(t: i64) -> &'static str {
    match t {
        T_STRING => "string",
        T_NUMBER => "number",
        T_BOOL => "bool",
        T_NULL => "null",
        T_OBJECT => "object",
        T_ARRAY => "array",
        _ => "?",
    }
}

pub fn db_path_for(json: &Path) -> PathBuf {
    let mut s = json.as_os_str().to_os_string();
    s.push(".jindex");
    PathBuf::from(s)
}

fn stored_path(p: &Path) -> String {
    strip_extended(&p.to_string_lossy())
}

fn strip_extended(s: &str) -> String {
    s.strip_prefix(r"\\?\").unwrap_or(s).to_string()
}

pub struct IndexOptions {
    pub select: Vec<String>,
    pub all: bool,
    pub fts: bool,
    pub batch: usize,
    pub force: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Ingest,
    Fts,
    Indexes,
}

pub struct Progress {
    pub phase: Phase,
    pub objects: u64,
    pub bytes: u64,
    pub total_bytes: u64,
    pub elapsed: f64,
    pub step: u32,
    pub steps: u32,
}

pub struct Summary {
    pub objects: u64,
    pub skipped: u64,
    pub oversized: u64,
    pub props: Vec<String>,
    pub missing: Vec<String>,
    pub elapsed: f64,
    pub db_bytes: u64,
    pub fts: bool,
    pub all: bool,
}

pub struct State {
    pub json_path: String,
    pub size: u64,
    pub fingerprint: u64,
    pub mtime: i64,
    pub props: Vec<String>,
    pub status: String,
    pub resume_offset: u64,
    pub next_id: i64,
    pub objects: u64,
    pub all: bool,
    pub fts: bool,
}

pub fn fingerprint(path: &Path) -> Result<(u64, u64, i64)> {
    let meta = fs::metadata(path).with_context(|| format!("no puedo leer {}", path.display()))?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut f = File::open(path)?;
    let mut h = DefaultHasher::new();
    h.write_u64(size);
    let mut buf = vec![0u8; 64 * 1024];
    let n = f.read(&mut buf)?;
    h.write(&buf[..n]);
    if size > (64 * 1024) as u64 {
        f.seek(SeekFrom::Start(size - 64 * 1024))?;
        let n = f.read(&mut buf)?;
        h.write(&buf[..n]);
    }
    Ok((size, h.finish(), mtime))
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-131072;
         PRAGMA mmap_size=268435456;",
    )?;
    crate::search::register_functions(conn)?;
    Ok(())
}

pub fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("no puedo abrir el índice {}", path.display()))?;
    let empty: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
    if empty == 0 {
        conn.pragma_update(None, "page_size", 16384_i64)?;
    }
    conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))?;
    configure(&conn)?;
    Ok(conn)
}

/// Abre el índice en modo lectura. Si SQLite no puede abrirlo así (por ejemplo,
/// porque falta el fichero `-shm` de un WAL), cae a una conexión de escritura
/// marcada como `query_only`.
pub fn open_db_ro(path: &Path) -> Result<Connection> {
    let opened = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY);
    match opened {
        Ok(conn) => {
            configure(&conn)?;
            conn.execute_batch("PRAGMA query_only=ON;")?;
            Ok(conn)
        }
        Err(_) => {
            let conn = open_db(path)?;
            conn.execute_batch("PRAGMA query_only=ON;")?;
            Ok(conn)
        }
    }
}

pub fn init_schema(conn: &Connection) -> Result<()> {
    init_tables(conn)?;
    init_indexes(conn)
}

fn init_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta(
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS paths(
             id INTEGER PRIMARY KEY,
             path TEXT NOT NULL UNIQUE
         );

         CREATE TABLE IF NOT EXISTS objects(
             id INTEGER PRIMARY KEY,
             root_key TEXT NOT NULL,
             byte_offset INTEGER NOT NULL,
             byte_length INTEGER NOT NULL
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS props(
             object_id INTEGER NOT NULL,
             path_id INTEGER NOT NULL,
             type INTEGER NOT NULL,
             value TEXT NOT NULL,
             num REAL,
             norm TEXT NOT NULL,
             PRIMARY KEY(object_id, path_id)
         ) WITHOUT ROWID;",
    )?;
    Ok(())
}

fn init_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_objects_key ON objects(root_key);
         CREATE INDEX IF NOT EXISTS idx_props_search ON props(path_id, norm, object_id) WHERE type IN (0,1,2,3);",
    )?;
    Ok(())
}

fn init_fts(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
             text,
             path_id UNINDEXED,
             object_id UNINDEXED,
             tokenize='trigram',
             detail=none
         );",
    )
    .context("no puedo crear el índice FTS5 (trigram); usa --force sin --fts")?;
    Ok(())
}

fn reset(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS fts;
         DROP TABLE IF EXISTS props;
         DROP TABLE IF EXISTS objects;
         DROP TABLE IF EXISTS paths;
         DROP TABLE IF EXISTS meta;",
    )?;
    init_tables(conn)
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?)
}

fn put_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn num<T: std::str::FromStr + Default>(conn: &Connection, key: &str) -> Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    Ok(get_meta(conn, key)?
        .unwrap_or_default()
        .parse::<T>()
        .unwrap_or_default())
}

fn flag(conn: &Connection, key: &str) -> Result<bool> {
    Ok(get_meta(conn, key)?.as_deref() == Some("1"))
}

pub fn read_state(conn: &Connection) -> Result<Option<State>> {
    let Some(fmt) = get_meta(conn, "format")? else {
        return Ok(None);
    };
    let fmt: i64 = fmt.parse().unwrap_or(0);
    if fmt != FORMAT_VERSION {
        bail!(
            "el índice usa la versión de formato {fmt} y esta build espera {FORMAT_VERSION}; reindexa con --force"
        );
    }
    let props: Vec<String> =
        serde_json::from_str(&get_meta(conn, "props")?.unwrap_or_else(|| "[]".into()))
            .unwrap_or_default();
    Ok(Some(State {
        json_path: strip_extended(&get_meta(conn, "json_path")?.unwrap_or_default()),
        size: num(conn, "size")?,
        fingerprint: num(conn, "fingerprint")?,
        mtime: num(conn, "mtime")?,
        props,
        status: get_meta(conn, "status")?.unwrap_or_default(),
        resume_offset: num(conn, "resume_offset")?,
        next_id: num(conn, "next_id")?,
        objects: num(conn, "objects")?,
        all: flag(conn, "all")?,
        fts: flag(conn, "fts")?,
    }))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Freshness {
    Ok,
    MtimeOnly,
}

pub fn check_source(conn: &Connection) -> Result<Freshness> {
    let st = read_state(conn)?.context("el índice está vacío")?;
    let json = PathBuf::from(&st.json_path);
    if !json.is_file() {
        bail!(
            "no encuentro el JSON original en {}; devuélvelo a su sitio o reindexa",
            st.json_path
        );
    }
    let (size, fp, mtime) = fingerprint(&json)?;
    if size != st.size || fp != st.fingerprint {
        bail!(
            "el contenido de {} ha cambiado desde que se indexó ({} → {} bytes).\nReindexa con --force.",
            st.json_path,
            st.size,
            size
        );
    }
    Ok(if mtime != st.mtime {
        Freshness::MtimeOnly
    } else {
        Freshness::Ok
    })
}

pub fn open_checked(path: &Path) -> Result<(Connection, Freshness)> {
    let conn = open_db_ro(path)?;
    let freshness = check_source(&conn)?;
    Ok((conn, freshness))
}

pub fn object_span(conn: &Connection, id: i64) -> Result<(u64, u64)> {
    let (off, len): (i64, i64) = conn
        .query_row(
            "SELECT byte_offset, byte_length FROM objects WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .with_context(|| format!("no existe ningún objeto con id {id} en este índice"))?;
    Ok((off as u64, len as u64))
}

pub fn index_json_path(conn: &Connection) -> Result<PathBuf> {
    match read_state(conn)? {
        Some(s) if !s.json_path.is_empty() => Ok(PathBuf::from(s.json_path)),
        _ => bail!("el índice no tiene registrado el JSON de origen"),
    }
}

fn path_id(conn: &Connection, cache: &mut HashMap<String, i64>, path: &str) -> Result<i64> {
    if let Some(&id) = cache.get(path) {
        return Ok(id);
    }
    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO paths(path) VALUES(?1)
             ON CONFLICT(path) DO UPDATE SET path=excluded.path
             RETURNING id",
        )?
        .query_row([path], |r| r.get(0))?;
    cache.insert(path.to_string(), id);
    Ok(id)
}

fn key_str(raw: &[u8]) -> Cow<'_, str> {
    if raw.contains(&b'\\') {
        let mut quoted = Vec::with_capacity(raw.len() + 2);
        quoted.push(b'"');
        quoted.extend_from_slice(raw);
        quoted.push(b'"');
        match serde_json::from_slice::<String>(&quoted) {
            Ok(s) => Cow::Owned(s),
            Err(_) => Cow::Borrowed(""),
        }
    } else {
        match std::str::from_utf8(raw) {
            Ok(s) => Cow::Borrowed(s),
            Err(_) => Cow::Owned(String::from_utf8_lossy(raw).into_owned()),
        }
    }
}

fn decode_string(raw: &[u8]) -> String {
    if raw.contains(&b'\\') {
        serde_json::from_slice::<String>(raw)
            .unwrap_or_else(|_| String::from_utf8_lossy(raw).into_owned())
    } else if raw.len() >= 2 {
        String::from_utf8_lossy(&raw[1..raw.len() - 1]).into_owned()
    } else {
        String::new()
    }
}

fn encode_value(raw: &[u8]) -> (i64, String) {
    match raw.first() {
        Some(b'"') => (T_STRING, decode_string(raw)),
        Some(b'{') => (T_OBJECT, truncate(raw)),
        Some(b'[') => (T_ARRAY, truncate(raw)),
        Some(b't') | Some(b'f') => (T_BOOL, String::from_utf8_lossy(raw).into_owned()),
        Some(b'n') => (T_NULL, "null".into()),
        _ => (T_NUMBER, String::from_utf8_lossy(raw).into_owned()),
    }
}

fn numeric_value(raw: &[u8], ty: i64) -> Option<f64> {
    if ty != T_NUMBER {
        return None;
    }
    std::str::from_utf8(raw)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
}

fn truncate(raw: &[u8]) -> String {
    if raw.len() <= MAX_STORED_NESTED {
        String::from_utf8_lossy(raw).into_owned()
    } else {
        let mut s = String::from_utf8_lossy(&raw[..MAX_STORED_NESTED]).into_owned();
        s.push('…');
        s
    }
}

pub fn build_index(
    json: &Path,
    db: &Path,
    opts: &IndexOptions,
    mut progress: impl FnMut(&Progress),
) -> Result<Summary> {
    if !json.is_file() {
        bail!("no existe el fichero {}", json.display());
    }
    if opts.all && !opts.select.is_empty() {
        bail!("--all y --select son incompatibles; usa uno de los dos");
    }
    if !opts.all && opts.select.is_empty() {
        bail!(
            "no has indicado ninguna propiedad; abre el JSON en el shell, consulta `props` y usa `index --select PROP`, o `index --all`"
        );
    }
    if opts.batch == 0 {
        bail!("--batch debe ser mayor que 0");
    }
    let abs_json = fs::canonicalize(json).unwrap_or_else(|_| json.to_path_buf());
    let abs_json_str = stored_path(&abs_json);
    let (size, fp, mtime) = fingerprint(json)?;

    let mut conn = open_db(db)?;
    // Fewer WAL checkpoints during a bulk build; one TRUNCATE at the end.
    conn.pragma_update(None, "wal_autocheckpoint", 10_000_i64)?;
    init_tables(&conn)?;

    let mut resume_offset = 0u64;
    let mut next_id = 0i64;
    let mut objects = 0u64;

    if opts.force {
        reset(&conn)?;
    }

    match read_state(&conn)? {
        None => {}
        Some(st) => {
            if st.json_path != abs_json_str || st.size != size || st.fingerprint != fp {
                bail!(
                    "el JSON ha cambiado desde que se indexó: contenido distinto ({} → {} bytes).\nEl índice queda invalidado; reindexa con --force.",
                    st.size,
                    size
                );
            }
            if st.all != opts.all || st.fts != opts.fts {
                bail!(
                    "el índice se creó con otras opciones (todas las propiedades: {}, FTS: {}).\nReindexa con --force.",
                    if st.all { "sí" } else { "no" },
                    if st.fts { "sí" } else { "no" }
                );
            }
            if !opts.all && st.props != opts.select {
                bail!(
                    "el índice se creó para [{}] pero has pedido [{}].\nMantén las mismas propiedades o reindexa con --force.",
                    st.props.join(", "),
                    opts.select.join(", ")
                );
            }
            if st.status == "complete" {
                bail!(
                    "este fichero ya está indexado en {}.\nUsa --force para rehacerlo desde cero.",
                    db.display()
                );
            }
            if st.status != "running" {
                bail!("el índice está en un estado inesperado: {}", st.status);
            }
            next_id = st.next_id;
            objects = st.objects;
            if next_id > 0 {
                let (offset, length) = object_span(&conn, next_id - 1)?;
                resume_offset = offset + length;
            }
        }
    }

    if resume_offset == 0 {
        let tx = conn.transaction()?;
        put_meta(&tx, "format", &FORMAT_VERSION.to_string())?;
        put_meta(&tx, "json_path", &abs_json_str)?;
        put_meta(&tx, "size", &size.to_string())?;
        put_meta(&tx, "fingerprint", &fp.to_string())?;
        put_meta(&tx, "mtime", &mtime.to_string())?;
        put_meta(&tx, "props", &serde_json::to_string(&opts.select)?)?;
        put_meta(&tx, "all", if opts.all { "1" } else { "0" })?;
        put_meta(&tx, "fts", if opts.fts { "1" } else { "0" })?;
        put_meta(&tx, "status", "running")?;
        put_meta(&tx, "resume_offset", "0")?;
        put_meta(&tx, "next_id", "0")?;
        put_meta(&tx, "objects", "0")?;
        put_meta(&tx, "fts_done", "0")?;
        tx.commit()?;
    }
    if opts.fts {
        init_fts(&conn)?;
    }

    let selected: HashSet<&str> = opts.select.iter().map(String::as_str).collect();
    let mut path_ids: HashMap<String, i64> = {
        let mut stmt = conn.prepare("SELECT path,id FROM paths")?;
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut members: Vec<MemberIdx> = Vec::new();

    let mut scanner = RootScanner::open(&abs_json, resume_offset)?;
    let mut key = String::new();
    let mut skipped = 0u64;
    let mut oversized = 0u64;
    let t0 = Instant::now();
    let mut finished = false;
    const PROP_CHUNK: usize = 100;
    let prop_chunk_sql = format!(
        "INSERT OR REPLACE INTO props(object_id,path_id,type,value,num,norm) VALUES {}",
        (0..PROP_CHUNK)
            .map(|_| "(?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(",")
    );
    progress(&Progress {
        phase: Phase::Ingest,
        objects,
        bytes: resume_offset,
        total_bytes: size,
        elapsed: 0.0,
        step: 0,
        steps: 0,
    });

    while !finished {
        let tx = conn.transaction()?;
        {
            let mut ins_obj = tx.prepare_cached(
                "INSERT INTO objects(id,root_key,byte_offset,byte_length) VALUES(?1,?2,?3,?4)",
            )?;
            let mut ins_prop = tx.prepare_cached(&prop_chunk_sql)?;
            let mut ins_prop_one = tx.prepare_cached(
                "INSERT OR REPLACE INTO props(object_id,path_id,type,value,num,norm)
                 VALUES(?1,?2,?3,?4,?5,?6)",
            )?;
            let mut prop_rows: Vec<SqlValue> = Vec::with_capacity(PROP_CHUNK * 6);

            for _ in 0..opts.batch {
                let span = match scanner.next_into(&mut key)? {
                    Some(s) => s,
                    None => {
                        finished = true;
                        break;
                    }
                };
                let id = next_id;
                next_id += 1;
                ins_obj.execute(params![
                    id,
                    key.as_str(),
                    span.start as i64,
                    span.len as i64
                ])?;

                let data = match scanner.window_mut().slice_opt(span)? {
                    Some(d) => d,
                    None => {
                        if span.len > CHUNK as u64 {
                            oversized += 1;
                        }
                        &[][..]
                    }
                };
                if data.first() == Some(&b'{') {
                    match object_members(data, &mut members) {
                        Ok(()) => {
                            for m in &members {
                                let k = key_str(&data[m.key_start..m.key_end]);
                                if !opts.all && !selected.contains(k.as_ref()) {
                                    continue;
                                }
                                let raw = &data[m.val_start..m.val_end];
                                let (ty, val) = encode_value(raw);
                                let num = numeric_value(raw, ty);
                                let norm = fold(&val);
                                let pid = path_id(&tx, &mut path_ids, k.as_ref())?;
                                prop_rows.push(SqlValue::Integer(id));
                                prop_rows.push(SqlValue::Integer(pid));
                                prop_rows.push(SqlValue::Integer(ty));
                                prop_rows.push(SqlValue::Text(val));
                                prop_rows.push(num.map(SqlValue::Real).unwrap_or(SqlValue::Null));
                                prop_rows.push(SqlValue::Text(norm));
                                if prop_rows.len() == PROP_CHUNK * 6 {
                                    ins_prop.execute(params_from_iter(prop_rows.iter()))?;
                                    prop_rows.clear();
                                }
                            }
                        }
                        Err(_) => skipped += 1,
                    }
                }
                objects += 1;
                resume_offset = span.end();
            }
            for row in prop_rows.chunks_exact(6) {
                ins_prop_one.execute(params_from_iter(row.iter()))?;
            }
        }

        let pos = scanner.position();
        put_meta(&tx, "resume_offset", &resume_offset.to_string())?;
        put_meta(&tx, "next_id", &next_id.to_string())?;
        put_meta(&tx, "objects", &objects.to_string())?;
        tx.commit()?;

        progress(&Progress {
            phase: Phase::Ingest,
            objects,
            bytes: pos,
            total_bytes: size,
            elapsed: t0.elapsed().as_secs_f64(),
            step: 0,
            steps: 0,
        });
    }

    if opts.fts && !flag(&conn, "fts_done")? {
        let batch_objects: i64 = 50_000;
        let steps = ((objects as i64 + batch_objects - 1) / batch_objects).max(1) as u32;
        conn.execute_batch("DELETE FROM fts;")?;
        progress(&Progress {
            phase: Phase::Fts,
            objects,
            bytes: 0,
            total_bytes: size,
            elapsed: t0.elapsed().as_secs_f64(),
            step: 0,
            steps,
        });
        let mut step = 0u32;
        let mut start = 0i64;
        while start < objects as i64 {
            let end = (start + batch_objects).min(objects as i64);
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO fts(text,path_id,object_id)
                 SELECT norm,path_id,object_id FROM props
                 WHERE type=?1 AND object_id>=?2 AND object_id<?3",
                params![T_STRING, start, end],
            )?;
            tx.commit()?;
            step += 1;
            progress(&Progress {
                phase: Phase::Fts,
                objects,
                bytes: end as u64,
                total_bytes: size,
                elapsed: t0.elapsed().as_secs_f64(),
                step,
                steps,
            });
            start = end;
        }
        let tx = conn.transaction()?;
        put_meta(&tx, "fts_done", "1")?;
        tx.commit()?;
    }

    // Bulk sorting must be able to spill to disk for large indexes.
    conn.execute_batch("PRAGMA temp_store=FILE;")?;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8) as i64;
    conn.pragma_update(None, "threads", threads)?;
    let indexes = [
        "CREATE INDEX IF NOT EXISTS idx_props_search ON props(path_id, norm, object_id) WHERE type IN (0,1,2,3)",
        "CREATE INDEX IF NOT EXISTS idx_objects_key ON objects(root_key)",
    ];
    progress(&Progress {
        phase: Phase::Indexes,
        objects,
        bytes: 0,
        total_bytes: size,
        elapsed: t0.elapsed().as_secs_f64(),
        step: 0,
        steps: indexes.len() as u32,
    });
    for (i, sql) in indexes.iter().enumerate() {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.commit()?;
        progress(&Progress {
            phase: Phase::Indexes,
            objects,
            bytes: 0,
            total_bytes: size,
            elapsed: t0.elapsed().as_secs_f64(),
            step: (i + 1) as u32,
            steps: indexes.len() as u32,
        });
    }

    let tx = conn.transaction()?;
    put_meta(&tx, "status", "complete")?;
    put_meta(&tx, "resume_offset", &scanner.position().to_string())?;
    put_meta(&tx, "objects", &objects.to_string())?;
    tx.commit()?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    let props: Vec<String> = if opts.all {
        let mut names: Vec<String> = path_ids.keys().cloned().collect();
        names.sort();
        names
    } else {
        opts.select.clone()
    };
    let missing: Vec<String> = if opts.all {
        Vec::new()
    } else {
        opts.select
            .iter()
            .filter(|p| !path_ids.contains_key(p.as_str()))
            .cloned()
            .collect()
    };

    Ok(Summary {
        objects,
        skipped,
        oversized,
        props,
        missing,
        elapsed: t0.elapsed().as_secs_f64(),
        db_bytes: fs::metadata(db).map(|m| m.len()).unwrap_or(0),
        fts: opts.fts,
        all: opts.all,
    })
}

pub struct PropInfo {
    pub name: String,
    pub count: u64,
    pub types: Vec<&'static str>,
}

pub fn discover_props(json: &Path, sample: usize) -> Result<Vec<PropInfo>> {
    let mut scanner = RootScanner::open(json, 0)?;
    let mut key = String::new();
    let mut members: Vec<MemberIdx> = Vec::new();
    let mut counts: HashMap<String, u64> = HashMap::new();
    let mut kinds: HashMap<String, Vec<i64>> = HashMap::new();

    for _ in 0..sample {
        let span = match scanner.next_into(&mut key)? {
            Some(s) => s,
            None => break,
        };
        let data = match scanner.window_mut().slice_opt(span)? {
            Some(d) => d,
            None => continue,
        };
        if data.first() != Some(&b'{') || object_members(data, &mut members).is_err() {
            continue;
        }
        for m in &members {
            let k = key_str(&data[m.key_start..m.key_end]).into_owned();
            *counts.entry(k.clone()).or_insert(0) += 1;
            let t = encode_value(&data[m.val_start..m.val_end]).0;
            let e = kinds.entry(k).or_default();
            if !e.contains(&t) {
                e.push(t);
            }
        }
    }

    let mut out: Vec<PropInfo> = counts
        .into_iter()
        .map(|(name, count)| {
            let mut types: Vec<&'static str> = kinds
                .get(&name)
                .map(|v| v.iter().map(|t| type_name(*t)).collect())
                .unwrap_or_default();
            types.sort_unstable();
            PropInfo { name, count, types }
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

pub struct Stats {
    pub json_path: PathBuf,
    pub objects: u64,
    pub paths: Vec<(String, u64)>,
    pub status: String,
    pub size: u64,
    pub all: bool,
    pub fts: bool,
}

pub fn stats(conn: &Connection) -> Result<Stats> {
    let st = read_state(conn)?.context("el índice está vacío")?;
    let mut paths = Vec::new();
    let mut q = conn.prepare(
        "SELECT pa.path, count(*) FROM props p JOIN paths pa ON pa.id=p.path_id GROUP BY pa.path ORDER BY 2 DESC",
    )?;
    let rows = q.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
    })?;
    for row in rows {
        paths.push(row?);
    }
    Ok(Stats {
        json_path: PathBuf::from(st.json_path),
        objects: st.objects,
        paths,
        status: st.status,
        size: st.size,
        all: st.all,
        fts: st.fts,
    })
}

pub fn object_span_by_key(conn: &Connection, key: &str) -> Result<(u64, u64)> {
    let (off, len): (i64, i64) = conn
        .query_row(
            "SELECT byte_offset, byte_length FROM objects WHERE root_key=?1 ORDER BY id LIMIT 1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .with_context(|| format!("no existe ninguna clave raíz `{key}` en este índice"))?;
    Ok((off as u64, len as u64))
}

pub struct Group {
    pub value: String,
    pub kind: i64,
    pub count: u64,
}

pub fn group_by(conn: &Connection, path: &str, limit: usize) -> Result<Vec<Group>> {
    let pid: Option<i64> = conn
        .query_row("SELECT id FROM paths WHERE path=?1", [path], |r| r.get(0))
        .optional()?;
    let Some(pid) = pid else {
        return Ok(Vec::new());
    };
    let limit = limit.clamp(1, 10_000) as i64;
    let mut stmt = conn.prepare(
        "SELECT type, value, count(*) AS n
           FROM props
          WHERE path_id=?1
          GROUP BY type, value
          ORDER BY n DESC, value
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![pid, limit], |r| {
        Ok(Group {
            kind: r.get(0)?,
            value: r.get(1)?,
            count: r.get::<_, i64>(2)? as u64,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub struct VerifyReport {
    pub objects: u64,
    pub checked: u64,
    pub checked_props: u64,
    pub oversized: u64,
    pub bad_keys: u64,
    pub bad_spans: u64,
    pub bad_json: u64,
    pub bad_props: u64,
    pub errors: Vec<String>,
}

impl VerifyReport {
    fn note(&mut self, msg: String) {
        if self.errors.len() < 20 {
            self.errors.push(msg);
        }
    }

    pub fn is_ok(&self) -> bool {
        self.bad_keys == 0 && self.bad_spans == 0 && self.bad_json == 0 && self.bad_props == 0
    }
}

fn prop_matches(v: &serde_json::Value, path: &str, ty: i64, stored: &str) -> bool {
    let Some(x) = v.get(path) else {
        return false;
    };
    match ty {
        T_STRING => x.as_str() == Some(stored),
        T_NUMBER => x
            .as_f64()
            .zip(stored.parse::<f64>().ok())
            .map(|(a, b)| a == b)
            .unwrap_or(false),
        T_BOOL => x.as_bool() == Some(stored == "true"),
        T_NULL => x.is_null() && stored == "null",
        T_OBJECT | T_ARRAY => {
            stored.ends_with('…')
                || serde_json::from_str::<serde_json::Value>(stored)
                    .map(|s| s == *x)
                    .unwrap_or(false)
        }
        _ => false,
    }
}

/// Recorre el origen y el índice en paralelo para comprobar claves, offsets y,
/// en una muestra, que cada objeto parsea y que sus propiedades coinciden.
/// `sample = 0` comprueba todos los objetos en profundidad.
pub fn verify(conn: &Connection, sample: usize) -> Result<VerifyReport> {
    let json = index_json_path(conn)?;
    let total: u64 =
        conn.query_row("SELECT count(*) FROM objects", [], |r| r.get::<_, i64>(0))? as u64;
    let step = if sample == 0 || total <= sample as u64 {
        1
    } else {
        (total / sample as u64).max(1)
    };

    let mut scanner = RootScanner::open(&json, 0)?;
    let mut rows_stmt =
        conn.prepare("SELECT id, root_key, byte_offset, byte_length FROM objects ORDER BY id")?;
    let mut rows = rows_stmt.query([])?;
    let mut prop_stmt = conn.prepare_cached(
        "SELECT pa.path, p.type, p.value
           FROM props p JOIN paths pa ON pa.id=p.path_id
          WHERE p.object_id=?1
          ORDER BY pa.path",
    )?;

    let mut report = VerifyReport {
        objects: 0,
        checked: 0,
        checked_props: 0,
        oversized: 0,
        bad_keys: 0,
        bad_spans: 0,
        bad_json: 0,
        bad_props: 0,
        errors: Vec::new(),
    };
    let mut key = String::new();
    let mut counter = 0u64;

    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let root_key: String = row.get(1)?;
        let offset: i64 = row.get(2)?;
        let length: i64 = row.get(3)?;

        let Some(span) = scanner.next_into(&mut key)? else {
            report.bad_spans += 1;
            report.note(format!(
                "el índice tiene más objetos que el origen (id {id})"
            ));
            break;
        };
        report.objects += 1;
        if key != root_key {
            report.bad_keys += 1;
            report.note(format!(
                "id {id}: clave `{key}` en el origen, `{root_key}` en el índice"
            ));
        }
        if span.start != offset as u64 || span.len != length as u64 {
            report.bad_spans += 1;
            report.note(format!(
                "id {id}: span {}:{} en el origen, {}:{} en el índice",
                span.start, span.len, offset, length
            ));
        }

        if step == 1 || counter.is_multiple_of(step) {
            report.checked += 1;
            if span.len > CHUNK as u64 {
                report.oversized += 1;
            } else if let Some(bytes) = scanner.window_mut().slice_opt(span)? {
                match serde_json::from_slice::<serde_json::Value>(bytes) {
                    Ok(value) => {
                        let mut props = prop_stmt.query([id])?;
                        while let Some(p) = props.next()? {
                            let path: String = p.get(0)?;
                            let ty: i64 = p.get(1)?;
                            let stored: String = p.get(2)?;
                            report.checked_props += 1;
                            if !prop_matches(&value, &path, ty, &stored) {
                                report.bad_props += 1;
                                report.note(format!(
                                    "id {id}: la propiedad `{path}` no coincide con el objeto origen"
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        report.bad_json += 1;
                        report.note(format!("id {id}: el objeto no parsea ({e})"));
                    }
                }
            }
        }
        counter += 1;
    }

    if scanner.next_into(&mut key)?.is_some() {
        report.bad_spans += 1;
        report.note("el origen tiene más objetos que el índice".to_string());
    }

    Ok(report)
}
