use std::path::PathBuf;

use clap::Parser;

mod shell;

#[derive(Parser)]
#[command(
    name = "jexplorer",
    version,
    about = "Explora JSON gigantes con menús y flechas, sin cargar todo el documento",
    after_help = "Sin fichero, abre el menú vacío. Enter/→ abren y ←/Esc vuelven. Con entrada o salida redirigida, usa el shell textual: `abrir <fichero>` y `ayuda`."
)]
struct Cli {
    /// JSON o índice .jindex que se abrirá en el shell
    file: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = shell::run(cli.file.as_deref()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

pub(crate) fn fmt(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.bytes().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c as char);
    }
    out
}

pub(crate) fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 3 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

pub(crate) fn trim(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}
