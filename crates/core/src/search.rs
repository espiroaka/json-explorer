use anyhow::{bail, Result};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params_from_iter, Connection, OptionalExtension};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Eq,
    Contains,
    Prefix,
}

impl Op {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "eq" | "=" | "igual" => Op::Eq,
            "contains" | "contiene" => Op::Contains,
            "prefix" | "starts" | "starts_with" => Op::Prefix,
            other => bail!("operador desconocido `{other}`; usa eq, contains o prefix"),
        })
    }
}

pub struct Hit {
    pub id: i64,
    pub root_key: String,
    pub value: String,
    pub kind: i64,
    pub offset: i64,
    pub length: i64,
}

pub struct SearchQuery {
    pub path: String,
    pub op: Op,
    pub value: String,
    pub limit: usize,
    pub after: Option<i64>,
}

const COLUMNS: &str = "o.id,o.root_key,p.value,p.type,o.byte_offset,o.byte_length";

pub fn search(conn: &Connection, q: &SearchQuery) -> Result<Vec<Hit>> {
    let pid: Option<i64> = conn
        .query_row("SELECT id FROM paths WHERE path=?1", [&q.path], |r| r.get(0))
        .optional()?;
    let Some(pid) = pid else {
        return Ok(Vec::new());
    };

    let after = q.after.unwrap_or(0);
    let limit = q.limit.clamp(1, 10_000) as i64;
    let mut args: Vec<SqlValue> = Vec::with_capacity(5);

    // En `eq` el valor es único, así que el índice (path_id,value) ya entrega las filas
    // ordenadas por object_id y la consulta puede parar al llegar al límite.
    // En `prefix` y `contains` se recorre `objects` en orden y se sondea la PK de props,
    // que evita ordenar todo el conjunto de coincidencias en un b-tree temporal.
    let sql = match q.op {
        Op::Eq => {
            args.push(SqlValue::Integer(pid));
            args.push(SqlValue::Text(q.value.clone()));
            args.push(SqlValue::Integer(after));
            args.push(SqlValue::Integer(limit));
            format!(
                "SELECT {COLUMNS}
                   FROM props p INDEXED BY idx_props_lookup
                   JOIN objects o ON o.id = p.object_id
                  WHERE p.path_id = ? AND p.value = ? AND p.object_id > ?
                  ORDER BY p.object_id
                  LIMIT ?"
            )
        }
        Op::Prefix => {
            args.push(SqlValue::Integer(pid));
            args.push(SqlValue::Text(q.value.clone()));
            args.push(SqlValue::Text(format!("{}\u{10FFFF}", q.value)));
            args.push(SqlValue::Integer(after));
            args.push(SqlValue::Integer(limit));
            format!(
                "SELECT {COLUMNS}
                   FROM objects o CROSS JOIN props p
                  WHERE p.object_id = o.id AND p.path_id = ?
                    AND p.value >= ? AND p.value < ? AND o.id > ?
                  ORDER BY o.id
                  LIMIT ?"
            )
        }
        Op::Contains => {
            args.push(SqlValue::Integer(pid));
            args.push(SqlValue::Text(q.value.clone()));
            args.push(SqlValue::Integer(after));
            args.push(SqlValue::Integer(limit));
            format!(
                "SELECT {COLUMNS}
                   FROM objects o CROSS JOIN props p
                  WHERE p.object_id = o.id AND p.path_id = ?
                    AND instr(p.value, ?) > 0 AND o.id > ?
                  ORDER BY o.id
                  LIMIT ?"
            )
        }
    };

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
        Ok(Hit {
            id: r.get(0)?,
            root_key: r.get(1)?,
            value: r.get(2)?,
            kind: r.get(3)?,
            offset: r.get(4)?,
            length: r.get(5)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
