use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use unicode_width::UnicodeWidthChar;

use super::{Next, OutputOptions, PAGE, Shell, check_limit, fmt, parse_query, tokenize};

#[path = "results.rs"]
mod results;
use results::Results;

struct Terminal {
    active: bool,
    ready: bool,
}

impl Terminal {
    fn new() -> Result<Self> {
        let mut terminal = Self {
            active: false,
            ready: false,
        };
        terminal.resume()?;
        Ok(terminal)
    }

    fn resume(&mut self) -> Result<()> {
        self.ready = false;
        terminal::enable_raw_mode()?;
        self.active = true;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            Hide,
            EnableBracketedPaste
        )?;
        self.ready = true;
        Ok(())
    }

    fn suspend(&mut self) -> Result<()> {
        self.ready = false;
        if self.active {
            let result = execute!(
                io::stdout(),
                DisableBracketedPaste,
                Show,
                LeaveAlternateScreen
            );
            terminal::disable_raw_mode()?;
            self.active = false;
            result?;
        }
        Ok(())
    }

    fn command(&mut self, shell: &mut Shell, line: &str) -> Result<Next> {
        self.suspend()?;
        let result = shell.exec(line);
        if let Err(error) = &result {
            eprintln!("error: {}", clean(&format!("{error:#}")));
        }
        let pause = if !matches!(result, Ok(Next::Quit)) {
            println!("\nPulsa Enter para volver al menú.");
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)
        } else {
            Ok(0)
        };
        self.resume()?;
        pause?;
        result
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

struct Row {
    text: String,
    selected: bool,
}

impl Row {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            selected: false,
        }
    }
}

pub(super) fn clean(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

fn clip(text: &str, width: usize) -> String {
    let mut used = 0;
    clean(text)
        .chars()
        .take_while(|ch| {
            used += ch.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

fn body_height() -> Result<usize> {
    Ok(usize::from(terminal::size()?.1).saturating_sub(5).max(1))
}

fn draw(title: &str, subtitle: &str, rows: &[Row], footer: &str) -> Result<()> {
    let (width, height) = terminal::size()?;
    if width == 0 || height == 0 {
        return Ok(());
    }
    let columns = usize::from(width.saturating_sub(1));
    let mut out = io::stdout();
    queue!(
        out,
        MoveTo(0, 0),
        Clear(ClearType::All),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print(clip(title, columns)),
        SetAttribute(Attribute::Reset),
        ResetColor
    )?;
    if height > 1 {
        queue!(out, MoveTo(0, 1), Print(clip(subtitle, columns)))?;
    }
    for (index, row) in rows
        .iter()
        .take(usize::from(height).saturating_sub(5))
        .enumerate()
    {
        queue!(out, MoveTo(0, index as u16 + 3))?;
        if row.selected {
            queue!(out, SetAttribute(Attribute::Reverse))?;
        }
        queue!(
            out,
            Print(clip(&row.text, columns)),
            SetAttribute(Attribute::Reset)
        )?;
    }
    if height > 2 {
        queue!(
            out,
            MoveTo(0, height - 1),
            SetForegroundColor(Color::DarkGrey),
            Print(clip(footer, columns)),
            ResetColor
        )?;
    }
    out.flush()?;
    Ok(())
}

fn key() -> Result<KeyEvent> {
    loop {
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => return Ok(key),
            Event::Resize(_, _) => return Ok(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE)),
            _ => {}
        }
    }
}

/// Espera una tecla. Mientras el total se cuenta en segundo plano vuelve cada
/// poco; `None` pide redibujar cuando el recuento termina.
fn results_event(results: &mut Results) -> Result<Option<KeyEvent>> {
    if !results.count_pending() {
        return Ok(Some(key()?));
    }
    loop {
        if event::poll(Duration::from_millis(120))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => return Ok(Some(key)),
                Event::Resize(_, _) => {
                    return Ok(Some(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE)));
                }
                _ => continue,
            }
        }
        if results.poll_count() {
            return Ok(None);
        }
    }
}

fn back(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc | KeyCode::Left | KeyCode::Char('q'))
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn move_selection(selected: &mut usize, len: usize, code: KeyCode, step: usize) {
    if len == 0 {
        return;
    }
    *selected = match code {
        KeyCode::Up => selected.saturating_sub(1),
        KeyCode::Down => (*selected + 1).min(len - 1),
        KeyCode::Home => 0,
        KeyCode::End => len - 1,
        KeyCode::PageUp => selected.saturating_sub(step),
        KeyCode::PageDown => selected.saturating_add(step).min(len - 1),
        _ => *selected,
    };
}

fn menu(
    title: &str,
    subtitle: &str,
    items: &[String],
    selected: &mut usize,
) -> Result<Option<usize>> {
    loop {
        let height = body_height()?;
        let start = selected.saturating_sub(height - 1);
        let rows: Vec<_> = items
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(index, item)| Row {
                text: format!(" {} {item}", if index == *selected { ">" } else { " " }),
                selected: index == *selected,
            })
            .collect();
        draw(
            title,
            subtitle,
            &rows,
            "↑/↓ elegir · Enter/→ abrir · ←/Esc volver · Inicio/Fin · RePág/AvPág",
        )?;
        let event = key()?;
        if back(event) {
            return Ok(None);
        }
        if matches!(event.code, KeyCode::Enter | KeyCode::Right) && !items.is_empty() {
            return Ok(Some(*selected));
        }
        move_selection(selected, items.len(), event.code, height);
    }
}

fn choose(title: &str, subtitle: &str, items: &[&str]) -> Result<Option<usize>> {
    menu(
        title,
        subtitle,
        &items.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
        &mut 0,
    )
}

struct Input {
    chars: Vec<char>,
    cursor: usize,
}

impl Input {
    fn insert(&mut self, text: &str) {
        for ch in clean(text).chars() {
            self.chars.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }

    fn visible(&self, width: usize) -> String {
        let suffix_width: usize = self.chars[self.cursor..]
            .iter()
            .map(|ch| ch.width().unwrap_or(0))
            .sum();
        let left_width = width.saturating_sub(suffix_width.min(width / 2) + 1);
        let mut start = self.cursor;
        let mut used = 0;
        while start > 0 {
            let next = self.chars[start - 1].width().unwrap_or(0);
            if used + next > left_width {
                break;
            }
            used += next;
            start -= 1;
        }
        let prefix: String = self.chars[start..self.cursor].iter().collect();
        let suffix: String = self.chars[self.cursor..].iter().collect();
        format!("> {prefix}│{suffix}")
    }

    fn edit(&mut self, event: KeyEvent) {
        // Windows reports AltGr as Ctrl+Alt.
        let altgr = matches!(event.code, KeyCode::Char(_))
            && event
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if event
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && !altgr
        {
            return;
        }
        match event.code {
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            KeyCode::Char(ch) => self.insert(&ch.to_string()),
            _ => {}
        }
    }
}

fn prompt(title: &str, hint: &str) -> Result<Option<String>> {
    let mut input = Input {
        chars: Vec::new(),
        cursor: 0,
    };
    loop {
        let width = usize::from(terminal::size()?.0).saturating_sub(4).max(1);
        draw(
            title,
            hint,
            &[Row::text(input.visible(width))],
            "Enter aceptar · Esc cancelar · ←/→ editar · pegar con Ctrl+V o terminal",
        )?;
        match event::read()? {
            Event::Paste(text) => input.insert(&text),
            Event::Key(event) if event.kind != KeyEventKind::Release => {
                if event.code == KeyCode::Esc
                    || (event.code == KeyCode::Char('c')
                        && event.modifiers.contains(KeyModifiers::CONTROL)
                        && !event.modifiers.contains(KeyModifiers::ALT))
                {
                    return Ok(None);
                }
                if event.code == KeyCode::Enter {
                    return Ok(Some(input.chars.iter().collect()));
                }
                input.edit(event);
            }
            _ => {}
        }
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut row = String::new();
        let mut used = 0;
        for ch in clean(line).chars() {
            let next = ch.width().unwrap_or(0);
            if used + next > width && !row.is_empty() {
                lines.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(ch);
            used += next;
        }
        lines.push(row);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn document(title: &str, text: &str) -> Result<()> {
    let mut selected = 0;
    loop {
        let width = usize::from(terminal::size()?.0).saturating_sub(1);
        let lines = wrap(text, width);
        selected = selected.min(lines.len() - 1);
        let height = body_height()?;
        let rows: Vec<_> = lines
            .iter()
            .skip(selected)
            .take(height)
            .map(Row::text)
            .collect();
        draw(
            title,
            &format!("Línea {} / {}", selected + 1, lines.len()),
            &rows,
            "↑/↓ desplazar · RePág/AvPág · Inicio/Fin · ←/Esc volver",
        )?;
        let event = key()?;
        if back(event) {
            return Ok(());
        }
        move_selection(&mut selected, lines.len(), event.code, height);
    }
}

fn viewer(session: &super::Session, hit: &jxcore::Hit) -> Result<()> {
    session.index.check_source()?;
    let source = session.index.json_path();
    let data = jxcore::read_object(&source, hit.offset as u64, hit.length as u64)?;
    let pretty = jxcore::pretty(&data)?;
    let raw = String::from_utf8_lossy(&data);
    let mut raw_mode = false;
    let mut selected = 0;
    let mut layout = None;
    let mut lines = Vec::new();
    loop {
        let width = usize::from(terminal::size()?.0).saturating_sub(1);
        if layout != Some((width, raw_mode)) {
            lines = wrap(if raw_mode { &raw } else { &pretty }, width);
            layout = Some((width, raw_mode));
        }
        selected = selected.min(lines.len() - 1);
        let height = body_height()?;
        let rows: Vec<_> = lines
            .iter()
            .skip(selected)
            .take(height)
            .map(Row::text)
            .collect();
        draw(
            &format!("Objeto · {} · id {}", hit.root_key, hit.id),
            &format!(
                "Línea {} / {} · {} bytes · {}",
                selected + 1,
                lines.len(),
                hit.length,
                if raw_mode { "original" } else { "formateado" }
            ),
            &rows,
            "↑/↓ desplazar · RePág/AvPág · Inicio/Fin · r original · e exportar · ←/Esc volver",
        )?;
        let event = key()?;
        if back(event) {
            return Ok(());
        }
        match event.code {
            KeyCode::Char('r') => {
                raw_mode = !raw_mode;
                selected = 0;
            }
            KeyCode::Char('e') => {
                if let Some(file) = prompt(
                    "Exportar objeto",
                    "Ruta del nuevo fichero; se guardarán los bytes originales",
                )? {
                    if !Path::new(&file).exists()
                        || choose("Sobrescribir fichero", &file, &["Cancelar", "Sobrescribir"])?
                            == Some(1)
                    {
                        match session.export_hit(hit, &file) {
                            Ok(length) => {
                                document("Exportado", &format!("{file}\n{length} bytes"))?
                            }
                            Err(error) => document("Error", &format!("{error:#}"))?,
                        }
                    }
                }
            }
            _ => move_selection(&mut selected, lines.len(), event.code, height),
        }
    }
}

const OPERATORS: &[&str] = &[
    "= igual",
    "!= distinto",
    "~ contiene",
    "^ empieza por",
    "=i igual sin mayúsculas/acentos",
    "~i contiene sin mayúsculas/acentos",
    "> mayor",
    ">= mayor o igual",
    "< menor",
    "<= menor o igual",
    "? existe",
    "!? no existe",
    "null es null",
    "~r expresión regular",
];

fn condition_summary(filters: &[jxcore::Filter], any: bool) -> String {
    let joiner = if any { " | " } else { " & " };
    clip(
        &filters
            .iter()
            .map(jxcore::Filter::describe)
            .collect::<Vec<_>>()
            .join(joiner),
        48,
    )
}

fn ask_filter(props: &[String]) -> Result<Option<jxcore::Filter>> {
    if props.is_empty() {
        document("Sin propiedades", "este índice no guarda ninguna propiedad")?;
        return Ok(None);
    }
    let Some(property) = menu("Propiedad", "Elige una propiedad indexada", props, &mut 0)? else {
        return Ok(None);
    };
    let Some(operator) = choose("Operador", &props[property], OPERATORS)? else {
        return Ok(None);
    };
    let (op, fold) = super::parse_op_fold(
        OPERATORS[operator]
            .split_whitespace()
            .next()
            .unwrap_or_default(),
    )?;
    let value = if op.needs_value() {
        prompt("Valor a buscar", &props[property])?
    } else {
        None
    };
    Ok(Some(jxcore::Filter::new(&props[property], op, value, fold)))
}

fn reload(
    session: &mut super::Session,
    filters: &[jxcore::Filter],
    any: bool,
    page_size: usize,
) -> Result<Results> {
    let results = Results::start(&session.index, filters.to_vec(), any, page_size)?;
    session.last = Some((filters.to_vec(), any));
    Ok(results)
}

/// Valores más frecuentes de una propiedad dentro de las coincidencias actuales.
/// Devuelve el filtro que añade la selección, si el usuario elige uno.
fn facet(
    index: &jxcore::Index,
    props: &[String],
    filters: &[jxcore::Filter],
    any: bool,
    total: Option<u64>,
) -> Result<Option<jxcore::Filter>> {
    let Some(property) = menu(
        "Valores frecuentes",
        "Elige una propiedad de las coincidencias",
        props,
        &mut 0,
    )?
    else {
        return Ok(None);
    };
    let path = props[property].clone();
    let groups = jxcore::group_matching(
        index,
        &path,
        &jxcore::SearchQuery {
            filters: filters.to_vec(),
            any,
            limit: PAGE,
            after: None,
        },
        100,
    )?;
    if groups.is_empty() {
        document(
            "Valores frecuentes",
            &format!("ninguna coincidencia tiene `{path}`"),
        )?;
        return Ok(None);
    }
    let labels: Vec<String> = groups
        .iter()
        .map(|group| format!("{}    {}", clip(&group.value, 52), fmt(group.count)))
        .collect();
    let subtitle = match total {
        Some(total) => format!(
            "{} coincidencias; las que no tienen `{path}` no cuentan · Enter añade la condición",
            fmt(total)
        ),
        None => format!("las que no tienen `{path}` no cuentan · Enter añade la condición"),
    };
    let Some(pick) = menu(&format!("Valores de {path}"), &subtitle, &labels, &mut 0)? else {
        return Ok(None);
    };
    Ok(Some(jxcore::Filter::new(
        path,
        jxcore::Op::Eq,
        Some(groups[pick].value.clone()),
        false,
    )))
}

/// Mensaje de las acciones que no pueden añadir una condición a una búsqueda con `|`.
const ANY_CONFLICT: &str =
    "esta búsqueda une condiciones con | ; vuelve a hacerla con & para poder añadirlas";

/// Copia `filters` y añade `filter` al final.
fn with_extra(filters: &[jxcore::Filter], filter: jxcore::Filter) -> Vec<jxcore::Filter> {
    let mut extended = filters.to_vec();
    extended.push(filter);
    extended
}

/// Lo que la tecla pulsada en la pantalla de coincidencias pide hacer.
/// Las variantes `*Blocked` son rechazos: la acción no tiene sentido con las
/// condiciones actuales y solo se muestra el motivo.
#[derive(Debug, PartialEq, Eq)]
enum BrowseAction {
    Back,
    Down,
    Up,
    NextPage,
    PreviousPage,
    FirstRow,
    LastRow,
    View,
    AddCondition,
    AddBlocked,
    UndoCondition,
    UndoBlocked,
    Facet,
    Ignore,
}

fn browse_action(event: &KeyEvent, any: bool, filters_len: usize) -> BrowseAction {
    if back(*event) {
        return BrowseAction::Back;
    }
    match event.code {
        KeyCode::Char('+') | KeyCode::Char('a') => {
            if any {
                BrowseAction::AddBlocked
            } else {
                BrowseAction::AddCondition
            }
        }
        KeyCode::Char('-') | KeyCode::Char('z') => {
            if filters_len < 2 {
                BrowseAction::UndoBlocked
            } else {
                BrowseAction::UndoCondition
            }
        }
        KeyCode::Char('t') => BrowseAction::Facet,
        KeyCode::Down => BrowseAction::Down,
        KeyCode::Up => BrowseAction::Up,
        KeyCode::PageDown => BrowseAction::NextPage,
        KeyCode::PageUp => BrowseAction::PreviousPage,
        KeyCode::Home => BrowseAction::FirstRow,
        KeyCode::End => BrowseAction::LastRow,
        KeyCode::Enter | KeyCode::Right => BrowseAction::View,
        _ => BrowseAction::Ignore,
    }
}

fn browse(shell: &mut Shell, filters: Vec<jxcore::Filter>, any: bool) -> Result<()> {
    let page_size = shell.output.limit;
    let session = shell.session()?;
    for filter in &filters {
        session.require_path(&filter.path)?;
    }
    let props = session.props.clone();
    let mut filters = filters;
    let mut results = reload(session, &filters, any, page_size)?;
    loop {
        results.poll_count();
        let height = body_height()?;
        let start = results.selected.saturating_sub(height - 1);
        let rows: Vec<_> = if results.hits.is_empty() {
            vec![Row::text("Sin coincidencias.")]
        } else {
            results
                .hits
                .iter()
                .enumerate()
                .skip(start)
                .take(height)
                .map(|(index, hit)| Row {
                    text: format!(
                        "{} {:>7}  id {:>7}  {}  = {}",
                        if index == results.selected { ">" } else { " " },
                        results.page * page_size + index + 1,
                        hit.id,
                        hit.root_key,
                        hit.value
                    ),
                    selected: index == results.selected,
                })
                .collect()
        };
        let total = if results.count_pending() {
            "…".to_string()
        } else if results.count_failed() {
            "no disponible".to_string()
        } else {
            fmt(results.total)
        };
        draw(
            "Coincidencias",
            &format!(
                "{} · Resultado {} / {} · Página {} · {} filas por página",
                condition_summary(&filters, any),
                fmt(results.current_number()),
                total,
                results.page + 1,
                page_size
            ),
            &rows,
            "↑/↓ recorrer todas · Enter/→ ver objeto · +/a añadir · -/z quitar última · t valores frecuentes · ←/Esc menú · RePág/AvPág cambiar página",
        )?;
        let Some(event) = results_event(&mut results)? else {
            continue;
        };
        let next = match browse_action(&event, any, filters.len()) {
            BrowseAction::Back => break,
            BrowseAction::AddBlocked => {
                document("No se puede acotar", ANY_CONFLICT)?;
                None
            }
            BrowseAction::UndoBlocked => {
                document("Nada que quitar", "solo queda una condición en la búsqueda")?;
                None
            }
            BrowseAction::AddCondition => {
                ask_filter(&props)?.map(|filter| with_extra(&filters, filter))
            }
            BrowseAction::UndoCondition => {
                let mut shortened = filters.clone();
                shortened.pop();
                Some(shortened)
            }
            BrowseAction::Facet => {
                let known_total =
                    (!results.count_pending() && !results.count_failed()).then_some(results.total);
                match facet(&session.index, &props, &filters, any, known_total)? {
                    Some(filter) if !any => Some(with_extra(&filters, filter)),
                    Some(_) => {
                        document("No se puede acotar", ANY_CONFLICT)?;
                        None
                    }
                    None => None,
                }
            }
            other => {
                let moved = match other {
                    BrowseAction::Down => results.down(&session.index),
                    BrowseAction::Up => results.up(&session.index),
                    BrowseAction::NextPage => results.next_page(&session.index).map(|_| ()),
                    BrowseAction::PreviousPage => results.previous_page(&session.index).map(|_| ()),
                    BrowseAction::FirstRow => {
                        results.selected = 0;
                        Ok(())
                    }
                    BrowseAction::LastRow => {
                        results.selected = results.hits.len().saturating_sub(1);
                        Ok(())
                    }
                    BrowseAction::View => match results.hits.get(results.selected) {
                        Some(hit) => viewer(session, hit),
                        None => Ok(()),
                    },
                    _ => Ok(()),
                };
                if let Err(error) = moved {
                    document("Error", &format!("{error:#}"))?;
                }
                None
            }
        };
        if let Some(next) = next {
            match reload(session, &next, any, page_size) {
                Ok(refreshed) => {
                    results = refreshed;
                    filters = next;
                }
                Err(error) => document("Error", &format!("{error:#}"))?,
            }
        }
    }
    session.hits = results.hits;
    Ok(())
}

fn search(shell: &mut Shell, guided: bool) -> Result<()> {
    if !guided {
        if let Some(query) = prompt(
            "Escribir consulta",
            "Ejemplo: edad >= 30 & ciudad = Madrid · ~i ignora mayúsculas y acentos",
        )? {
            let (filters, any) = parse_query(&query)?;
            browse(shell, filters, any)?;
        }
        return Ok(());
    }
    let props = shell.session()?.props.clone();
    if let Some(filter) = ask_filter(&props)? {
        browse(shell, vec![filter], false)?;
    }
    Ok(())
}

fn index(shell: &mut Shell, terminal: &mut Terminal) -> Result<()> {
    shell.json.as_ref().context("abre un JSON primero")?;
    let Some(choice) = choose(
        "Indexar JSON",
        "El JSON original no se modifica. La FTS solo compensa en texto largo: tarda más y ocupa más.",
        &[
            "Todas las propiedades (recomendado)",
            "Todas las propiedades + FTS (texto largo)",
            "Opciones avanzadas",
            "Rehacer índice (sustituye el anterior)",
        ],
    )?
    else {
        return Ok(());
    };
    let command = match choice {
        0 => "index".into(),
        1 => "index --fts".into(),
        2 => {
            let Some(options) = prompt(
                "Opciones de indexación",
                "Ejemplo: --select nombre --select edad --fts --cpus 4 --low-priority --jobs 6",
            )?
            else {
                return Ok(());
            };
            format!("index {options}")
        }
        _ => "index --force".into(),
    };
    if tokenize(&command)?
        .iter()
        .any(|token| token.text == "--force")
        && choose(
            "Rehacer índice",
            "Se sustituirá el índice anterior, no el JSON.",
            &["Cancelar", "Rehacer"],
        )? != Some(1)
    {
        return Ok(());
    }
    terminal.command(shell, &command).map(|_| ())
}

fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
    }
}

struct FileEntry {
    path: PathBuf,
    directory: bool,
}

struct FileBrowser {
    directory: PathBuf,
    entries: Vec<FileEntry>,
    selected: usize,
}

impl FileBrowser {
    fn new(directory: &Path) -> Result<Self> {
        let directory = fs::canonicalize(directory)
            .with_context(|| format!("no puedo abrir la carpeta {}", display_path(directory)))?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            let is_directory = kind.is_dir() || (kind.is_symlink() && path.is_dir());
            if is_directory
                || path.extension().is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("json")
                        || extension.eq_ignore_ascii_case("jindex")
                })
            {
                entries.push(FileEntry {
                    path,
                    directory: is_directory,
                });
            }
        }
        entries.sort_by_cached_key(|entry| {
            (
                !entry.directory,
                entry
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_lowercase(),
            )
        });
        Ok(Self {
            directory,
            entries,
            selected: 0,
        })
    }

    fn enter(&mut self, directory: &Path) -> Result<()> {
        *self = Self::new(directory)?;
        Ok(())
    }

    fn parent(&mut self) -> Result<bool> {
        let Some(parent) = self.directory.parent().map(Path::to_path_buf) else {
            return Ok(false);
        };
        let previous = self.directory.clone();
        self.enter(&parent)?;
        if let Some(index) = self.entries.iter().position(|entry| entry.path == previous) {
            self.selected = index + 3;
        }
        Ok(true)
    }

    fn resolve(&self, input: &str) -> Result<Option<PathBuf>> {
        let input = super::unquote(input.trim())?;
        if input.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.directory.join(input)))
    }

    fn items(&self) -> Vec<String> {
        let mut items = vec![
            "Escribir o pegar una ruta...".into(),
            "[..] Carpeta superior".into(),
            if cfg!(windows) {
                "Elegir unidad...".into()
            } else {
                "Ir a la raíz /".into()
            },
        ];
        items.extend(self.entries.iter().map(|entry| {
            let kind = if entry.directory {
                if jxcore::is_index_dir(&entry.path) {
                    "Índice"
                } else {
                    "Carpeta"
                }
            } else if entry
                .path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jindex"))
            {
                "Antiguo índice"
            } else {
                "JSON"
            };
            format!(
                "[{kind}] {}",
                entry.path.file_name().unwrap().to_string_lossy()
            )
        }));
        items
    }
}

fn choose_root() -> Result<Option<PathBuf>> {
    #[cfg(windows)]
    {
        let roots: Vec<PathBuf> = ('A'..='Z')
            .map(|drive| PathBuf::from(format!("{drive}:\\")))
            .filter(|path| path.is_dir())
            .collect();
        let labels: Vec<String> = roots.iter().map(|path| display_path(path)).collect();
        Ok(
            menu("Elegir unidad", "Selecciona una unidad", &labels, &mut 0)?
                .map(|index| roots[index].clone()),
        )
    }
    #[cfg(not(windows))]
    {
        Ok(Some(PathBuf::from("/")))
    }
}

fn open_source(shell: &mut Shell, terminal: &mut Terminal, path: &Path) -> Result<()> {
    terminal.suspend()?;
    let result = shell.open(path);
    terminal.resume()?;
    result
}

fn open_files(shell: &mut Shell, terminal: &mut Terminal) -> Result<()> {
    let initial = shell
        .json
        .as_deref()
        .and_then(Path::parent)
        .filter(|path| path.is_dir())
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?;
    let mut browser = FileBrowser::new(&initial)?;
    loop {
        let items = browser.items();
        let height = body_height()?;
        let start = browser.selected.saturating_sub(height - 1);
        let rows: Vec<_> = items
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(index, item)| Row {
                text: format!(
                    " {} {item}",
                    if index == browser.selected { ">" } else { " " }
                ),
                selected: index == browser.selected,
            })
            .collect();
        draw(
            "Abrir JSON o índice · explorar carpetas",
            &display_path(&browser.directory),
            &rows,
            "↑/↓ elegir · Enter/→ abrir · ←/Retroceso subir · r ruta · u unidad · Esc cancelar",
        )?;
        let event = key()?;
        if matches!(event.code, KeyCode::Esc | KeyCode::Char('q'))
            || (event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Ok(());
        }
        let mut path = None;
        let result = match event.code {
            KeyCode::Left | KeyCode::Backspace => match browser.parent() {
                Ok(false) => return Ok(()),
                result => result.map(|_| ()),
            },
            KeyCode::Char('r') => {
                if let Some(input) = prompt(
                    "Abrir archivo · escribir ruta",
                    "Ruta completa o relativa a esta carpeta; acepta espacios y comillas",
                )? {
                    match browser.resolve(&input) {
                        Ok(resolved) => {
                            path = resolved;
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    Ok(())
                }
            }
            KeyCode::Char('u') => {
                if let Some(root) = choose_root()? {
                    browser.enter(&root)
                } else {
                    Ok(())
                }
            }
            KeyCode::Enter | KeyCode::Right => match browser.selected {
                0 => {
                    if let Some(input) = prompt(
                        "Abrir archivo · escribir ruta",
                        "Ruta completa o relativa a esta carpeta; acepta espacios y comillas",
                    )? {
                        match browser.resolve(&input) {
                            Ok(resolved) => {
                                path = resolved;
                                Ok(())
                            }
                            Err(error) => Err(error),
                        }
                    } else {
                        Ok(())
                    }
                }
                1 => browser.parent().map(|_| ()),
                2 => {
                    if let Some(root) = choose_root()? {
                        browser.enter(&root)
                    } else {
                        Ok(())
                    }
                }
                index => {
                    path = Some(browser.entries[index - 3].path.clone());
                    Ok(())
                }
            },
            _ => {
                move_selection(&mut browser.selected, items.len(), event.code, height);
                Ok(())
            }
        };
        let result = result.and_then(|_| {
            if let Some(path) = path {
                if path.is_dir() && !jxcore::is_index_dir(&path) {
                    browser.enter(&path)?;
                } else {
                    open_source(shell, terminal, &path)?;
                    return Ok(true);
                }
            }
            Ok(false)
        });
        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if terminal.ready => document("Error", &format!("{error:#}"))?,
            Err(error) => return Err(error),
        }
    }
}

fn open_recent(shell: &mut Shell, terminal: &mut Terminal) -> Result<()> {
    let mut selected = 0;
    loop {
        let paths = super::recent_files()?;
        if paths.is_empty() {
            return document(
                "Abiertos recientemente",
                "Todavía no has abierto ningún archivo.",
            );
        }
        let items: Vec<String> = paths
            .iter()
            .map(|path| {
                format!(
                    "{}{}",
                    if path.is_file() {
                        ""
                    } else {
                        "[No disponible] "
                    },
                    display_path(path)
                )
            })
            .collect();
        selected = selected.min(paths.len() - 1);
        let Some(index) = menu(
            "Abiertos recientemente",
            "Más recientes primero · Enter/→ abrir · Esc volver",
            &items,
            &mut selected,
        )?
        else {
            return Ok(());
        };
        match open_source(shell, terminal, &paths[index]) {
            Ok(()) => return Ok(()),
            Err(error) if terminal.ready => document("Error", &format!("{error:#}"))?,
            Err(error) => return Err(error),
        }
    }
}

fn stored_indices(shell: &mut Shell, terminal: &mut Terminal) -> Result<()> {
    let mut selected = 0;
    loop {
        let entries = jxcore::list_indices()?;
        if entries.is_empty() {
            return document(
                "Índices guardados",
                "Todavía no hay índices guardados.\n\nAbre un JSON y usa Indexar JSON para crear uno.",
            );
        }
        let items: Vec<String> = entries
            .iter()
            .map(|entry| {
                let manifest = &entry.manifest;
                format!(
                    "{}{}{} · {} objetos · {} · {} fragmentos{}",
                    if manifest.status == "complete" {
                        ""
                    } else {
                        "[incompleto] "
                    },
                    if super::outdated(manifest) {
                        "[obsoleto] "
                    } else {
                        ""
                    },
                    display_path(Path::new(&manifest.source)),
                    fmt(manifest.objects),
                    super::bytes(entry.bytes),
                    manifest.shard_count,
                    if manifest.fts { " · FTS" } else { "" }
                )
            })
            .collect();
        selected = selected.min(entries.len() - 1);
        let Some(position) = menu(
            "Índices guardados",
            &format!(
                "{} · Enter/→ elegir · Esc volver{}",
                display_path(&jxcore::indexes_root()),
                if entries.iter().any(|entry| super::outdated(&entry.manifest)) {
                    " · los [obsoleto] solo se pueden rehacer"
                } else {
                    ""
                }
            ),
            &items,
            &mut selected,
        )?
        else {
            return Ok(());
        };
        let entry = &entries[position];
        let source = display_path(Path::new(&entry.manifest.source));
        let Some(action) = choose("Índice guardado", &source, &["Abrir", "Borrar", "Rehacer"])?
        else {
            continue;
        };
        match action {
            0 => return open_source(shell, terminal, &entry.dir),
            1 => {
                if choose(
                    "Borrar índice",
                    "Se borrarán el manifiesto y los fragmentos. El JSON original no se modifica.",
                    &["Cancelar", "Borrar"],
                )? != Some(1)
                {
                    continue;
                }
                if shell
                    .session
                    .as_ref()
                    .is_some_and(|session| session.dir == entry.dir)
                {
                    shell.session = None;
                }
                jxcore::remove_index(&entry.dir)?;
                document(
                    "Índice borrado",
                    &format!(
                        "{}\n\nEl JSON original sigue intacto.",
                        display_path(&entry.dir)
                    ),
                )?;
            }
            _ => {
                let Some(rehacer) = choose(
                    "Rehacer índice",
                    "Se sustituirá el índice actual con la misma configuración.",
                    &["Cancelar", "Rehacer", "Rehacer gastando menos CPU"],
                )?
                else {
                    continue;
                };
                if rehacer == 0 {
                    continue;
                }
                let opciones = if rehacer == 1 {
                    String::new()
                } else {
                    let Some(options) = prompt(
                        "Opciones de rehacer",
                        "Ejemplo: --cpus 4 --low-priority --jobs 2",
                    )?
                    else {
                        continue;
                    };
                    options
                };
                let command = format!("indices rehacer {} {opciones}", position + 1);
                terminal.command(shell, &command)?;
                return Ok(());
            }
        }
    }
}

fn action(shell: &mut Shell, terminal: &mut Terminal, choice: usize) -> Result<bool> {
    match choice {
        0 => open_files(shell, terminal)?,
        1 => open_recent(shell, terminal)?,
        2 => stored_indices(shell, terminal)?,
        3 | 4 => search(shell, choice == 3)?,
        5 => {
            let (filters, any) = shell
                .session()?
                .last
                .clone()
                .context("no hay ninguna búsqueda anterior")?;
            browse(shell, filters, any)?;
        }
        6 => index(shell, terminal)?,
        7 => {
            let text = if let Some(session) = &shell.session {
                session
                    .index
                    .stats()
                    .paths
                    .iter()
                    .map(|(path, count)| format!("{path}    {} apariciones", fmt(*count)))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                let source = shell.json.as_ref().context("abre un JSON primero")?;
                jxcore::discover_props(source, 1000)?
                    .iter()
                    .map(|prop| {
                        format!(
                            "{}    {}    {}",
                            prop.name,
                            fmt(prop.count),
                            prop.types.join(", ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            document("Propiedades", &text)?;
        }
        8 => {
            let session = shell.session()?;
            let stats = session.index.stats();
            document(
                "Estadísticas",
                &format!(
                    "JSON: {}\nObjetos: {}\nEstado: {}\nFTS: {}\nÍndice: {} · {} fragmentos\nCarpeta: {}\nTamaño JSON: {}",
                    stats.json_path.display(),
                    fmt(stats.objects),
                    stats.status,
                    if stats.fts { "sí" } else { "no" },
                    super::bytes(stats.index_bytes),
                    stats.shards,
                    stats.dir.display(),
                    super::bytes(stats.size)
                ),
            )?;
        }
        9 => {
            let session = shell.session()?;
            if let Some(property) = menu(
                "Valores frecuentes",
                "Elige una propiedad",
                &session.props,
                &mut 0,
            )? {
                let text = jxcore::group_by(&session.index, &session.props[property], 100)?
                    .iter()
                    .map(|group| {
                        format!(
                            "{:>10}  {}  [{}]",
                            fmt(group.count),
                            group.value,
                            jxcore::type_name(group.kind)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                document("Valores frecuentes · primeros 100", &text)?;
            }
        }
        10 => {
            terminal.command(shell, "verify")?;
        }
        11 => {
            if let Some(setting) = choose(
                "Ajustes",
                "Se conservan al cambiar de fuente",
                &["Tamaño de página", "Formato de comandos del shell"],
            )? {
                if setting == 0 {
                    if let Some(value) = prompt(
                        "Tamaño de página",
                        "Entre 1 y 10000. Borra la búsqueda anterior.",
                    )? {
                        let limit = value.parse().context("el tamaño no es un número")?;
                        check_limit(limit)?;
                        shell.output.limit = limit;
                        if let Some(session) = &mut shell.session {
                            session.hits.clear();
                            session.last = None;
                        }
                    }
                } else if let Some(format) = choose(
                    "Formato",
                    "Solo afecta a comandos del shell, no al navegador",
                    &["table", "json", "csv", "tsv"],
                )? {
                    shell.output.format = ["table", "json", "csv", "tsv"][format];
                }
            }
        }
        12 => {
            if let Some(line) = prompt(
                "Comandos del shell",
                "Consulta, abrir, index, indices, count, top, id, key, export, formato, limite, ayuda o q",
            )? {
                if super::looks_like_query(&line)
                    || matches!(
                        super::split_first(&line),
                        Some(("s" | "search" | "buscar", _))
                    )
                {
                    let query = if super::looks_like_query(&line) {
                        &line
                    } else {
                        super::split_first(&line).unwrap().1
                    };
                    let (filters, any) = parse_query(query)?;
                    browse(shell, filters, any)?;
                } else {
                    if tokenize(&line)?.iter().any(|token| token.text == "--force")
                        && choose(
                            "Confirmar reconstrucción",
                            "El comando puede sustituir el índice anterior.",
                            &["Cancelar", "Ejecutar"],
                        )? != Some(1)
                    {
                        return Ok(false);
                    }
                    return terminal
                        .command(shell, &line)
                        .map(|next| matches!(next, Next::Quit));
                }
            }
        }
        13 => document(
            "Ayuda",
            "MENÚS\n↑/↓ seleccionan, Enter o → abren, ← o Esc vuelven.\n\nARCHIVOS\nAbrir JSON o índice explora carpetas; ← o Retroceso suben a la carpeta superior.\nPuedes escribir o pegar una ruta, entrecomillada o no, y elegir otra unidad.\nAbiertos recientemente conserva las últimas 20 aperturas entre sesiones.\n\nÍNDICES\nLos índices se guardan en la carpeta de datos de la aplicación, no junto al JSON.\nÍndices guardados permite abrirlos, borrarlos o rehacerlos.\n\nCOINCIDENCIAS\nLas flechas recorren todas las coincidencias, cargando solo una página.\nRePág/AvPág cambian de página; Inicio/Fin seleccionan sus extremos.\nEnter o → abren el objeto; ← vuelve al mismo resultado.\n\nVISOR\n↑/↓ y RePág/AvPág desplazan todo el objeto, sin límite de 200 líneas.\nr alterna JSON formateado/original; e exporta los bytes originales.\n\nCONSULTAS\nedad >= 30 & ciudad = Madrid\nnombre ~i maria\nnota !?\nSepara & y | con espacios; no se pueden mezclar.\n\nINDEXACIÓN\nAbrir un JSON no lo indexa automáticamente. Usa Indexar JSON.\nEl índice se reparte en fragmentos que se crean en paralelo (--jobs).\nFTS acelera búsquedas de texto, pero ocupa más espacio.\n\nTodos los comandos anteriores siguen disponibles en Comandos del shell.\nCon entrada o salida redirigida se usa el shell textual, sin menús.",
        )?,
        _ => return Ok(true),
    }
    Ok(false)
}

pub(super) fn run(file: Option<&Path>) -> Result<()> {
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
    let mut terminal = Terminal::new()?;
    let items = [
        "Abrir JSON o índice",
        "Abiertos recientemente",
        "Índices guardados",
        "Buscar por propiedad",
        "Escribir consulta",
        "Última búsqueda",
        "Indexar JSON",
        "Propiedades",
        "Estadísticas",
        "Valores frecuentes",
        "Verificar índice",
        "Ajustes",
        "Comandos del shell",
        "Ayuda",
        "Salir",
    ]
    .map(String::from);
    let mut selected = 0;
    loop {
        let source = shell
            .json
            .as_ref()
            .map(|path| display_path(path))
            .unwrap_or_else(|| "Sin fuente · abre un JSON o un índice".into());
        let subtitle = format!(
            "{source} · {}",
            if shell.session.is_some() {
                "índice listo"
            } else {
                "sin índice abierto"
            }
        );
        let Some(choice) = menu(
            &format!("jexplorer {}", env!("CARGO_PKG_VERSION")),
            &subtitle,
            &items,
            &mut selected,
        )?
        else {
            break;
        };
        match action(&mut shell, &mut terminal, choice) {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) if terminal.ready => document("Error", &format!("{error:#}"))?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_browser_sorts_directories_and_filters_supported_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b carpeta", "A carpeta"] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        for name in [
            "B.JSON",
            "a.json",
            "z.jindex",
            "ignorar.txt",
            "a.json.jindex-wal",
        ] {
            fs::write(dir.path().join(name), b"{}").unwrap();
        }
        let browser = FileBrowser::new(dir.path()).unwrap();
        let names: Vec<_> = browser
            .entries
            .iter()
            .map(|entry| {
                (
                    entry
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    entry.directory,
                )
            })
            .collect();
        assert_eq!(
            names,
            vec![
                ("A carpeta".into(), true),
                ("b carpeta".into(), true),
                ("a.json".into(), false),
                ("B.JSON".into(), false),
                ("z.jindex".into(), false),
            ]
        );
        assert_eq!(browser.items().len(), 8);
    }

    #[test]
    fn file_browser_handles_paths_parent_and_failed_navigation() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("carpeta con é");
        fs::create_dir(&child).unwrap();
        let mut browser = FileBrowser::new(dir.path()).unwrap();
        assert_eq!(
            browser.resolve("\"archivo con é.json\"").unwrap(),
            Some(browser.directory.join("archivo con é.json"))
        );
        assert_eq!(browser.resolve("  ").unwrap(), None);
        assert!(browser.resolve("\"sin cierre").is_err());
        browser.selected = 3;
        let directory = browser.directory.clone();
        assert!(browser.enter(&dir.path().join("inexistente")).is_err());
        assert_eq!(browser.directory, directory);
        assert_eq!(browser.selected, 3);
        browser.enter(&child).unwrap();
        assert!(browser.entries.is_empty());
        assert!(browser.parent().unwrap());
        assert_eq!(browser.directory, directory);
        assert_eq!(browser.selected, 3);
        let resolved = browser.resolve(&display_path(&child)).unwrap().unwrap();
        assert_eq!(
            fs::canonicalize(resolved).unwrap(),
            fs::canonicalize(child).unwrap()
        );
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Datos\fichero.json")),
            r"C:\Datos\fichero.json"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\servidor\datos\fichero.json")),
            r"\\servidor\datos\fichero.json"
        );
    }

    #[test]
    fn terminal_controls_and_direction_overrides_are_removed() {
        assert_eq!(clean("a\x1b[31m\n\t\u{202e}b"), "a [31m   b");
        assert_eq!(clip("a界bc", 3), "a界");
        assert_eq!(clip("éx", 1), "é");
    }

    #[test]
    fn resume_condiciones_sin_pasarse_del_ancho() {
        let filters = vec![
            jxcore::Filter::new("edad", jxcore::Op::Gt, Some("30".to_string()), false),
            jxcore::Filter::new("ciudad", jxcore::Op::Eq, Some("Madrid".to_string()), true),
        ];
        assert_eq!(
            condition_summary(&filters, false),
            "edad > 30 & ciudad =i Madrid"
        );
        assert_eq!(
            condition_summary(&filters, true),
            "edad > 30 | ciudad =i Madrid"
        );
        let repetidas: Vec<jxcore::Filter> = (0..20).map(|_| filters[0].clone()).collect();
        assert_eq!(condition_summary(&repetidas, false).chars().count(), 48);
    }

    fn pressed(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn teclas_de_coincidencias_se_traducen_en_acciones() {
        for code in [KeyCode::Char('+'), KeyCode::Char('a')] {
            assert_eq!(
                browse_action(&pressed(code), false, 1),
                BrowseAction::AddCondition
            );
        }
        assert_eq!(
            browse_action(&pressed(KeyCode::Char('-')), false, 2),
            BrowseAction::UndoCondition
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Char('t')), false, 1),
            BrowseAction::Facet
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Down), false, 1),
            BrowseAction::Down
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Up), false, 1),
            BrowseAction::Up
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::PageDown), false, 1),
            BrowseAction::NextPage
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::PageUp), false, 1),
            BrowseAction::PreviousPage
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Home), false, 1),
            BrowseAction::FirstRow
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::End), false, 1),
            BrowseAction::LastRow
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Enter), false, 1),
            BrowseAction::View
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Right), false, 1),
            BrowseAction::View
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Null), false, 1),
            BrowseAction::Ignore
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Char('x')), false, 1),
            BrowseAction::Ignore
        );
    }

    #[test]
    fn acciones_que_no_tienen_sentido_se_rechazan_sin_mover_nada() {
        // una OR no se puede acotar; ver los valores de todas formas es útil
        assert_eq!(
            browse_action(&pressed(KeyCode::Char('+')), true, 1),
            BrowseAction::AddBlocked
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Char('a')), true, 3),
            BrowseAction::AddBlocked
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Char('t')), true, 3),
            BrowseAction::Facet
        );
        // con una sola condición no hay nada que quitar
        assert_eq!(
            browse_action(&pressed(KeyCode::Char('-')), false, 1),
            BrowseAction::UndoBlocked
        );
        assert_eq!(
            browse_action(&pressed(KeyCode::Char('z')), false, 0),
            BrowseAction::UndoBlocked
        );
    }

    #[test]
    fn cualquier_ruta_de_salida_vuelve_al_menu() {
        for code in [KeyCode::Esc, KeyCode::Left, KeyCode::Char('q')] {
            assert_eq!(browse_action(&pressed(code), true, 2), BrowseAction::Back);
        }
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(browse_action(&ctrl_c, false, 1), BrowseAction::Back);
    }

    #[test]
    fn anadir_una_condicion_no_toca_la_anterior() {
        let base = vec![jxcore::Filter::new(
            "edad",
            jxcore::Op::Gt,
            Some("30".to_string()),
            false,
        )];
        let nueva =
            jxcore::Filter::new("ciudad", jxcore::Op::Eq, Some("Madrid".to_string()), false);
        let extended = with_extra(&base, nueva);
        assert_eq!(base.len(), 1);
        assert_eq!(extended.len(), 2);
        assert_eq!(
            condition_summary(&extended, false),
            "edad > 30 & ciudad = Madrid"
        );
    }

    #[test]
    fn lines_wrap_without_losing_unicode_or_blank_lines() {
        assert_eq!(wrap("ab界c\n\nfin", 3), vec!["ab", "界c", "", "fin"]);
        assert_eq!(wrap("", 0), vec![""]);
        assert_eq!(wrap("a\x1bb", 2), vec!["a ", "b"]);
    }

    #[test]
    fn navigation_stays_within_bounds_even_in_tiny_terminals() {
        let mut selected = 0;
        move_selection(&mut selected, 0, KeyCode::End, 1);
        assert_eq!(selected, 0);
        move_selection(&mut selected, 3, KeyCode::Up, 1);
        assert_eq!(selected, 0);
        move_selection(&mut selected, 3, KeyCode::PageDown, 50);
        assert_eq!(selected, 2);
        move_selection(&mut selected, 3, KeyCode::PageUp, 50);
        assert_eq!(selected, 0);
    }

    #[test]
    fn input_uses_available_width_and_keeps_the_cursor_visible() {
        let mut input = Input {
            chars: Vec::new(),
            cursor: 0,
        };
        input.insert("C:\\datos con espacios\\fichero.json");
        assert_eq!(input.visible(80), "> C:\\datos con espacios\\fichero.json│");
        assert_eq!(input.visible(8), "> ro.json│");
        input.edit(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert!(input.visible(8).starts_with("> │C:"));
        assert_eq!(
            Input {
                chars: vec!['界', 'a'],
                cursor: 2
            }
            .visible(4),
            "> 界a│"
        );
    }

    #[test]
    fn prompt_edits_unicode_and_preserves_windows_paths() {
        let mut input = Input {
            chars: Vec::new(),
            cursor: 0,
        };
        input.insert("C:\\datos\\é.json");
        input.edit(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        input.edit(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        input.edit(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
        input.edit(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        input.edit(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(input.chars.iter().collect::<String>(), "D:\\datos\\é.jso");
        input.insert("\x1b\n");
        assert!(input.chars.iter().all(|ch| !ch.is_control()));
    }

    #[test]
    fn prompt_accepts_altgr_characters_and_preserves_shortcuts() {
        let mut input = Input {
            chars: Vec::new(),
            cursor: 0,
        };
        for ch in "C:\\datos con espacios\\é.json".chars() {
            let modifiers = if ch == '\\' {
                KeyModifiers::CONTROL | KeyModifiers::ALT
            } else {
                KeyModifiers::NONE
            };
            input.edit(KeyEvent::new(KeyCode::Char(ch), modifiers));
        }
        assert_eq!(
            input.chars.iter().collect::<String>(),
            "C:\\datos con espacios\\é.json"
        );
        for ch in "@|{}[]€c".chars() {
            input.edit(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            ));
        }
        assert_eq!(
            input.chars.iter().collect::<String>(),
            "C:\\datos con espacios\\é.json@|{}[]€c"
        );
        let before = input.chars.clone();
        let cursor = input.cursor;
        for event in [
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
            KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
        ] {
            input.edit(event);
            assert_eq!(input.chars, before);
            assert_eq!(input.cursor, cursor);
        }
    }
}
