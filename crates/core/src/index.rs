use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::parser::{object_members, MemberIdx, RootScanner};

pub const FORMAT_VERSION: i64 = 1;
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
    pub batch: usize,
    pub force: bool,
}

pub struct Progress {
    pub objects: u64,
    pub bytes: u64,
    pub total_bytes: u64,
    pub elapsed: f64,
}

pub struct Summary {
    pub objects: u64,
    pub skipped: u64,
    pub props: Vec<String>,
    pub missing: Vec<String>,
    pub elapsed: f64,
    pub db_bytes: u64,
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

pub fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("no puedo abrir el índice {}", path.display()))?;
    conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))?;
    conn.execute_batch(
        "PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-131072;
         PRAGMA mmap_size=268435456;",
    )?;
    Ok(conn)
}

pub fn init_schema(conn: &Connection) -> Result<()> {
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
             PRIMARY KEY(object_id, path_id)
         ) WITHOUT ROWID;

         CREATE INDEX IF NOT EXISTS idx_props_lookup ON props(path_id, value);",
    )?;
    Ok(())
}

fn reset(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS props;
         DROP TABLE IF EXISTS objects;
         DROP TABLE IF EXISTS paths;
         DROP TABLE IF EXISTS meta;",
    )?;
    init_schema(conn)
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get::<_, String>(0))
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
    Ok(get_meta(conn, key)?.unwrap_or_default().parse::<T>().unwrap_or_default())
}

pub fn read_state(conn: &Connection) -> Result<Option<State>> {
    let Some(fmt) = get_meta(conn, "format")? else {
        return Ok(None);
    };
    let fmt: i64 = fmt.parse().unwrap_or(0);
    if fmt != FORMAT_VERSION {
        bail!("el índice usa la versión de formato {fmt} y esta build espera {FORMAT_VERSION}; reindexa con --force");
    }
    let props: Vec<String> =
        serde_json::from_str(&get_meta(conn, "props")?.unwrap_or_else(|| "[]".into())).unwrap_or_default();
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
        bail!("no encuentro el JSON original en {}; devuélvelo a su sitio o reindexa", st.json_path);
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
    Ok(if mtime != st.mtime { Freshness::MtimeOnly } else { Freshness::Ok })
}

pub fn open_checked(path: &Path) -> Result<(Connection, Freshness)> {
    let conn = open_db(path)?;
    let freshness = check_source(&conn)?;
    Ok((conn, freshness))
}

pub fn object_span(conn: &Connection, id: i64) -> Result<(u64, u64)> {
    let (off, len): (i64, i64) = conn
        .query_row("SELECT byte_offset, byte_length FROM objects WHERE id=?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
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
    let id: i64 = conn.query_row(
        "INSERT INTO paths(path) VALUES(?1)
         ON CONFLICT(path) DO UPDATE SET path=excluded.path
         RETURNING id",
        [path],
        |r| r.get(0),
    )?;
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
        serde_json::from_slice::<String>(raw).unwrap_or_else(|_| String::from_utf8_lossy(raw).into_owned())
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
    if opts.select.is_empty() {
        bail!("no has indicado ninguna propiedad. Ejecuta primero `jexplorer props <fichero>` y pasa --select");
    }
    let abs_json = fs::canonicalize(json).unwrap_or_else(|_| json.to_path_buf());
    let abs_json_str = stored_path(&abs_json);
    let (size, fp, mtime) = fingerprint(json)?;

    let mut conn = open_db(db)?;
    init_schema(&conn)?;

    let mut resume_offset = 0u64;
    let mut next_id = 0i64;
    let mut objects = 0u64;

    match read_state(&conn)? {
        None => {}
        Some(st) => {
            if opts.force {
                reset(&conn)?;
            } else {
                if st.json_path != abs_json_str || st.size != size || st.fingerprint != fp {
                    bail!(
                        "el JSON ha cambiado desde que se indexó: contenido distinto ({} → {} bytes).\nEl índice queda invalidado; reindexa con --force.",
                        st.size,
                        size
                    );
                }
                if st.props != opts.select {
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
                resume_offset = st.resume_offset;
                next_id = st.next_id;
                objects = st.objects;
            }
        }
    }

    if resume_offset == 0 {
        put_meta(&conn, "format", &FORMAT_VERSION.to_string())?;
        put_meta(&conn, "json_path", &abs_json_str)?;
        put_meta(&conn, "size", &size.to_string())?;
        put_meta(&conn, "fingerprint", &fp.to_string())?;
        put_meta(&conn, "mtime", &mtime.to_string())?;
        put_meta(&conn, "props", &serde_json::to_string(&opts.select)?)?;
        put_meta(&conn, "status", "running")?;
        put_meta(&conn, "resume_offset", "0")?;
        put_meta(&conn, "next_id", "0")?;
        put_meta(&conn, "objects", "0")?;
    }

    let selected: HashSet<&str> = opts.select.iter().map(|s| s.as_str()).collect();
    let mut seen: HashMap<&str, u64> = opts.select.iter().map(|s| (s.as_str(), 0u64)).collect();
    let mut path_ids: HashMap<String, i64> = HashMap::new();
    let mut members: Vec<MemberIdx> = Vec::new();

    let mut scanner = RootScanner::open(&abs_json, resume_offset)?;
    let mut key = String::new();
    let mut skipped = 0u64;
    let t0 = Instant::now();
    let mut finished = false;

    while !finished {
        let tx = conn.transaction()?;
        {
            let mut ins_obj = tx.prepare_cached(
                "INSERT INTO objects(id,root_key,byte_offset,byte_length) VALUES(?1,?2,?3,?4)",
            )?;
            let mut ins_prop = tx.prepare_cached(
                "INSERT OR REPLACE INTO props(object_id,path_id,type,value) VALUES(?1,?2,?3,?4)",
            )?;

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
                ins_obj.execute(params![id, key.as_str(), span.start as i64, span.len as i64])?;

                let data = match scanner.window_mut().slice_opt(span)? {
                    Some(d) => d,
                    None => &[][..],
                };
                if data.first() == Some(&b'{') {
                    match object_members(data, &mut members) {
                        Ok(()) => {
                            for m in &members {
                                let k = key_str(&data[m.key_start..m.key_end]);
                                if !selected.contains(k.as_ref()) {
                                    continue;
                                }
                                let (ty, val) = encode_value(&data[m.val_start..m.val_end]);
                                let pid = path_id(&tx, &mut path_ids, k.as_ref())?;
                                ins_prop.execute(params![id, pid, ty, val.as_str()])?;
                                if let Some(c) = seen.get_mut(k.as_ref()) {
                                    *c += 1;
                                }
                            }
                        }
                        Err(_) => skipped += 1,
                    }
                }
                objects += 1;
            }
        }

        let pos = scanner.position();
        put_meta(&tx, "resume_offset", &pos.to_string())?;
        put_meta(&tx, "next_id", &next_id.to_string())?;
        put_meta(&tx, "objects", &objects.to_string())?;
        tx.commit()?;

        progress(&Progress {
            objects,
            bytes: pos,
            total_bytes: size,
            elapsed: t0.elapsed().as_secs_f64(),
        });
    }

    put_meta(&conn, "status", "complete")?;
    put_meta(&conn, "resume_offset", &scanner.position().to_string())?;
    put_meta(&conn, "objects", &objects.to_string())?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    let missing: Vec<String> = opts
        .select
        .iter()
        .filter(|p| seen.get(p.as_str()).copied().unwrap_or(0) == 0)
        .cloned()
        .collect();

    Ok(Summary {
        objects,
        skipped,
        props: opts.select.clone(),
        missing,
        elapsed: t0.elapsed().as_secs_f64(),
        db_bytes: fs::metadata(db).map(|m| m.len()).unwrap_or(0),
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
}

pub fn stats(conn: &Connection) -> Result<Stats> {
    let st = read_state(conn)?.context("el índice está vacío")?;
    let mut paths = Vec::new();
    let mut q = conn.prepare(
        "SELECT pa.path, count(*) FROM props p JOIN paths pa ON pa.id=p.path_id GROUP BY pa.path ORDER BY 2 DESC",
    )?;
    let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
    for row in rows {
        paths.push(row?);
    }
    Ok(Stats {
        json_path: PathBuf::from(st.json_path),
        objects: st.objects,
        paths,
        status: st.status,
        size: st.size,
    })
}
