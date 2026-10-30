use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params_from_iter};

use crate::index::{T_NULL, T_STRING, type_name};
use crate::text::fold;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Eq,
    Ne,
    Contains,
    Prefix,
    Gt,
    Gte,
    Lt,
    Lte,
    Exists,
    Missing,
    Null,
    Regex,
}

impl Op {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "eq" | "=" | "==" | "igual" => Op::Eq,
            "ne" | "!=" | "<>" | "distinto" => Op::Ne,
            "contains" | "contiene" | "~" => Op::Contains,
            "prefix" | "starts" | "starts_with" | "empieza" | "^" => Op::Prefix,
            "gt" | ">" | "mayor" => Op::Gt,
            "gte" | ">=" | "mayor_igual" => Op::Gte,
            "lt" | "<" | "menor" => Op::Lt,
            "lte" | "<=" | "menor_igual" => Op::Lte,
            "exists" | "existe" | "?" => Op::Exists,
            "missing" | "falta" | "!?" => Op::Missing,
            "null" | "nulo" => Op::Null,
            "regex" | "~r" => Op::Regex,
            other => bail!(
                "operador desconocido `{other}`; usa eq, ne, contains, prefix, gt/gte/lt/lte, exists, missing, null o regex"
            ),
        })
    }

    pub fn needs_value(self) -> bool {
        !matches!(self, Op::Exists | Op::Missing | Op::Null)
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Contains => "~",
            Op::Prefix => "^",
            Op::Gt => ">",
            Op::Gte => ">=",
            Op::Lt => "<",
            Op::Lte => "<=",
            Op::Exists => "?",
            Op::Missing => "!?",
            Op::Null => "null",
            Op::Regex => "~r",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Filter {
    pub path: String,
    pub op: Op,
    pub value: Option<String>,
    pub fold: bool,
}

impl Filter {
    pub fn new(
        path: impl Into<String>,
        op: Op,
        value: Option<impl Into<String>>,
        fold: bool,
    ) -> Self {
        Self {
            path: path.into(),
            op,
            value: value.map(Into::into),
            fold,
        }
    }

    pub fn describe(&self) -> String {
        match &self.value {
            Some(v) => format!("{} {} {v}", self.path, self.op.symbol()),
            None => format!("{} {}", self.path, self.op.symbol()),
        }
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

impl Hit {
    pub fn kind_name(&self) -> &'static str {
        type_name(self.kind)
    }
}

pub struct SearchQuery {
    pub filters: Vec<Filter>,
    pub any: bool,
    pub limit: usize,
    pub after: Option<i64>,
}

impl SearchQuery {
    pub fn single(
        path: impl Into<String>,
        op: Op,
        value: Option<String>,
        fold: bool,
        limit: usize,
        after: Option<i64>,
    ) -> Self {
        Self {
            filters: vec![Filter::new(path, op, value, fold)],
            any: false,
            limit,
            after,
        }
    }
}

const COLUMNS: &str = "o.id,o.root_key,p.value,p.type,o.byte_offset,o.byte_length";

pub(crate) fn register_functions(conn: &Connection) -> Result<()> {
    let cache: Arc<Mutex<HashMap<String, Regex>>> = Arc::new(Mutex::new(HashMap::new()));
    conn.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |ctx| {
            let pattern: String = ctx.get(0)?;
            let value: Option<String> = ctx.get(1)?;
            let Some(value) = value else {
                return Ok(false);
            };
            let mut map = cache.lock().map_err(|_| {
                rusqlite::Error::UserFunctionError("caché de regex envenenada".into())
            })?;
            if map.len() > 64 {
                map.clear();
            }
            if !map.contains_key(&pattern) {
                let re = Regex::new(&pattern)
                    .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
                map.insert(pattern.clone(), re);
            }
            Ok(map.get(&pattern).is_some_and(|re| re.is_match(&value)))
        },
    )?;
    Ok(())
}

fn resolve_path(conn: &Connection, path: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT id FROM paths WHERE path=?1", [path], |r| r.get(0))
        .optional()?)
}

fn has_fts(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='fts'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn fts_query(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    chars
        .windows(3)
        .map(|w| {
            let gram: String = w.iter().collect();
            format!("\"{}\"", gram.replace('"', "\"\""))
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn text(v: impl Into<String>) -> SqlValue {
    SqlValue::Text(v.into())
}

fn int(v: i64) -> SqlValue {
    SqlValue::Integer(v)
}

struct Plan {
    columns: String,
    from_where: String,
    order_col: &'static str,
    select_args: Vec<SqlValue>,
    where_args: Vec<SqlValue>,
}

impl Plan {
    fn fast(from_where: String, args: Vec<SqlValue>) -> Self {
        Self::fast_cols(COLUMNS.to_string(), from_where, args)
    }

    fn fast_no_props(from_where: String, args: Vec<SqlValue>) -> Self {
        Self::fast_cols(
            format!("o.id,o.root_key,'' AS value,{T_NULL} AS type,o.byte_offset,o.byte_length"),
            from_where,
            args,
        )
    }

    fn fast_cols(columns: String, from_where: String, args: Vec<SqlValue>) -> Self {
        Self {
            columns,
            from_where,
            order_col: "o.id",
            select_args: args.clone(),
            where_args: args,
        }
    }
}

fn build_predicate(f: &Filter, pid: Option<i64>, fts: bool) -> Result<(String, Vec<SqlValue>)> {
    let Some(pid) = pid else {
        return Ok(if f.op == Op::Missing {
            ("1".to_string(), Vec::new())
        } else {
            ("0".to_string(), Vec::new())
        });
    };

    let raw_value = f.value.clone().unwrap_or_default();
    let folded = fold(&raw_value);
    let col = if f.fold && f.op != Op::Regex {
        "p.norm"
    } else {
        "p.value"
    };
    let eq_value = if f.fold && f.op != Op::Regex {
        folded.clone()
    } else {
        raw_value.clone()
    };

    let (sql, args) = match f.op {
        Op::Eq => (
            format!(
                "EXISTS (SELECT 1 FROM props p WHERE p.object_id = o.id AND p.path_id = ? AND {col} = ?)"
            ),
            vec![int(pid), text(eq_value)],
        ),
        Op::Ne => (
            format!(
                "EXISTS (SELECT 1 FROM props p WHERE p.object_id = o.id AND p.path_id = ? AND {col} <> ?)"
            ),
            vec![int(pid), text(eq_value)],
        ),
        Op::Prefix => {
            let hi = format!("{eq_value}\u{10FFFF}");
            (
                format!(
                    "EXISTS (SELECT 1 FROM props p WHERE p.object_id = o.id AND p.path_id = ? AND {col} >= ? AND {col} < ?)"
                ),
                vec![int(pid), text(eq_value), text(hi)],
            )
        }
        Op::Gt | Op::Gte | Op::Lt | Op::Lte => {
            let n: f64 = raw_value.parse().map_err(|_| {
                anyhow!(
                    "el operador `{}` requiere un número; recibí `{raw_value}`",
                    f.op.symbol()
                )
            })?;
            let cmp = match f.op {
                Op::Gt => ">",
                Op::Gte => ">=",
                Op::Lt => "<",
                Op::Lte => "<=",
                _ => unreachable!(),
            };
            (
                format!(
                    "EXISTS (SELECT 1 FROM props p WHERE p.object_id = o.id AND p.path_id = ? AND p.num {cmp} ?)"
                ),
                vec![int(pid), SqlValue::Real(n)],
            )
        }
        Op::Contains => {
            if f.fold && fts && folded.chars().count() >= 3 {
                (
                    "EXISTS (SELECT 1 FROM fts JOIN props p ON p.object_id = fts.object_id AND p.path_id = fts.path_id WHERE fts.object_id = o.id AND fts.path_id = ? AND fts MATCH ? AND instr(p.norm, ?) > 0)"
                        .to_string(),
                    vec![int(pid), text(fts_query(&folded)), text(folded)],
                )
            } else {
                (
                    format!(
                        "EXISTS (SELECT 1 FROM props p WHERE p.object_id = o.id AND p.path_id = ? AND instr({col}, ?) > 0)"
                    ),
                    vec![int(pid), text(eq_value)],
                )
            }
        }
        Op::Exists => (
            "EXISTS (SELECT 1 FROM props p WHERE p.object_id = o.id AND p.path_id = ?)".to_string(),
            vec![int(pid)],
        ),
        Op::Missing => (
            "NOT EXISTS (SELECT 1 FROM props p WHERE p.object_id = o.id AND p.path_id = ?)".to_string(),
            vec![int(pid)],
        ),
        Op::Null => (
            "EXISTS (SELECT 1 FROM props p WHERE p.object_id = o.id AND p.path_id = ? AND p.type = ?)"
                .to_string(),
            vec![int(pid), int(T_NULL)],
        ),
        Op::Regex => (
            format!(
                "EXISTS (SELECT 1 FROM props p WHERE p.object_id = o.id AND p.path_id = ? AND {col} REGEXP ?)"
            ),
            vec![int(pid), text(raw_value)],
        ),
    };
    Ok((sql, args))
}

fn fast_plan(f: &Filter, pid: Option<i64>, fts: bool) -> Result<Option<Plan>> {
    let Some(pid) = pid else {
        let sql = if f.op == Op::Missing {
            "FROM objects o WHERE 1"
        } else {
            "FROM objects o WHERE 0"
        };
        return Ok(Some(Plan::fast_no_props(sql.to_string(), Vec::new())));
    };

    let raw_value = f.value.clone().unwrap_or_default();
    let folded = fold(&raw_value);
    let plan = match f.op {
        Op::Eq => {
            if raw_value.starts_with('{') || raw_value.starts_with('[') {
                return Ok(None);
            }
            let mut predicate = "p.type IN (0,1,2,3) AND p.norm = ?".to_string();
            let mut args = vec![int(pid), text(folded)];
            if !f.fold {
                predicate.push_str(" AND p.value = ?");
                args.push(text(raw_value));
            }
            let mut plan = Plan::fast(
                format!(
                    "FROM props p INDEXED BY idx_props_search JOIN objects o ON o.id = p.object_id WHERE p.path_id = ? AND {predicate}"
                ),
                args,
            );
            plan.order_col = "p.object_id";
            plan
        }
        Op::Prefix => {
            let (col, value) = if f.fold {
                ("p.norm", folded)
            } else {
                ("p.value", raw_value)
            };
            let hi = format!("{value}\u{10FFFF}");
            Plan::fast(
                format!(
                    "FROM objects o CROSS JOIN props p WHERE p.object_id = o.id AND p.path_id = ? AND {col} >= ? AND {col} < ?"
                ),
                vec![int(pid), text(value), text(hi)],
            )
        }
        Op::Gt | Op::Gte | Op::Lt | Op::Lte => {
            let n: f64 = raw_value.parse().map_err(|_| {
                anyhow!(
                    "el operador `{}` requiere un número; recibí `{raw_value}`",
                    f.op.symbol()
                )
            })?;
            let cmp = match f.op {
                Op::Gt => ">",
                Op::Gte => ">=",
                Op::Lt => "<",
                Op::Lte => "<=",
                _ => unreachable!(),
            };
            Plan::fast(
                format!(
                    "FROM objects o CROSS JOIN props p WHERE p.object_id = o.id AND p.path_id = ? AND p.num {cmp} ?"
                ),
                vec![int(pid), SqlValue::Real(n)],
            )
        }
        Op::Exists | Op::Null => {
            let mut sql =
                "FROM objects o CROSS JOIN props p WHERE p.object_id = o.id AND p.path_id = ?".to_string();
            let mut args = vec![int(pid)];
            if f.op == Op::Null {
                sql.push_str(" AND p.type = ?");
                args.push(int(T_NULL));
            }
            Plan::fast(sql, args)
        }
        Op::Missing => Plan::fast_no_props(
            "FROM objects o WHERE NOT EXISTS (SELECT 1 FROM props p WHERE p.object_id = o.id AND p.path_id = ?)"
                .to_string(),
            vec![int(pid)],
        ),
        Op::Contains if f.fold && fts && folded.chars().count() >= 3 => Plan::fast(
            "FROM fts JOIN props p ON p.object_id = fts.object_id AND p.path_id = fts.path_id JOIN objects o ON o.id = fts.object_id WHERE fts MATCH ? AND fts.path_id = ? AND p.type = ? AND instr(p.norm, ?) > 0"
                .to_string(),
            vec![text(fts_query(&folded)), int(pid), int(T_STRING), text(folded)],
        ),
        _ => return Ok(None),
    };
    Ok(Some(plan))
}

fn plan(conn: &Connection, q: &SearchQuery) -> Result<Plan> {
    if q.filters.is_empty() {
        bail!("no hay filtros que aplicar");
    }
    for f in &q.filters {
        if f.op.needs_value() && f.value.is_none() {
            bail!(
                "el operador `{}` del filtro `{}` necesita un valor",
                f.op.symbol(),
                f.path
            );
        }
    }

    let fts = has_fts(conn)?;
    let mut resolved: Vec<(&Filter, Option<i64>)> = Vec::with_capacity(q.filters.len());
    for f in &q.filters {
        resolved.push((f, resolve_path(conn, &f.path)?));
    }

    if q.filters.len() == 1 {
        let (f, pid) = resolved[0];
        if let Some(plan) = fast_plan(f, pid, fts)? {
            return Ok(plan);
        }
    }

    let mut display_args = Vec::new();
    let display = resolved
        .iter()
        .find(|(f, pid)| f.op != Op::Exists && f.op != Op::Missing && pid.is_some());
    let columns = if let Some((_, Some(pid))) = display {
        display_args.push(int(*pid));
        display_args.push(int(*pid));
        format!(
            "o.id,o.root_key,COALESCE((SELECT p.value FROM props p WHERE p.object_id = o.id AND p.path_id = ? LIMIT 1), ''),COALESCE((SELECT p.type FROM props p WHERE p.object_id = o.id AND p.path_id = ? LIMIT 1), {T_NULL}),o.byte_offset,o.byte_length"
        )
    } else {
        format!("o.id,o.root_key,'' AS value,{T_NULL} AS type,o.byte_offset,o.byte_length")
    };

    let mut where_args = Vec::new();
    let mut preds = Vec::with_capacity(resolved.len());
    for (f, pid) in &resolved {
        let (sql, args) = build_predicate(f, *pid, fts)?;
        preds.push(format!("({sql})"));
        where_args.extend(args);
    }

    let mut select_args = display_args;
    select_args.extend(where_args.iter().cloned());
    let joiner = if q.any { " OR " } else { " AND " };
    Ok(Plan {
        columns,
        from_where: format!("FROM objects o WHERE ({})", preds.join(joiner)),
        order_col: "o.id",
        select_args,
        where_args,
    })
}

pub fn search(conn: &Connection, q: &SearchQuery) -> Result<Vec<Hit>> {
    let plan = plan(conn, q)?;
    let mut args = plan.select_args;
    args.push(int(q.after.unwrap_or(-1)));
    args.push(int(q.limit.clamp(1, 10_000) as i64));
    let sql = format!(
        "SELECT {} {} AND {} > ? ORDER BY {} LIMIT ?",
        plan.columns, plan.from_where, plan.order_col, plan.order_col
    );
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

pub fn count(conn: &Connection, q: &SearchQuery) -> Result<u64> {
    let plan = plan(conn, q)?;
    let sql = format!("SELECT count(*) {}", plan.from_where);
    let n: i64 = conn.query_row(&sql, params_from_iter(plan.where_args.iter()), |r| r.get(0))?;
    Ok(n.max(0) as u64)
}
