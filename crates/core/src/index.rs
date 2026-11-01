use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{BuildHasherDefault, DefaultHasher, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};

use crate::parser::{CHUNK, MemberIdx, RootScanner, object_members};
use crate::store::{self, Manifest, ShardInfo};
use crate::text::fold;

pub const FORMAT_VERSION: i64 = 6;
pub const MAX_STORED_NESTED: usize = 4096;

pub const T_STRING: i64 = 0;
pub const T_NUMBER: i64 = 1;
pub const T_BOOL: i64 = 2;
pub const T_NULL: i64 = 3;
pub const T_OBJECT: i64 = 4;
pub const T_ARRAY: i64 = 5;

const INGEST_WAVE: usize = 50_000;
const PROP_CHUNK: usize = 100;
const FTS_BATCH: i64 = 200_000;
const PROGRESS_EVERY: Duration = Duration::from_millis(150);
const MAX_JOBS: usize = 64;
const MAX_BATCH: usize = 1_000_000;

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

pub fn auto_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 8)
}

pub struct IndexOptions {
    pub select: Vec<String>,
    pub all: bool,
    pub fts: bool,
    pub batch: usize,
    /// 0 elige un valor según los núcleos disponibles.
    pub jobs: usize,
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
    pub shards: usize,
}

struct BuildOutcome {
    objects: u64,
    oversized: u64,
    skipped: u64,
    db_bytes: u64,
    props: Vec<String>,
    missing: Vec<String>,
    elapsed: f64,
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

fn configure(conn: &Connection, cache_kb: i64) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(&format!(
        "PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-{cache_kb};
         PRAGMA mmap_size=268435456;"
    ))?;
    crate::search::register_functions(conn)?;
    Ok(())
}

fn open_db_with(path: &Path, cache_kb: i64) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("no puedo abrir el índice {}", path.display()))?;
    let empty: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
    if empty == 0 {
        conn.pragma_update(None, "page_size", 16384_i64)?;
    }
    // SQLite crea los ficheros con journal DELETE por defecto. La construcción
    // activa WAL temporalmente y lo desactiva al cerrar cada fragmento.
    configure(&conn, cache_kb)?;
    Ok(conn)
}

pub fn open_db(path: &Path) -> Result<Connection> {
    open_db_with(path, 131_072)
}

/// Abre un fragmento en modo lectura. Si SQLite no puede abrirlo así (por
/// ejemplo, porque falta el fichero `-shm` de un WAL), cae a una conexión de
/// escritura marcada como `query_only`.
pub fn open_db_ro(path: &Path) -> Result<Connection> {
    open_db_ro_with(path, 131_072)
}

fn open_db_ro_with(path: &Path, cache_kb: i64) -> Result<Connection> {
    let opened = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY);
    match opened {
        Ok(conn) => {
            configure(&conn, cache_kb)?;
            conn.execute_batch("PRAGMA query_only=ON;")?;
            Ok(conn)
        }
        Err(_) => {
            let conn = open_db_with(path, cache_kb)?;
            conn.execute_batch("PRAGMA query_only=ON;")?;
            Ok(conn)
        }
    }
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

fn init_fts(conn: &Connection) -> Result<()> {
    // Contentless: el texto ya vive en props.norm; ahorra ~5x espacio y tiempo.
    // La clave (object_id,path_id) se codifica en el rowid como object_id*base+path_id.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
             text,
             tokenize='trigram',
             detail=none,
             content='',
             columnsize=0
         );",
    )
    .context("no puedo crear el índice FTS5 (trigram); usa --force sin --fts")?;
    Ok(())
}

pub(crate) fn fts_base(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT coalesce(max(id),0)+1 FROM paths", [], |r| r.get(0))?)
}

/// FNV-1a: las claves de propiedad son cortas y se consultan una vez por
/// propiedad de cada objeto; SipHash dominaba el coste de interning.
#[derive(Default)]
struct FastHasher(u64);

impl Hasher for FastHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut h = 0xcbf2_9ce4_8422_2325u64 ^ self.0;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = h;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;
type FastSet<K> = HashSet<K, BuildHasherDefault<FastHasher>>;

fn path_id(conn: &Connection, cache: &mut FastMap<String, i64>, path: &str) -> Result<i64> {
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

fn shard_file(index: usize) -> String {
    format!("{}{index:03}{}", store::SHARD_PREFIX, store::SHARD_SUFFIX)
}

struct BatchObj {
    id: i64,
    key_off: usize,
    key_len: usize,
    data_off: usize,
    data_len: usize,
    span_start: u64,
    span_len: u64,
}

struct Batch {
    objs: Vec<BatchObj>,
    chunk: Vec<u8>,
    rootbuf: Vec<u8>,
}

enum Msg {
    Batch(Batch),
    Finish,
}

struct ShardDone {
    file: String,
    objects: u64,
    prop_rows: u64,
    paths: Vec<(String, u64)>,
    skipped: u64,
    bytes: u64,
}

struct WorkerArgs {
    index: usize,
    conn: Connection,
    rx: Receiver<Msg>,
    shard_file: String,
    shard_path: PathBuf,
    all: bool,
    fts: bool,
    selected: Arc<FastSet<String>>,
    progress: mpsc::Sender<Progress>,
    fts_objects: Arc<AtomicU64>,
    index_steps: Arc<AtomicU64>,
    total_index_steps: u32,
    counts: Arc<Mutex<Vec<u64>>>,
    barrier: Arc<Barrier>,
    abort: Arc<AtomicBool>,
    index_threads: i64,
    t0: Instant,
}

struct ShardState {
    path_ids: FastMap<String, i64>,
    path_counts: Vec<u64>,
    objects: u64,
    prop_rows: u64,
    skipped: u64,
    members: Vec<MemberIdx>,
    prop_values: Vec<SqlValue>,
    prop_chunk_sql: String,
}

impl ShardState {
    fn new() -> Self {
        Self {
            path_ids: FastMap::default(),
            path_counts: Vec::new(),
            objects: 0,
            prop_rows: 0,
            skipped: 0,
            members: Vec::new(),
            prop_values: Vec::with_capacity(PROP_CHUNK * 6),
            prop_chunk_sql: format!(
                "INSERT OR REPLACE INTO props(object_id,path_id,type,value,num,norm) VALUES {}",
                (0..PROP_CHUNK)
                    .map(|_| "(?,?,?,?,?,?)")
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    fn count(&mut self, pid: i64, seen_mask: &mut u128, seen_extra: &mut Vec<i64>) {
        let fresh = if (1..128).contains(&pid) {
            let bit = 1u128 << (pid as u32);
            let fresh = *seen_mask & bit == 0;
            *seen_mask |= bit;
            fresh
        } else if !seen_extra.contains(&pid) {
            seen_extra.push(pid);
            true
        } else {
            false
        };
        if fresh {
            let idx = pid as usize;
            if idx > self.path_counts.len() {
                self.path_counts.resize(idx, 0);
            }
            self.path_counts[idx - 1] += 1;
        }
    }

    fn write(
        &mut self,
        conn: &mut Connection,
        batch: &Batch,
        all: bool,
        selected: &FastSet<String>,
    ) -> Result<()> {
        let tx = conn.transaction()?;
        {
            let mut ins_obj = tx.prepare_cached(
                "INSERT INTO objects(id,root_key,byte_offset,byte_length) VALUES(?1,?2,?3,?4)",
            )?;
            let mut ins_prop = tx.prepare_cached(&self.prop_chunk_sql)?;
            let mut ins_prop_one = tx.prepare_cached(
                "INSERT OR REPLACE INTO props(object_id,path_id,type,value,num,norm)
                 VALUES(?1,?2,?3,?4,?5,?6)",
            )?;
            let mut seen_mask: u128;
            let mut seen_extra: Vec<i64> = Vec::new();
            for o in &batch.objs {
                let root = std::str::from_utf8(&batch.rootbuf[o.key_off..o.key_off + o.key_len])
                    .unwrap_or("");
                ins_obj.execute(params![o.id, root, o.span_start as i64, o.span_len as i64])?;
                self.objects += 1;
                let data = &batch.chunk[o.data_off..o.data_off + o.data_len];
                if data.first() != Some(&b'{') {
                    continue;
                }
                let members_ok = object_members(data, &mut self.members).is_ok();
                if !members_ok {
                    self.skipped += 1;
                    continue;
                }
                seen_mask = 0;
                seen_extra.clear();
                for i in 0..self.members.len() {
                    let m = self.members[i];
                    let k = key_str(&data[m.key_start..m.key_end]);
                    if !all && !selected.contains(k.as_ref()) {
                        continue;
                    }
                    let pid = match self.path_ids.get(k.as_ref()).copied() {
                        Some(pid) => pid,
                        None => path_id(&tx, &mut self.path_ids, k.as_ref())?,
                    };
                    self.count(pid, &mut seen_mask, &mut seen_extra);
                    let raw = &data[m.val_start..m.val_end];
                    let (ty, value) = encode_value(raw);
                    let num = numeric_value(raw, ty);
                    let norm = fold(&value);
                    self.prop_values.push(SqlValue::Integer(o.id));
                    self.prop_values.push(SqlValue::Integer(pid));
                    self.prop_values.push(SqlValue::Integer(ty));
                    self.prop_values.push(SqlValue::Text(value));
                    self.prop_values
                        .push(num.map(SqlValue::Real).unwrap_or(SqlValue::Null));
                    self.prop_values.push(SqlValue::Text(norm));
                    self.prop_rows += 1;
                    if self.prop_values.len() == PROP_CHUNK * 6 {
                        ins_prop.execute(params_from_iter(self.prop_values.iter()))?;
                        self.prop_values.clear();
                    }
                }
            }
            for row in self.prop_values.chunks_exact(6) {
                ins_prop_one.execute(params_from_iter(row.iter()))?;
            }
            self.prop_values.clear();
        }
        tx.commit()?;
        Ok(())
    }
}

fn build_fts(
    conn: &mut Connection,
    progress: &mpsc::Sender<Progress>,
    fts_objects: &AtomicU64,
    total_objects: u64,
    shard_objects: u64,
    t0: Instant,
) -> Result<()> {
    conn.execute_batch(
        "INSERT INTO fts(fts, rank) VALUES('automerge', 0);
         INSERT INTO fts(fts, rank) VALUES('crisismerge', 1000000);",
    )?;
    let base = fts_base(conn)?;
    let (min_id, max_id): (Option<i64>, Option<i64>) =
        conn.query_row("SELECT min(id), max(id) FROM objects", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let mut done = 0u64;
    if let (Some(min_id), Some(max_id)) = (min_id, max_id) {
        let mut start = min_id;
        loop {
            let end: i64 = conn.query_row(
                "SELECT max(id) FROM (SELECT id FROM objects WHERE id >= ?1 ORDER BY id LIMIT ?2)",
                params![start, FTS_BATCH],
                |r| r.get(0),
            )?;
            let batch_objects: u64 = conn.query_row(
                "SELECT count(*) FROM objects WHERE id >= ?1 AND id <= ?2",
                params![start, end],
                |r| r.get::<_, i64>(0),
            )? as u64;
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO fts(rowid,text)
                 SELECT object_id*?1+path_id, norm FROM props
                 WHERE type=?2 AND length(norm)>=3 AND object_id>=?3 AND object_id<=?4",
                params![base, T_STRING, start, end],
            )?;
            tx.commit()?;
            let global = fts_objects.fetch_add(batch_objects, Ordering::Relaxed) + batch_objects;
            let _ = progress.send(Progress {
                phase: Phase::Fts,
                objects: global,
                bytes: global,
                total_bytes: total_objects,
                elapsed: t0.elapsed().as_secs_f64(),
                step: 0,
                steps: 0,
            });
            done += batch_objects;
            if end >= max_id {
                break;
            }
            start = end + 1;
        }
    }
    ensure!(
        done == shard_objects,
        "el fragmento FTS procesó {done} objetos y esperaba {shard_objects}"
    );
    conn.execute_batch("INSERT INTO fts(fts) VALUES('optimize');")
        .context("no puedo compactar el índice FTS")?;
    let global = fts_objects.load(Ordering::Relaxed);
    let _ = progress.send(Progress {
        phase: Phase::Fts,
        objects: global,
        bytes: global,
        total_bytes: total_objects,
        elapsed: t0.elapsed().as_secs_f64(),
        step: 0,
        steps: 0,
    });
    Ok(())
}

fn run_worker(args: WorkerArgs) -> Result<ShardDone> {
    let WorkerArgs {
        index,
        mut conn,
        rx,
        shard_file,
        shard_path,
        all,
        fts,
        selected,
        progress,
        fts_objects,
        index_steps,
        total_index_steps,
        counts,
        barrier,
        abort,
        index_threads,
        t0,
    } = args;
    let mut state = ShardState::new();
    let mut failure: Option<anyhow::Error> = None;

    while let Ok(Msg::Batch(batch)) = rx.recv() {
        if let Err(error) = state.write(&mut conn, &batch, all, &selected) {
            failure = Some(error);
            abort.store(true, Ordering::Relaxed);
            break;
        }
    }
    // Al soltar el receptor el productor recibe un error de envío en cuanto
    // intente usar este fragmento, en vez de quedarse bloqueado.
    drop(rx);

    let objects = state.objects;
    counts.lock().unwrap_or_else(|e| e.into_inner())[index] =
        if failure.is_some() { 0 } else { objects };
    barrier.wait();
    if let Some(error) = failure {
        return Err(error);
    }
    if abort.load(Ordering::Relaxed) {
        bail!("la indexación se canceló porque otro fragmento falló");
    }
    let total_objects: u64 = counts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .sum();

    if fts {
        let _ = progress.send(Progress {
            phase: Phase::Fts,
            objects: fts_objects.load(Ordering::Relaxed),
            bytes: fts_objects.load(Ordering::Relaxed),
            total_bytes: total_objects,
            elapsed: t0.elapsed().as_secs_f64(),
            step: 0,
            steps: 0,
        });
        build_fts(
            &mut conn,
            &progress,
            &fts_objects,
            total_objects,
            objects,
            t0,
        )?;
    }

    // Bulk sorting must be able to spill to disk for large indexes.
    conn.execute_batch("PRAGMA temp_store=FILE;")?;
    conn.pragma_update(None, "threads", index_threads)?;
    let indexes = [
        "CREATE INDEX IF NOT EXISTS idx_props_search ON props(path_id, norm, object_id) WHERE type IN (0,1,2,3)",
        "CREATE INDEX IF NOT EXISTS idx_objects_key ON objects(root_key)",
    ];
    for sql in indexes {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.commit()?;
        let step = index_steps.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = progress.send(Progress {
            phase: Phase::Indexes,
            objects: total_objects,
            bytes: 0,
            total_bytes: total_objects,
            elapsed: t0.elapsed().as_secs_f64(),
            step: step as u32,
            steps: total_index_steps,
        });
    }

    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let mode: String = conn.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("delete") {
        bail!(
            "no puedo consolidar el fragmento en un único fichero (¿está abierto en otra ventana?)"
        );
    }
    drop(conn);
    let bytes = fs::metadata(&shard_path)
        .with_context(|| format!("no puedo leer {}", shard_path.display()))?
        .len();
    let paths = state
        .path_ids
        .iter()
        .map(|(name, &pid)| {
            (
                name.clone(),
                state
                    .path_counts
                    .get(pid as usize - 1)
                    .copied()
                    .unwrap_or(0),
            )
        })
        .collect();
    Ok(ShardDone {
        file: shard_file,
        objects,
        prop_rows: state.prop_rows,
        paths,
        skipped: state.skipped,
        bytes,
    })
}

struct CleanupOnError<'a> {
    dir: &'a Path,
    armed: bool,
}

impl Drop for CleanupOnError<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = store::clean_shards(self.dir);
        }
    }
}

fn send_batch(
    shard: usize,
    buffers: &mut [(Vec<BatchObj>, Vec<u8>, Vec<u8>)],
    senders: &[SyncSender<Msg>],
) -> Result<()> {
    let (objs, chunk, rootbuf) = &mut buffers[shard];
    if objs.is_empty() {
        return Ok(());
    }
    let batch = Batch {
        objs: std::mem::take(objs),
        chunk: std::mem::take(chunk),
        rootbuf: std::mem::take(rootbuf),
    };
    senders[shard]
        .send(Msg::Batch(batch))
        .map_err(|_| anyhow!("un fragmento dejó de responder"))?;
    Ok(())
}

pub fn build_index(
    json: &Path,
    dir: &Path,
    opts: &IndexOptions,
    progress: impl FnMut(&Progress) + Send + 'static,
) -> Result<Summary> {
    crate::init_sqlite();
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
    if opts.batch == 0 || opts.batch > MAX_BATCH {
        bail!("--batch debe estar entre 1 y {MAX_BATCH}");
    }
    let jobs = if opts.jobs == 0 {
        auto_jobs()
    } else {
        opts.jobs
    };
    if !(1..=MAX_JOBS).contains(&jobs) {
        bail!("--jobs debe estar entre 1 y {MAX_JOBS}");
    }
    if dir.is_file() {
        bail!("la ruta del índice debe ser una carpeta: {}", dir.display());
    }

    let abs_json = fs::canonicalize(json).unwrap_or_else(|_| json.to_path_buf());
    let abs_json_str = store::stored_path(&abs_json);
    let (size, fingerprint, mtime) = self::fingerprint(json)?;

    if let Ok(manifest) = store::read_manifest(dir) {
        if manifest.format != FORMAT_VERSION {
            if !opts.force {
                bail!(
                    "la carpeta {} contiene un índice de formato antiguo; usa --force para rehacerlo",
                    dir.display()
                );
            }
        } else if manifest.status == "complete" && !opts.force {
            if manifest.source != abs_json_str {
                bail!(
                    "la carpeta {} contiene el índice de otro JSON ({}).\nUsa --force para sustituirlo.",
                    dir.display(),
                    manifest.source
                );
            }
            if manifest.all != opts.all || manifest.fts != opts.fts {
                bail!(
                    "el índice se creó con otras opciones (todas las propiedades: {}, FTS: {}).\nReindexa con --force.",
                    if manifest.all { "sí" } else { "no" },
                    if manifest.fts { "sí" } else { "no" }
                );
            }
            if !opts.all && manifest.props != opts.select {
                bail!(
                    "el índice se creó para [{}] pero has pedido [{}].\nMantén las mismas propiedades o reindexa con --force.",
                    manifest.props.join(", "),
                    opts.select.join(", ")
                );
            }
            bail!(
                "este JSON ya está indexado en {}.\nUsa --force para rehacerlo.",
                dir.display()
            );
        }
    } else if store::has_shards(dir) && !opts.force {
        bail!(
            "la carpeta {} contiene restos de un índice sin manifiesto; usa --force para rehacerlo",
            dir.display()
        );
    }

    fs::create_dir_all(dir).with_context(|| format!("no puedo crear {}", dir.display()))?;
    store::clean_shards(dir)?;
    store::remove_manifest(dir)?;
    let mut manifest = Manifest {
        format: FORMAT_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        source: abs_json_str,
        size,
        fingerprint,
        mtime,
        all: opts.all,
        fts: opts.fts,
        props: if opts.all {
            Vec::new()
        } else {
            opts.select.clone()
        },
        status: "building".to_string(),
        objects: 0,
        prop_rows: 0,
        path_counts: Vec::new(),
        shards: Vec::new(),
        shard_count: jobs,
        created: store::now_epoch(),
        elapsed: 0.0,
    };
    store::write_manifest(dir, &manifest)?;
    let mut cleanup = CleanupOnError { dir, armed: true };
    let t0 = Instant::now();

    let mut connections = Vec::with_capacity(jobs);
    let cache_kb = (524_288 / jobs as i64).clamp(8_192, 131_072);
    for index in 0..jobs {
        let path = dir.join(shard_file(index));
        let conn = open_db_with(&path, cache_kb)?;
        init_tables(&conn)?;
        // El índice es desechable mientras se construye (el manifiesto dice
        // "building"): sin diario ni fsync se evita duplicar cada escritura en
        // el WAL, que dominaba el tiempo de ingesta.
        conn.query_row("PRAGMA journal_mode=OFF", [], |r| r.get::<_, String>(0))?;
        conn.execute_batch("PRAGMA synchronous=OFF; PRAGMA mmap_size=0;")?;
        if opts.fts {
            init_fts(&conn)?;
        }
        connections.push(conn);
    }

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let index_threads = (cores / jobs).clamp(1, 8) as i64;
    let selected: Arc<FastSet<String>> = Arc::new(opts.select.iter().cloned().collect());
    let counts = Arc::new(Mutex::new(vec![0u64; jobs]));
    let barrier = Arc::new(Barrier::new(jobs));
    let abort = Arc::new(AtomicBool::new(false));
    let fts_objects = Arc::new(AtomicU64::new(0));
    let index_steps = Arc::new(AtomicU64::new(0));
    let total_index_steps = (jobs * 2) as u32;
    let mut senders = Vec::with_capacity(jobs);
    let mut receivers = Vec::with_capacity(jobs);
    for _ in 0..jobs {
        let (sender, receiver) = mpsc::sync_channel::<Msg>(2);
        senders.push(sender);
        receivers.push(receiver);
    }
    let (progress_tx, progress_rx) = mpsc::channel::<Progress>();
    let progress_thread = std::thread::spawn(move || {
        let mut callback = progress;
        let mut last_fts = 0u64;
        for event in progress_rx {
            if event.phase == Phase::Fts {
                if event.objects < last_fts {
                    continue;
                }
                last_fts = event.objects;
            }
            callback(&event);
        }
    });

    let outcome: Result<BuildOutcome> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(jobs);
        for (index, (conn, rx)) in connections.into_iter().zip(receivers).enumerate() {
            let args = WorkerArgs {
                index,
                conn,
                rx,
                shard_file: shard_file(index),
                shard_path: dir.join(shard_file(index)),
                all: opts.all,
                fts: opts.fts,
                selected: Arc::clone(&selected),
                progress: progress_tx.clone(),
                fts_objects: Arc::clone(&fts_objects),
                index_steps: Arc::clone(&index_steps),
                total_index_steps,
                counts: Arc::clone(&counts),
                barrier: Arc::clone(&barrier),
                abort: Arc::clone(&abort),
                index_threads,
                t0,
            };
            handles.push(scope.spawn(move || run_worker(args)));
        }

        let producer: Result<(u64, u64)> = (|| {
            let mut scanner = RootScanner::open(&abs_json, 0)?;
            let mut key = String::new();
            let mut next_id = 0i64;
            let mut objects = 0u64;
            let mut oversized = 0u64;
            let mut finished = false;
            let mut last_report = Instant::now();
            let mut buffers: Vec<(Vec<BatchObj>, Vec<u8>, Vec<u8>)> = (0..jobs)
                .map(|_| {
                    (
                        Vec::with_capacity(opts.batch.min(INGEST_WAVE)),
                        Vec::with_capacity(256 * 1024),
                        Vec::with_capacity(4 * 1024),
                    )
                })
                .collect();
            let _ = progress_tx.send(Progress {
                phase: Phase::Ingest,
                objects: 0,
                bytes: 0,
                total_bytes: size,
                elapsed: 0.0,
                step: 0,
                steps: 0,
            });
            while !finished {
                for _ in 0..INGEST_WAVE {
                    let Some(span) = scanner.next_into(&mut key)? else {
                        finished = true;
                        break;
                    };
                    let id = next_id;
                    next_id += 1;
                    let data = match scanner.window_mut().slice_opt(span)? {
                        Some(data) => data,
                        None => {
                            if span.len > CHUNK as u64 {
                                oversized += 1;
                            }
                            &[][..]
                        }
                    };
                    let shard = (id % jobs as i64) as usize;
                    let (objs, chunk, rootbuf) = &mut buffers[shard];
                    let key_off = rootbuf.len();
                    rootbuf.extend_from_slice(key.as_bytes());
                    let data_off = chunk.len();
                    chunk.extend_from_slice(data);
                    objs.push(BatchObj {
                        id,
                        key_off,
                        key_len: rootbuf.len() - key_off,
                        data_off,
                        data_len: chunk.len() - data_off,
                        span_start: span.start,
                        span_len: span.len,
                    });
                    objects += 1;
                    if objs.len() >= opts.batch {
                        send_batch(shard, &mut buffers, &senders)?;
                    }
                    if last_report.elapsed() >= PROGRESS_EVERY {
                        last_report = Instant::now();
                        let _ = progress_tx.send(Progress {
                            phase: Phase::Ingest,
                            objects,
                            bytes: scanner.position(),
                            total_bytes: size,
                            elapsed: t0.elapsed().as_secs_f64(),
                            step: 0,
                            steps: 0,
                        });
                    }
                }
            }
            for shard in 0..jobs {
                send_batch(shard, &mut buffers, &senders)?;
            }
            let _ = progress_tx.send(Progress {
                phase: Phase::Ingest,
                objects,
                bytes: scanner.position(),
                total_bytes: size,
                elapsed: t0.elapsed().as_secs_f64(),
                step: 0,
                steps: 0,
            });
            Ok((objects, oversized))
        })();
        if producer.is_err() {
            abort.store(true, Ordering::Relaxed);
        }
        for sender in &senders {
            let _ = sender.send(Msg::Finish);
        }
        drop(senders);
        let mut results = Vec::with_capacity(jobs);
        for (index, handle) in handles.into_iter().enumerate() {
            let result = match handle.join() {
                Ok(result) => result.with_context(|| format!("fragmento {index}")),
                Err(_) => Err(anyhow!("el fragmento {index} terminó de forma inesperada")),
            };
            results.push(result);
        }
        let (objects, oversized) = producer?;
        let mut dons = Vec::with_capacity(jobs);
        for result in results {
            dons.push(result?);
        }
        let mut total_objects = 0u64;
        let mut prop_rows = 0u64;
        let mut skipped = 0u64;
        let mut db_bytes = 0u64;
        let mut path_counts: HashMap<String, u64> = HashMap::new();
        let mut shards = Vec::with_capacity(jobs);
        for done in dons {
            total_objects += done.objects;
            prop_rows += done.prop_rows;
            skipped += done.skipped;
            db_bytes += done.bytes;
            for (name, count) in done.paths {
                *path_counts.entry(name).or_insert(0) += count;
            }
            shards.push(ShardInfo {
                file: done.file,
                objects: done.objects,
                prop_rows: done.prop_rows,
            });
        }
        ensure!(
            total_objects == objects,
            "la suma de los fragmentos ({total_objects}) no coincide con los objetos leídos ({objects})"
        );
        let mut counts_sorted: Vec<(String, u64)> = path_counts.into_iter().collect();
        counts_sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        manifest.objects = objects;
        manifest.prop_rows = prop_rows;
        manifest.path_counts = counts_sorted;
        manifest.shards = shards;
        manifest.shard_count = jobs;
        manifest.status = "complete".to_string();
        manifest.elapsed = t0.elapsed().as_secs_f64();
        if opts.all {
            manifest.props = manifest
                .path_counts
                .iter()
                .map(|(name, _)| name.clone())
                .collect();
            manifest.props.sort();
        }
        store::write_manifest(dir, &manifest)?;
        cleanup.armed = false;
        let missing: Vec<String> = if opts.all {
            Vec::new()
        } else {
            opts.select
                .iter()
                .filter(|name| !manifest.path_counts.iter().any(|(path, _)| path == *name))
                .cloned()
                .collect()
        };
        Ok(BuildOutcome {
            objects,
            oversized,
            skipped,
            db_bytes,
            props: manifest.props.clone(),
            missing,
            elapsed: manifest.elapsed,
        })
    });
    drop(progress_tx);
    progress_thread
        .join()
        .map_err(|_| anyhow!("el hilo de progreso terminó de forma inesperada"))?;
    let outcome = outcome?;
    Ok(Summary {
        objects: outcome.objects,
        skipped: outcome.skipped,
        oversized: outcome.oversized,
        props: outcome.props,
        missing: outcome.missing,
        elapsed: outcome.elapsed,
        db_bytes: outcome.db_bytes,
        fts: opts.fts,
        all: opts.all,
        shards: jobs,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Ok,
    MtimeOnly,
}

pub struct Index {
    pub dir: PathBuf,
    pub manifest: Manifest,
    shards: Vec<Connection>,
}

impl Index {
    pub fn open(dir: &Path) -> Result<Self> {
        let manifest = store::read_manifest(dir)?;
        if manifest.format != FORMAT_VERSION {
            bail!(
                "el índice de {} usa un formato antiguo; reindexa con --force",
                dir.display()
            );
        }
        if manifest.status != "complete" {
            bail!(
                "el índice de {} está incompleto; reindexa con --force",
                dir.display()
            );
        }
        if manifest.shards.is_empty() || manifest.shard_count != manifest.shards.len() {
            bail!("el manifiesto de {} no es válido", dir.display());
        }
        let budget_kb = (524_288 / manifest.shards.len() as i64).clamp(8_192, 131_072);
        let mut shards = Vec::with_capacity(manifest.shards.len());
        for shard in &manifest.shards {
            let path = dir.join(&shard.file);
            shards.push(
                open_db_ro_with(&path, budget_kb)
                    .with_context(|| format!("no puedo abrir el fragmento {}", path.display()))?,
            );
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
            shards,
        })
    }

    pub fn open_checked(dir: &Path) -> Result<(Self, Freshness)> {
        let index = Self::open(dir)?;
        let freshness = index.check_source()?;
        Ok((index, freshness))
    }

    pub fn shards(&self) -> &[Connection] {
        &self.shards
    }

    pub fn shard(&self, id: i64) -> &Connection {
        &self.shards[id.rem_euclid(self.shards.len() as i64) as usize]
    }

    pub fn json_path(&self) -> PathBuf {
        PathBuf::from(&self.manifest.source)
    }

    pub fn check_source(&self) -> Result<Freshness> {
        let json = self.json_path();
        if !json.is_file() {
            bail!(
                "no encuentro el JSON original en {}; devuélvelo a su sitio o reindexa",
                self.manifest.source
            );
        }
        let (size, fingerprint, mtime) = fingerprint(&json)?;
        if size != self.manifest.size || fingerprint != self.manifest.fingerprint {
            bail!(
                "el contenido de {} ha cambiado desde que se indexó ({} → {} bytes).\nReindexa con --force.",
                self.manifest.source,
                self.manifest.size,
                size
            );
        }
        Ok(if mtime != self.manifest.mtime {
            Freshness::MtimeOnly
        } else {
            Freshness::Ok
        })
    }

    pub fn stats(&self) -> Stats {
        let index_bytes = self
            .manifest
            .shards
            .iter()
            .filter_map(|shard| fs::metadata(self.dir.join(&shard.file)).ok())
            .map(|meta| meta.len())
            .sum();
        Stats {
            json_path: self.json_path(),
            objects: self.manifest.objects,
            paths: self.manifest.path_counts.clone(),
            status: self.manifest.status.clone(),
            size: self.manifest.size,
            all: self.manifest.all,
            fts: self.manifest.fts,
            dir: self.dir.clone(),
            index_bytes,
            shards: self.shards.len(),
        }
    }

    pub fn object_span(&self, id: i64) -> Result<(u64, u64)> {
        let (off, len): (i64, i64) = self
            .shard(id)
            .query_row(
                "SELECT byte_offset, byte_length FROM objects WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .with_context(|| format!("no existe ningún objeto con id {id} en este índice"))?;
        Ok((off as u64, len as u64))
    }

    pub fn object_span_by_key(&self, key: &str) -> Result<(u64, u64)> {
        let mut best: Option<(i64, u64, u64)> = None;
        for conn in &self.shards {
            let row = conn
                .query_row(
                    "SELECT id, byte_offset, byte_length FROM objects WHERE root_key=?1 ORDER BY id LIMIT 1",
                    [key],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)? as u64,
                            r.get::<_, i64>(2)? as u64,
                        ))
                    },
                )
                .optional()?;
            if let Some((id, off, len)) = row {
                if best.is_none_or(|(best_id, _, _)| id < best_id) {
                    best = Some((id, off, len));
                }
            }
        }
        best.map(|(_, off, len)| (off, len))
            .with_context(|| format!("no existe ninguna clave raíz `{key}` en este índice"))
    }

    /// Recorre el origen y consulta el fragmento que corresponde a cada id para
    /// comprobar claves, offsets y, en una muestra, propiedades.
    /// `sample = 0` comprueba todos los objetos en profundidad.
    pub fn verify(&self, sample: usize) -> Result<VerifyReport> {
        let json = self.json_path();
        if !json.is_file() {
            bail!(
                "no encuentro el JSON original en {}; devuélvelo a su sitio o reindexa",
                self.manifest.source
            );
        }
        let total = self.manifest.objects;
        let step = if sample == 0 || total <= sample as u64 {
            1
        } else {
            (total / sample as u64).max(1)
        };

        let mut scanner = RootScanner::open(&json, 0)?;
        let mut span_stmts = Vec::with_capacity(self.shards.len());
        let mut prop_stmts = Vec::with_capacity(self.shards.len());
        for conn in &self.shards {
            span_stmts.push(
                conn.prepare("SELECT root_key, byte_offset, byte_length FROM objects WHERE id=?1")?,
            );
            prop_stmts.push(conn.prepare(
                "SELECT pa.path, p.type, p.value
                   FROM props p JOIN paths pa ON pa.id=p.path_id
                  WHERE p.object_id=?1
                  ORDER BY pa.path",
            )?);
        }

        let jobs = self.shards.len() as u64;
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
        let mut id = 0i64;

        while let Some(span) = scanner.next_into(&mut key)? {
            report.objects += 1;
            let shard = (id % jobs as i64) as usize;
            let row = {
                let mut rows = span_stmts[shard].query([id])?;
                match rows.next()? {
                    Some(row) => Some((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    )),
                    None => None,
                }
            };
            match row {
                None => {
                    report.bad_spans += 1;
                    report.note(format!(
                        "id {id}: el objeto existe en el origen y falta en el índice"
                    ));
                }
                Some((root_key, offset, length)) => {
                    if key != root_key {
                        report.bad_keys += 1;
                        report.note(format!(
                            "id {id}: clave `{key}` en el origen, `{root_key}` en el índice"
                        ));
                    }
                    if span.start != offset as u64 || span.len != length as u64 {
                        report.bad_spans += 1;
                        report.note(format!(
                            "id {id}: span {}:{} en el origen, {offset}:{length} en el índice",
                            span.start, span.len
                        ));
                    }
                }
            }

            if step == 1 || (id as u64).is_multiple_of(step) {
                report.checked += 1;
                if span.len > CHUNK as u64 {
                    report.oversized += 1;
                } else if let Some(bytes) = scanner.window_mut().slice_opt(span)? {
                    match serde_json::from_slice::<serde_json::Value>(bytes) {
                        Ok(value) => {
                            let mut props = prop_stmts[shard].query([id])?;
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
            id += 1;
        }

        for (shard, conn) in self.shards.iter().enumerate() {
            let actual: u64 =
                conn.query_row("SELECT count(*) FROM objects", [], |r| r.get::<_, i64>(0))? as u64;
            let expected = if report.objects > shard as u64 {
                (report.objects - shard as u64 - 1) / jobs + 1
            } else {
                0
            };
            if actual != expected {
                report.bad_spans += 1;
                report.note(format!(
                    "el fragmento {shard} tiene {actual} objetos y el origen {expected}"
                ));
            }
        }

        Ok(report)
    }
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
    pub dir: PathBuf,
    pub index_bytes: u64,
    pub shards: usize,
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
