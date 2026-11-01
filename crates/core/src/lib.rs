pub mod index;
pub mod parser;
pub mod priority;
pub mod reader;
pub mod search;
pub mod store;
pub mod text;

use std::sync::Once;

static SQLITE_TUNED: Once = Once::new();

/// Desactiva las estadísticas de memoria de SQLite antes de inicializarlo.
/// Sin esto, cada reserva y liberación de páginas pasa por un mutex global que
/// serializa los fragmentos que se construyen en paralelo.
pub fn init_sqlite() {
    SQLITE_TUNED.call_once(|| {
        #[allow(unsafe_code)]
        fn disable_memstat() {
            // SAFETY: solo toca configuración global previa a sqlite3_initialize;
            // si SQLite ya estaba inicializado devuelve SQLITE_MISUSE sin efecto.
            unsafe {
                rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0i32);
            }
        }
        disable_memstat();
    });
}

pub use index::{
    FORMAT_VERSION, Freshness, Index, IndexOptions, Phase, Progress, PropInfo, Stats, Summary,
    VerifyReport, auto_jobs, available_cores, build_index, discover_props, open_db, open_db_ro,
    type_name,
};
pub use reader::{pretty, read_object};
pub use search::{Filter, Group, Hit, Op, SearchQuery, count, group_by, group_matching, search};
pub use store::{
    Manifest, ShardInfo, StoredIndex, data_root, dir_for_json, find_for_json, indexes_root,
    is_index_dir, list_indices, read_manifest, remove_index,
};
pub use text::fold;
