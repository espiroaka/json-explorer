use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

pub const CHUNK: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub start: u64,
    pub len: u64,
}

impl Span {
    pub fn end(self) -> u64 {
        self.start + self.len
    }
}

fn eof_err() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "fin de fichero inesperado")
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

pub struct Window {
    file: File,
    buf: Vec<u8>,
    base: u64,
    len: usize,
    size: u64,
}

impl Window {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file,
            buf: vec![0; CHUNK],
            base: 0,
            len: 0,
            size,
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    fn fill_at(&mut self, pos: u64) -> io::Result<()> {
        if pos >= self.size {
            self.base = pos;
            self.len = 0;
            return Ok(());
        }
        let want = CHUNK.min((self.size - pos) as usize);
        self.file.seek(SeekFrom::Start(pos))?;
        self.file.read_exact(&mut self.buf[..want])?;
        self.base = pos;
        self.len = want;
        Ok(())
    }

    fn ensure(&mut self, pos: u64) -> io::Result<()> {
        if pos >= self.base && pos < self.base + self.len as u64 {
            return Ok(());
        }
        self.fill_at(pos)
    }

    pub fn byte_at(&mut self, pos: u64) -> io::Result<Option<u8>> {
        self.ensure(pos)?;
        if pos >= self.base + self.len as u64 {
            return Ok(None);
        }
        Ok(Some(self.buf[(pos - self.base) as usize]))
    }

    pub fn slice_opt(&mut self, s: Span) -> io::Result<Option<&[u8]>> {
        if s.len as usize > CHUNK {
            return Ok(None);
        }
        let covered = s.start >= self.base && s.end() <= self.base + self.len as u64;
        if !covered {
            self.fill_at(s.start)?;
        }
        if s.end() > self.base + self.len as u64 {
            return Ok(None);
        }
        let a = (s.start - self.base) as usize;
        let b = (s.end() - self.base) as usize;
        Ok(Some(&self.buf[a..b]))
    }

    pub fn skip_ws(&mut self, mut pos: u64) -> io::Result<u64> {
        loop {
            match self.byte_at(pos)? {
                Some(b) if b.is_ascii_whitespace() => pos += 1,
                _ => return Ok(pos),
            }
        }
    }

    pub fn skip_string(&mut self, start: u64) -> io::Result<u64> {
        if self.byte_at(start)? != Some(b'"') {
            return Err(bad(format!("se esperaba una cadena en el byte {start}")));
        }
        let mut i = start + 1;
        loop {
            self.ensure(i)?;
            let end = self.base + self.len as u64;
            if i >= end {
                return Err(eof_err());
            }
            let s = (i - self.base) as usize;
            let hay = &self.buf[s..self.len];
            match memchr::memchr2(b'"', b'\\', hay) {
                Some(j) => {
                    if hay[j] == b'"' {
                        return Ok(self.base + (s + j) as u64 + 1);
                    }
                    i = self.base + (s + j) as u64 + 2;
                }
                None => i = end,
            }
        }
    }

    pub fn scan_value_end(&mut self, start: u64) -> io::Result<u64> {
        match self.byte_at(start)? {
            None => Err(eof_err()),
            Some(b'"') => self.skip_string(start),
            Some(b'}') | Some(b']') | Some(b',') => {
                Err(bad(format!("valor vacío en el byte {start}")))
            }
            Some(b'{') | Some(b'[') => {
                let mut depth = 0i64;
                let mut pos = start;
                loop {
                    self.ensure(pos)?;
                    let end = self.base + self.len as u64;
                    if pos >= end {
                        return Err(eof_err());
                    }
                    let s = (pos - self.base) as usize;
                    let hay = &self.buf[s..self.len];
                    let Some(off) = next_structural(hay) else {
                        if end >= self.size {
                            return Err(eof_err());
                        }
                        pos = end;
                        continue;
                    };
                    let at = pos + off as u64;
                    match hay[off] {
                        b'"' => pos = self.skip_string(at)?,
                        b'{' | b'[' => {
                            depth += 1;
                            pos = at + 1;
                        }
                        _ => {
                            depth -= 1;
                            if depth == 0 {
                                return Ok(at + 1);
                            }
                            pos = at + 1;
                        }
                    }
                }
            }
            Some(_) => {
                let mut pos = start;
                loop {
                    self.ensure(pos)?;
                    let end = self.base + self.len as u64;
                    if pos >= end {
                        return Ok(pos);
                    }
                    let s = (pos - self.base) as usize;
                    let hay = &self.buf[s..self.len];
                    match hay.iter().position(|b| is_value_delimiter(*b)) {
                        Some(off) => return Ok(pos + off as u64),
                        None => pos = end,
                    }
                }
            }
        }
    }
}

fn next_structural(hay: &[u8]) -> Option<usize> {
    let first = memchr::memchr3(b'"', b'{', b'[', hay);
    let second = memchr::memchr3(b'"', b'}', b']', hay);
    match (first, second) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn is_value_delimiter(b: u8) -> bool {
    b == b',' || b == b'}' || b == b']' || b.is_ascii_whitespace()
}

pub struct RootScanner {
    win: Window,
    next: u64,
    done: bool,
    last_end: u64,
}

impl RootScanner {
    pub fn open(path: &Path, resume_at: u64) -> io::Result<Self> {
        let mut win = Window::open(path)?;
        let mut next = resume_at;
        if resume_at == 0 {
            if win.size() >= 3
                && win.byte_at(0)? == Some(0xEF)
                && win.byte_at(1)? == Some(0xBB)
                && win.byte_at(2)? == Some(0xBF)
            {
                next = 3;
            }
            next = win.skip_ws(next)?;
            if win.byte_at(next)? != Some(b'{') {
                return Err(bad("el JSON debe comenzar por un objeto raíz '{'"));
            }
            next += 1;
        }
        Ok(Self {
            win,
            next,
            done: false,
            last_end: resume_at,
        })
    }

    pub fn next_into(&mut self, key: &mut String) -> io::Result<Option<Span>> {
        if self.done {
            return Ok(None);
        }
        let mut pos = self.win.skip_ws(self.next)?;
        match self.win.byte_at(pos)? {
            None => return Err(eof_err()),
            Some(b'}') => {
                self.done = true;
                self.last_end = pos + 1;
                return Ok(None);
            }
            Some(b',') => pos = self.win.skip_ws(pos + 1)?,
            Some(b'"') => {}
            Some(_) => return Err(bad(format!("se esperaba una clave en el byte {pos}"))),
        }
        match self.win.byte_at(pos)? {
            Some(b'"') => {}
            Some(b'}') | None => {
                self.done = true;
                return Ok(None);
            }
            Some(_) => return Err(bad(format!("se esperaba una clave en el byte {pos}"))),
        }
        let key_start = pos;
        let after = self.win.skip_string(pos)?;
        key.clear();
        self.decode_key(key_start, after, key)?;
        pos = self.win.skip_ws(after)?;
        if self.win.byte_at(pos)? != Some(b':') {
            return Err(bad(format!("falta ':' tras la clave en el byte {pos}")));
        }
        pos = self.win.skip_ws(pos + 1)?;
        let vend = self.win.scan_value_end(pos)?;
        self.last_end = vend;
        self.next = vend;
        Ok(Some(Span {
            start: pos,
            len: vend - pos,
        }))
    }

    pub fn position(&self) -> u64 {
        self.last_end
    }

    pub fn window_mut(&mut self) -> &mut Window {
        &mut self.win
    }

    fn decode_key(&mut self, start: u64, end: u64, out: &mut String) -> io::Result<()> {
        let raw = match self.win.slice_opt(Span {
            start,
            len: end - start,
        })? {
            Some(r) => r,
            None => return Ok(()),
        };
        let inner = if raw.len() >= 2 {
            &raw[1..raw.len() - 1]
        } else {
            &[][..]
        };
        if inner.contains(&b'\\') {
            match serde_json::from_slice::<String>(raw) {
                Ok(s) => out.push_str(&s),
                Err(_) => out.push_str(&String::from_utf8_lossy(inner)),
            }
        } else {
            out.push_str(&String::from_utf8_lossy(inner));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MemberIdx {
    pub key_start: usize,
    pub key_end: usize,
    pub val_start: usize,
    pub val_end: usize,
}

fn skip_ws_mem(data: &[u8], mut i: usize) -> usize {
    while i < data.len() && data[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn scan_string_mem(data: &[u8], start: usize) -> anyhow::Result<usize> {
    if data.get(start) != Some(&b'"') {
        anyhow::bail!("se esperaba una cadena en la posición {start}");
    }
    let mut i = start + 1;
    while i < data.len() {
        match memchr::memchr2(b'"', b'\\', &data[i..]) {
            Some(j) => {
                let p = i + j;
                if data[p] == b'"' {
                    return Ok(p + 1);
                }
                i = p + 2;
            }
            None => break,
        }
    }
    anyhow::bail!("cadena sin cerrar")
}

fn scan_value_mem(data: &[u8], start: usize) -> anyhow::Result<usize> {
    match data.get(start) {
        None => anyhow::bail!("objeto truncado"),
        Some(b'"') => scan_string_mem(data, start),
        Some(b'}') | Some(b']') | Some(b',') => anyhow::bail!("valor vacío"),
        Some(b'{') | Some(b'[') => {
            let mut depth = 0i64;
            let mut i = start;
            while i < data.len() {
                match data[i] {
                    b'"' => {
                        i = scan_string_mem(data, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Ok(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            anyhow::bail!("contenedor sin cerrar")
        }
        Some(_) => {
            let mut i = start;
            while i < data.len() {
                let b = data[i];
                if b == b',' || b == b'}' || b == b']' || b.is_ascii_whitespace() {
                    break;
                }
                i += 1;
            }
            Ok(i)
        }
    }
}

pub fn object_members(data: &[u8], out: &mut Vec<MemberIdx>) -> anyhow::Result<()> {
    out.clear();
    let mut i = skip_ws_mem(data, 0);
    if data.get(i) != Some(&b'{') {
        anyhow::bail!("se esperaba un objeto en la posición {i}");
    }
    i += 1;
    loop {
        i = skip_ws_mem(data, i);
        match data.get(i) {
            Some(b'}') => return Ok(()),
            Some(b',') => {
                i += 1;
                continue;
            }
            Some(b'"') => {}
            Some(b) => anyhow::bail!("byte inesperado 0x{b:02x} en la posición {i}"),
            None => anyhow::bail!("objeto truncado"),
        }
        let key_start = i + 1;
        let after_key = scan_string_mem(data, i)?;
        let key_end = after_key - 1;
        i = skip_ws_mem(data, after_key);
        if data.get(i) != Some(&b':') {
            anyhow::bail!("falta ':' tras la clave en la posición {i}");
        }
        i = skip_ws_mem(data, i + 1);
        let val_start = i;
        let val_end = scan_value_mem(data, i)?;
        out.push(MemberIdx {
            key_start,
            key_end,
            val_start,
            val_end,
        });
        i = val_end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members_of(json: &str) -> Vec<(String, String)> {
        let mut v = Vec::new();
        object_members(json.as_bytes(), &mut v).unwrap();
        v.iter()
            .map(|m| {
                (
                    json[m.key_start..m.key_end].to_string(),
                    json[m.val_start..m.val_end].to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn parsea_anidados_y_escapes() {
        let got = members_of(r#"{"a":"x}y","b":{"c":[1,{"d":"]"}]},"e":12}"#);
        assert_eq!(
            got,
            vec![
                ("a".into(), r#""x}y""#.into()),
                ("b".into(), r#"{"c":[1,{"d":"]"}]}"#.into()),
                ("e".into(), "12".into()),
            ]
        );
    }

    #[test]
    fn tolera_espacios_y_vacios() {
        assert!(members_of("{}").is_empty());
        let got = members_of(" {\n \"a\" : null ,\n \"b\" : true } ");
        assert_eq!(
            got,
            vec![("a".into(), "null".into()), ("b".into(), "true".into())]
        );
    }

    #[test]
    fn rechaza_objeto_roto() {
        let mut v = Vec::new();
        assert!(object_members(br#"{"a": }"#, &mut v).is_err());
        assert!(object_members(br#"{"a": "sin cerrar}"#, &mut v).is_err());
    }
}
