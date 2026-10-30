pub mod index;
pub mod parser;
pub mod reader;
pub mod search;
pub mod text;

pub use index::{
    Freshness, Group, IndexOptions, Progress, PropInfo, Stats, Summary, VerifyReport, build_index,
    check_source, db_path_for, discover_props, group_by, index_json_path, object_span,
    object_span_by_key, open_checked, open_db, open_db_ro, stats, type_name, verify,
};
pub use reader::{pretty, read_object};
pub use search::{Filter, Hit, Op, SearchQuery, count, search};
pub use text::fold;
