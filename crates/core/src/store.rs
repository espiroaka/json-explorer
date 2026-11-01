use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::index::fingerprint;

pub const MANIFEST: &str = "manifest.json";
pub const SHARD_PREFIX: &str = "shard-";
pub const SHARD_SUFFIX: &str = ".jxdb";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShardInfo {
    pub file: String,
    pub objects: u64,
    pub prop_rows: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: i64,
    pub version: String,
    pub source: String,
    pub size: u64,
    pub fingerprint: u64,
    pub mtime: i64,
    pub all: bool,
    pub fts: bool,
    pub props: Vec<String>,
    pub status: String,
    pub objects: u64,
    pub prop_rows: u64,
    pub path_counts: Vec<(String, u64)>,
    pub shards: Vec<ShardInfo>,
    pub shard_count: usize,
    pub created: i64,
    pub elapsed: f64,
}

#[derive(Clone, Debug)]
pub struct StoredIndex {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub bytes: u64,
}

fn env_nonempty(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).filter(|value| !value.is_empty())
}

/// Raíz del almacén: todo lo que escribe jexplorer vive aquí salvo que el
/// usuario elija otra carpeta con `--db`.
pub fn data_root() -> PathBuf {
    if let Some(dir) = env_nonempty("JXPLORER_DATA_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        if let Some(dir) = env_nonempty("LOCALAPPDATA").or_else(|| env_nonempty("APPDATA")) {
            return PathBuf::from(dir).join("json-explorer");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(dir) = env_nonempty("XDG_DATA_HOME") {
            return PathBuf::from(dir).join("json-explorer");
        }
        if let Some(home) = env_nonempty("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("json-explorer");
        }
    }
    PathBuf::from("json-explorer")
}

pub fn indexes_root() -> PathBuf {
    data_root().join("indexes")
}

pub(crate) fn stored_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

pub fn is_index_dir(dir: &Path) -> bool {
    dir.join(MANIFEST).is_file()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn dir_name(source: &str) -> String {
    let stem = Path::new(source)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut slug = String::with_capacity(stem.len());
    for ch in stem.chars() {
        let next = if ch.is_ascii_alphanumeric() || ch == '_' {
            ch
        } else {
            '-'
        };
        if next == '-' && slug.ends_with('-') {
            continue;
        }
        slug.push(next);
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(40).collect();
    let slug = if slug.is_empty() {
        "indice".to_string()
    } else {
        slug
    };
    format!("{slug}-{:016x}", fnv1a(source.as_bytes()))
}

pub fn dir_for_json(json: &Path) -> Result<PathBuf> {
    let canonical =
        fs::canonicalize(json).with_context(|| format!("no puedo abrir {}", json.display()))?;
    Ok(indexes_root().join(dir_name(&stored_path(&canonical))))
}

pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    let path = dir.join(MANIFEST);
    let data = fs::read(&path)
        .with_context(|| format!("no encuentro el manifiesto {}", path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&data)
        .with_context(|| format!("el manifiesto {} está dañado", path.display()))?;
    Ok(manifest)
}

pub fn write_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
    fs::create_dir_all(dir)?;
    let data = serde_json::to_vec_pretty(manifest)?;
    let path = dir.join(MANIFEST);
    let tmp = dir.join(format!("{MANIFEST}.tmp"));
    let mut file =
        fs::File::create(&tmp).with_context(|| format!("no puedo escribir {}", tmp.display()))?;
    file.write_all(&data)?;
    file.sync_all()?;
    drop(file);
    if fs::rename(&tmp, &path).is_err() {
        let _ = fs::remove_file(&path);
        fs::rename(&tmp, &path)?;
    }
    Ok(())
}

pub(crate) fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn is_shard_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(SHARD_PREFIX) else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let rest = &rest[digits..];
    rest == SHARD_SUFFIX || rest.starts_with(&format!("{SHARD_SUFFIX}-"))
}

/// Indica si quedan ficheros de fragmento, aunque falte el manifiesto.
pub(crate) fn has_shards(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| is_shard_name(&entry.file_name().to_string_lossy()))
}

/// Borra solo los ficheros que genera la construcción de un índice.
pub(crate) fn clean_shards(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !is_shard_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

pub(crate) fn remove_manifest(dir: &Path) -> Result<()> {
    let path = dir.join(MANIFEST);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn shard_bytes(dir: &Path, manifest: &Manifest) -> u64 {
    manifest
        .shards
        .iter()
        .filter_map(|shard| fs::metadata(dir.join(&shard.file)).ok())
        .map(|meta| meta.len())
        .sum()
}

fn list_in(root: &Path) -> Result<Vec<StoredIndex>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(manifest) = read_manifest(&dir) else {
            continue;
        };
        let bytes = shard_bytes(&dir, &manifest);
        out.push(StoredIndex {
            dir,
            manifest,
            bytes,
        });
    }
    out.sort_by(|a, b| {
        b.manifest
            .created
            .cmp(&a.manifest.created)
            .then_with(|| a.dir.cmp(&b.dir))
    });
    Ok(out)
}

pub fn list_indices() -> Result<Vec<StoredIndex>> {
    list_in(&indexes_root())
}

fn find_in(root: &Path, json: &Path) -> Result<Option<StoredIndex>> {
    let canonical =
        fs::canonicalize(json).with_context(|| format!("no puedo abrir {}", json.display()))?;
    let source = stored_path(&canonical);
    let mut entries = list_in(root)?;
    if let Some(pos) = entries
        .iter()
        .position(|entry| entry.manifest.source == source)
    {
        return Ok(Some(entries.remove(pos)));
    }
    if !canonical.is_file() {
        return Ok(None);
    }
    let (size, fingerprint, mtime) = fingerprint(&canonical)?;
    // Reasocia un JSON movido o renombrado solo si su antigua ruta ya no
    // existe; así una copia del mismo contenido no roba el índice original.
    let mut candidates: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.manifest.status == "complete"
                && entry.manifest.size == size
                && entry.manifest.fingerprint == fingerprint
                && !Path::new(&entry.manifest.source).exists()
        })
        .map(|(index, _)| index)
        .collect();
    candidates.sort_by_key(|&index| std::cmp::Reverse(entries[index].manifest.created));
    let Some(index) = candidates.first().copied() else {
        return Ok(None);
    };
    let mut entry = entries.remove(index);
    entry.manifest.source = source;
    entry.manifest.mtime = mtime;
    write_manifest(&entry.dir, &entry.manifest)?;
    Ok(Some(entry))
}

pub fn find_for_json(json: &Path) -> Result<Option<StoredIndex>> {
    find_in(&indexes_root(), json)
}

fn remove_in(root: &Path, dir: &Path) -> Result<()> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let target = fs::canonicalize(dir)
        .with_context(|| format!("no encuentro el índice {}", dir.display()))?;
    if target == root || !target.starts_with(&root) {
        bail!("solo se pueden borrar índices guardados en el almacén");
    }
    read_manifest(&target).context("la carpeta no contiene un índice")?;
    fs::remove_dir_all(&target).with_context(|| format!("no puedo borrar {}", dir.display()))?;
    Ok(())
}

pub fn remove_index(dir: &Path) -> Result<()> {
    remove_in(&indexes_root(), dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::fingerprint;

    fn manifest_for(json: &Path, created: i64) -> Manifest {
        let (size, fingerprint, mtime) = fingerprint(json).unwrap();
        let canonical = fs::canonicalize(json).unwrap();
        Manifest {
            format: 6,
            version: "0.2.0".into(),
            source: stored_path(&canonical),
            size,
            fingerprint,
            mtime,
            all: true,
            fts: false,
            props: vec!["nombre".into()],
            status: "complete".into(),
            objects: 1,
            prop_rows: 1,
            path_counts: vec![("nombre".into(), 1)],
            shards: vec![ShardInfo {
                file: "shard-000.jxdb".into(),
                objects: 1,
                prop_rows: 1,
            }],
            shard_count: 1,
            created,
            elapsed: 0.1,
        }
    }

    fn store_index(root: &Path, json: &Path, created: i64) -> PathBuf {
        let dir = root.join(format!("indice-{created:04}"));
        write_manifest(&dir, &manifest_for(json, created)).unwrap();
        fs::write(dir.join("shard-000.jxdb"), b"datos").unwrap();
        dir
    }

    #[test]
    fn nombres_de_carpeta_estables_y_seguros() {
        let a = dir_name("C:/datos con espacios/clientes.json");
        assert!(a.starts_with("clientes-"), "{a}");
        assert_eq!(a.len(), "clientes-".len() + 16);
        assert_eq!(a, dir_name("C:/datos con espacios/clientes.json"));
        assert_ne!(a, dir_name("C:/otra/clientes.json"));
        assert!(
            a.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );

        let empty = dir_name("C:/x/---.json");
        assert!(empty.starts_with("indice-"), "{empty}");
        let unicode = dir_name("C:/x/María Ñandú.json");
        assert!(unicode.starts_with("Mar-a-and-"), "{unicode}");
        assert_eq!(
            stored_path(Path::new(r"\\?\C:\datos\clientes.json")),
            r"C:\datos\clientes.json"
        );
    }

    #[test]
    fn lista_ordena_por_fecha_y_omite_carpetas_sin_manifiesto() {
        let tmp = tempfile::tempdir().unwrap();
        let json = tmp.path().join("d.json");
        fs::write(&json, b"{}").unwrap();
        let root = tmp.path().join("store");
        fs::create_dir_all(root.join("suelta")).unwrap();
        let older = store_index(&root, &json, 10);
        let newer = store_index(&root, &json, 20);
        let entries = list_in(&root).unwrap();
        let dirs: Vec<PathBuf> = entries.iter().map(|entry| entry.dir.clone()).collect();
        assert_eq!(dirs, vec![newer, older]);
        assert_eq!(entries[0].bytes, 5);
        assert!(list_in(&tmp.path().join("no existe")).unwrap().is_empty());
    }

    #[test]
    fn encuentra_por_ruta_exacta_y_reasocia_solo_si_la_antigua_desaparece() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let json = tmp.path().join("original.json");
        fs::write(&json, b"{\"a\":1}").unwrap();
        let dir = store_index(&root, &json, 5);

        assert_eq!(find_in(&root, &json).unwrap().unwrap().dir, dir);

        let copy = tmp.path().join("copia.json");
        fs::copy(&json, &copy).unwrap();
        assert!(
            find_in(&root, &copy).unwrap().is_none(),
            "la ruta original sigue existiendo"
        );

        let moved = tmp.path().join("movido.json");
        fs::rename(&json, &moved).unwrap();
        let found = find_in(&root, &moved).unwrap().unwrap();
        assert_eq!(found.dir, dir);
        assert_eq!(
            read_manifest(&dir).unwrap().source,
            stored_path(&moved.canonicalize().unwrap())
        );
    }

    #[test]
    fn no_reasocia_indices_incompletos() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let json = tmp.path().join("original.json");
        fs::write(&json, b"{\"a\":1}").unwrap();
        let dir = store_index(&root, &json, 5);
        let mut manifest = read_manifest(&dir).unwrap();
        manifest.status = "building".into();
        write_manifest(&dir, &manifest).unwrap();
        let moved = tmp.path().join("movido.json");
        fs::rename(&json, &moved).unwrap();
        assert!(find_in(&root, &moved).unwrap().is_none());
    }

    #[test]
    fn borrar_solo_dentro_del_almacen_y_con_manifiesto() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let json = tmp.path().join("d.json");
        fs::write(&json, b"{}").unwrap();
        let dir = store_index(&root, &json, 1);

        let outside = tmp.path().join("fuera");
        write_manifest(&outside, &manifest_for(&json, 2)).unwrap();
        assert!(remove_in(&root, &outside).is_err());
        assert!(remove_in(&root, &root).is_err());

        let no_manifest = root.join("sin-manifiesto");
        fs::create_dir_all(&no_manifest).unwrap();
        assert!(remove_in(&root, &no_manifest).is_err());
        assert!(no_manifest.exists());

        remove_in(&root, &dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn limpieza_borra_fragmentos_y_conserva_lo_demas() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "shard-000.jxdb",
            "shard-12.jxdb-wal",
            "manifest.json",
            "nombres.txt",
            "shard-x.jxdb",
        ] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        fs::create_dir(dir.path().join("shard-9.jxdb")).unwrap();
        fs::write(dir.path().join("shard-9.jxdb").join("a"), b"y").unwrap();
        assert!(has_shards(dir.path()));
        clean_shards(dir.path()).unwrap();
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["manifest.json", "nombres.txt", "shard-x.jxdb"]);
        assert!(!has_shards(dir.path()));
        clean_shards(&dir.path().join("no existe")).unwrap();
    }

    #[test]
    fn manifiesto_ausente_o_danado_es_un_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_manifest(dir.path()).is_err());
        fs::write(dir.path().join(MANIFEST), b"no es json").unwrap();
        assert!(read_manifest(dir.path()).is_err());
    }
}
