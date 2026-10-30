use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use jxcore::{Filter, Hit, Op, SearchQuery};
use rusqlite::Connection;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::{bytes, fmt, trim};

const PAGE: usize = 20;
const MAX_VIEW_LINES: usize = 200;

enum Next {
    Continue,
    Quit,
}

struct Session {
    conn: Connection,
    db: PathBuf,
    props: Vec<String>,
    hits: Vec<Hit>,
    last: Option<(Vec<Filter>, bool)>,
}

pub fn run(db: &Path) -> Result<()> {
    let (conn, freshness) = jxcore::open_checked(db)?;
    if freshness != jxcore::Freshness::Ok {
        eprintln!(
            "aviso: la fecha de modificación del JSON no coincide con la del índice; el contenido sigue intacto."
        );
    }
    let info = jxcore::stats(&conn)?;
    let props: Vec<&str> = info.paths.iter().map(|(p, _)| p.as_str()).collect();

    println!("JSON:  {}", info.json_path.display());
    println!(
        "       {} objetos · índice {} · propiedades: {}",
        fmt(info.objects),
        bytes(std::fs::metadata(db).map(|m| m.len()).unwrap_or(0)),
        if props.is_empty() {
            "ninguna".into()
        } else {
            props.join(", ")
        }
    );
    println!("       Escribe una consulta como `nombre = Antonio`, o `ayuda`. `q` para salir.\n");

    let mut session = Session {
        conn,
        db: db.to_path_buf(),
        props: props.iter().map(|p| p.to_string()).collect(),
        hits: Vec::new(),
        last: None,
    };
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
        match session.exec(&line) {
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

impl Session {
    fn exec(&mut self, line: &str) -> Result<Next> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(Next::Continue);
        }
        if looks_like_query(line) {
            self.query(line)?;
            return Ok(Next::Continue);
        }
        let (head, rest) = split_first(line).unwrap_or((line, ""));
        match head.to_ascii_lowercase().as_str() {
            "q" | "quit" | "exit" | "salir" => return Ok(Next::Quit),
            "h" | "help" | "ayuda" | "?" => print_help(),
            "n" | "next" | "sig" | "siguiente" => self.next_page()?,
            "v" | "ver" | "o" | "open" | "abrir" => self.view(parse_pos(rest)?, false)?,
            "r" | "raw" | "crudo" => self.view(parse_pos(rest)?, true)?,
            "e" | "export" | "exportar" => self.export(rest)?,
            "g" | "get" | "id" => self.by_id(parse_id(rest)?)?,
            "k" | "key" | "clave" => self.by_key(rest)?,
            "top" | "agrupar" | "groupby" | "grupos" => self.top(rest)?,
            "stats" => self.stats()?,
            "props" => self.props()?,
            "s" | "search" | "buscar" => self.query(rest)?,
            _ => self.query(line)?,
        }
        Ok(Next::Continue)
    }

    fn query(&mut self, rest: &str) -> Result<()> {
        let Some((path, rest)) = split_first(rest) else {
            bail!("uso: <propiedad> <operador> <valor>, por ejemplo `nombre = Antonio`");
        };
        let Some((op_str, value)) = split_first(rest) else {
            bail!("falta el operador: usa =, !=, ~, ~i, ^, >, >=, <, <=, ?, !?, null o ~r");
        };
        let (op, fold) = parse_op_fold(op_str)?;
        let value = value.trim();
        let value = if op.needs_value() {
            if value.is_empty() {
                bail!("falta el valor a buscar");
            }
            Some(value.to_string())
        } else {
            None
        };
        self.run_page(vec![Filter::new(path, op, value, fold)], false, None, true)
    }

    fn run_page(
        &mut self,
        filters: Vec<Filter>,
        any: bool,
        after: Option<i64>,
        fresh: bool,
    ) -> Result<()> {
        for f in &filters {
            if !self.props.iter().any(|p| p == &f.path) {
                let lista = if self.props.is_empty() {
                    "el índice no tiene ninguna propiedad".to_string()
                } else {
                    format!("disponibles: {}", self.props.join(", "))
                };
                bail!("la propiedad `{}` no está indexada; {lista}", f.path);
            }
        }
        let query = SearchQuery {
            filters: filters.clone(),
            any,
            limit: PAGE,
            after,
        };
        let t = Instant::now();
        let hits = jxcore::search(&self.conn, &query)?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;

        if hits.is_empty() {
            self.hits.clear();
            if fresh {
                self.last = Some((filters, any));
            }
            println!("sin resultados ({ms:.0} ms)");
            return Ok(());
        }

        if fresh {
            self.last = Some((filters.clone(), any));
        }
        let label = filters
            .iter()
            .find(|f| !matches!(f.op, Op::Exists | Op::Missing))
            .map(|f| f.path.as_str())
            .unwrap_or("");
        let shown = hits.len();
        self.hits = hits;
        for (i, h) in self.hits.iter().enumerate() {
            let value = if h.value.is_empty() {
                "—".to_string()
            } else {
                trim(&h.value, 44)
            };
            println!(
                "{:>3}  {:>9}  {:<26} {:<44} [{offset} · {len} B]",
                i + 1,
                h.id,
                trim(&h.root_key, 26),
                format!("{label} = {value}"),
                offset = h.offset,
                len = h.length
            );
        }
        let hint = if shown == PAGE {
            "   `n` para más, `ver <n>` para abrir"
        } else {
            "   `ver <n>` para abrir"
        };
        println!("\n{shown} resultado(s) en {ms:.0} ms{hint}");
        Ok(())
    }

    fn next_page(&mut self) -> Result<()> {
        let Some((filters, any)) = self.last.clone() else {
            println!("no hay ninguna búsqueda activa");
            return Ok(());
        };
        if self.hits.len() < PAGE {
            println!("no hay más resultados");
            return Ok(());
        }
        let after = self.hits.last().map(|h| h.id);
        self.run_page(filters, any, after, false)
    }

    fn view(&mut self, pos: usize, raw: bool) -> Result<()> {
        let hit = self
            .hits
            .get(pos.wrapping_sub(1))
            .ok_or_else(|| anyhow!("no hay un resultado {pos}; haz una búsqueda primero"))?;
        let (offset, length) = (hit.offset as u64, hit.length as u64);
        let json = jxcore::index_json_path(&self.conn)?;
        let data = jxcore::read_object(&json, offset, length)?;
        let text = if raw {
            String::from_utf8_lossy(&data).into_owned()
        } else {
            jxcore::pretty(&data)?
        };
        print_capped(&text);
        Ok(())
    }

    fn by_id(&mut self, id: i64) -> Result<()> {
        let json = jxcore::index_json_path(&self.conn)?;
        let (offset, length) = jxcore::object_span(&self.conn, id)?;
        let data = jxcore::read_object(&json, offset, length)?;
        print_capped(&jxcore::pretty(&data)?);
        Ok(())
    }

    fn by_key(&mut self, rest: &str) -> Result<()> {
        let key = unquote(rest.trim());
        if key.is_empty() {
            bail!("uso: key <clave-raíz>");
        }
        let json = jxcore::index_json_path(&self.conn)?;
        let (offset, length) = jxcore::object_span_by_key(&self.conn, &key)?;
        let data = jxcore::read_object(&json, offset, length)?;
        print_capped(&jxcore::pretty(&data)?);
        Ok(())
    }

    fn export(&mut self, rest: &str) -> Result<()> {
        let Some((pos, file)) = split_first(rest) else {
            bail!("uso: e <n> <fichero>");
        };
        let file = unquote(file.trim());
        if file.is_empty() {
            bail!("falta la ruta del fichero de salida");
        }
        let pos: usize = pos
            .parse()
            .map_err(|_| anyhow!("`{pos}` no es un número de resultado"))?;
        let hit = self
            .hits
            .get(pos.wrapping_sub(1))
            .ok_or_else(|| anyhow!("no hay un resultado {pos}; haz una búsqueda primero"))?;
        let json = jxcore::index_json_path(&self.conn)?;
        let data = jxcore::read_object(&json, hit.offset as u64, hit.length as u64)?;
        std::fs::write(&file, &data)?;
        println!("exportado el resultado {pos} a {file} ({} B)", data.len());
        Ok(())
    }

    fn top(&mut self, rest: &str) -> Result<()> {
        let Some((path, rest)) = split_first(rest) else {
            bail!("uso: top <propiedad> [n]");
        };
        let limit = if rest.trim().is_empty() {
            10
        } else {
            rest.trim()
                .parse::<usize>()
                .map_err(|_| anyhow!("`{rest}` no es un número"))?
        };
        let groups = jxcore::group_by(&self.conn, path, limit)?;
        if groups.is_empty() {
            println!("sin valores para `{path}`");
            return Ok(());
        }
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
        Ok(())
    }

    fn props(&self) -> Result<()> {
        let info = jxcore::stats(&self.conn)?;
        if info.paths.is_empty() {
            println!("el índice no tiene propiedades");
            return Ok(());
        }
        for (p, n) in &info.paths {
            println!("  {:<26} {:>12}", p, fmt(*n));
        }
        Ok(())
    }

    fn stats(&self) -> Result<()> {
        let info = jxcore::stats(&self.conn)?;
        println!("JSON:    {}", info.json_path.display());
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
            "Índice:  {}",
            bytes(std::fs::metadata(&self.db).map(|m| m.len()).unwrap_or(0))
        );
        Ok(())
    }
}

fn print_help() {
    println!(
        "\
consulta        nombre = Antonio        valor exacto
                nombre =i antonio       sin mayúsculas ni acentos
                nombre != Antonio       distinto
                nombre ~ Ant            contiene
                nombre ~i ant           contiene sin mayúsculas ni acentos
                nombre ^ Ant            empieza por   (^i sin acentos)
                edad > 40               también >=, < y <=
                email ?                 existe
                email !?                no existe
                email null              es nulo
                nombre ~r ^Ant          expresión regular
n               siguiente página
ver <n>         abrir el resultado n formateado   (v)
raw <n>         abrir el resultado n tal cual está en el fichero   (r)
id <id>         abrir un objeto por su id interno
key <clave>     abrir un objeto por su clave raíz   (k)
top <prop> [n]  valores más frecuentes de una propiedad
e <n> <fichero> exportar el resultado n a un fichero
props           propiedades indexadas
stats           datos del índice
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

fn parse_op_fold(s: &str) -> Result<(Op, bool)> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "=" | "==" | "eq" | "igual" => (Op::Eq, false),
        "=i" | "eqi" => (Op::Eq, true),
        "!=" | "<>" | "ne" | "distinto" => (Op::Ne, false),
        "~" | "contains" | "contiene" => (Op::Contains, false),
        "~i" | "contains_i" => (Op::Contains, true),
        "^" | "prefix" | "empieza" | "starts_with" => (Op::Prefix, false),
        "^i" | "prefix_i" => (Op::Prefix, true),
        ">" | "gt" | "mayor" => (Op::Gt, false),
        ">=" | "gte" | "mayor_igual" => (Op::Gte, false),
        "<" | "lt" | "menor" => (Op::Lt, false),
        "<=" | "lte" | "menor_igual" => (Op::Lte, false),
        "?" | "exists" | "existe" => (Op::Exists, false),
        "!?" | "missing" | "falta" => (Op::Missing, false),
        "null" | "nulo" => (Op::Null, false),
        "~r" | "regex" => (Op::Regex, false),
        other => bail!(
            "operador desconocido `{other}`: usa =, !=, ~, ~i, ^, >, >=, <, <=, ?, !?, null o ~r"
        ),
    })
}

fn parse_pos(s: &str) -> Result<usize> {
    let s = s.trim();
    s.parse()
        .map_err(|_| anyhow!("uso: ver <n>, donde n es el número de resultado"))
}

fn parse_id(s: &str) -> Result<i64> {
    let s = s.trim();
    s.parse()
        .map_err(|_| anyhow!("uso: id <id>, donde id es el identificador interno del objeto"))
}

fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}
