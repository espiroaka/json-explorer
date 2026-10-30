use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub fn read_object(json: &Path, offset: u64, length: u64) -> Result<Vec<u8>> {
    let mut f = File::open(json).with_context(|| format!("no puedo abrir {}", json.display()))?;
    let size = f.metadata()?.len();
    if offset >= size {
        bail!("el offset {offset} queda fuera del fichero ({size} bytes)");
    }
    let len = (length as usize).min((size - offset) as usize);
    let mut buf = vec![0u8; len];
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn pretty(raw: &[u8]) -> Result<String> {
    let v: serde_json::Value =
        serde_json::from_slice(raw).context("el objeto no es JSON válido")?;
    Ok(serde_json::to_string_pretty(&v)?)
}
