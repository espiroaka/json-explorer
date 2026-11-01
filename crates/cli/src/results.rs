use anyhow::{Result, ensure};
use jxcore::{Filter, Hit, Index, SearchQuery, count, search};

pub(super) struct Results {
    pub hits: Vec<Hit>,
    pub selected: usize,
    pub page: usize,
    pub total: u64,
    filters: Vec<Filter>,
    any: bool,
    page_size: usize,
    cursors: Vec<Option<i64>>,
}

impl Results {
    pub fn new(index: &Index, filters: Vec<Filter>, any: bool, page_size: usize) -> Result<Self> {
        ensure!(
            (1..=10_000).contains(&page_size),
            "el tamaño de página debe estar entre 1 y 10000"
        );
        let query = SearchQuery {
            filters,
            any,
            limit: page_size,
            after: None,
        };
        let total = count(index, &query)?;
        let hits = search(index, &query)?;
        Ok(Self {
            hits,
            selected: 0,
            page: 0,
            total,
            filters: query.filters,
            any,
            page_size,
            // None permite incluir el ID 0 al regresar a la primera página.
            cursors: vec![None],
        })
    }

    pub fn down(&mut self, index: &Index) -> Result<()> {
        if self.hits.is_empty() {
            return Ok(());
        }
        if self.selected + 1 < self.hits.len() {
            self.selected += 1;
        } else {
            self.next_page(index)?;
        }
        Ok(())
    }

    pub fn up(&mut self, index: &Index) -> Result<()> {
        if self.hits.is_empty() {
            return Ok(());
        }
        if self.selected > 0 {
            self.selected -= 1;
        } else if self.previous_page(index)? {
            self.selected = self.hits.len() - 1;
        }
        Ok(())
    }

    pub fn next_page(&mut self, index: &Index) -> Result<bool> {
        let Some(last) = self.hits.last() else {
            return Ok(false);
        };
        let page = self.page + 1;
        let after = self.cursors.get(page).copied().unwrap_or(Some(last.id));
        let hits = self.load(index, after)?;
        if hits.is_empty() {
            return Ok(false);
        }
        if page == self.cursors.len() {
            self.cursors.push(after);
        }
        self.hits = hits;
        self.page = page;
        self.selected = 0;
        Ok(true)
    }

    pub fn previous_page(&mut self, index: &Index) -> Result<bool> {
        let Some(page) = self.page.checked_sub(1) else {
            return Ok(false);
        };
        let hits = self.load(index, self.cursors[page])?;
        if hits.is_empty() {
            return Ok(false);
        }
        self.hits = hits;
        self.page = page;
        self.selected = 0;
        Ok(true)
    }

    pub fn current_number(&self) -> u64 {
        if self.hits.is_empty() {
            0
        } else {
            self.page as u64 * self.page_size as u64 + self.selected as u64 + 1
        }
    }

    fn load(&self, index: &Index, after: Option<i64>) -> Result<Vec<Hit>> {
        search(
            index,
            &SearchQuery {
                filters: self.filters.clone(),
                any: self.any,
                limit: self.page_size,
                after,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use jxcore::{IndexOptions, Op};
    use tempfile::TempDir;

    use super::*;

    fn objects(size: usize) -> serde_json::Map<String, serde_json::Value> {
        let mut objects = serde_json::Map::new();
        for id in 0..size {
            objects.insert(
                format!("item{id:08}"),
                serde_json::json!({ "value": id, "even": id % 2 == 0 }),
            );
        }
        objects
    }

    fn build(json: &Path, dir: &Path, select: &[&str]) -> Index {
        jxcore::build_index(
            json,
            dir,
            &IndexOptions {
                select: select.iter().map(|name| name.to_string()).collect(),
                all: false,
                fts: false,
                batch: 256,
                jobs: 8,
                force: false,
            },
            |_| {},
        )
        .unwrap();
        Index::open(dir).unwrap()
    }

    fn indexed(size: usize) -> (TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("data.json");
        fs::write(&json, serde_json::to_vec(&objects(size)).unwrap()).unwrap();
        let index = build(&json, &dir.path().join("indice"), &["value", "even"]);
        (dir, index)
    }

    fn broken(size: usize) -> (TempDir, Index) {
        let (dir, index) = indexed(size);
        let path = index.dir.clone();
        let shard = path.join(&index.manifest.shards[0].file);
        drop(index);
        fs::remove_file(&shard).unwrap();
        drop(jxcore::open_db(&shard).unwrap());
        (dir, Index::open(&path).unwrap())
    }

    fn filters() -> Vec<Filter> {
        vec![Filter::new("value", Op::Exists, None::<String>, false)]
    }

    fn ids(results: &Results) -> Vec<i64> {
        results.hits.iter().map(|hit| hit.id).collect()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Snapshot {
        hits: Vec<(i64, String, String, i64, i64, i64)>,
        selected: usize,
        page: usize,
        total: u64,
        cursors: Vec<Option<i64>>,
    }

    fn snapshot(results: &Results) -> Snapshot {
        Snapshot {
            hits: results
                .hits
                .iter()
                .map(|hit| {
                    (
                        hit.id,
                        hit.root_key.clone(),
                        hit.value.clone(),
                        hit.kind,
                        hit.offset,
                        hit.length,
                    )
                })
                .collect(),
            selected: results.selected,
            page: results.page,
            total: results.total,
            cursors: results.cursors.clone(),
        }
    }

    #[test]
    fn single_row_pages_preserve_id_zero_and_its_cursor() {
        let (_dir, index) = indexed(3);
        let mut results = Results::new(&index, filters(), false, 1).unwrap();
        assert_eq!(ids(&results), vec![0]);
        assert_eq!(results.current_number(), 1);
        assert_eq!(results.cursors, vec![None]);
        results.down(&index).unwrap();
        assert_eq!(ids(&results), vec![1]);
        assert_eq!(results.cursors, vec![None, Some(0)]);
        results.up(&index).unwrap();
        assert_eq!(ids(&results), vec![0]);
        assert_eq!(results.page, 0);
        assert_eq!(results.selected, 0);
        assert!(results.next_page(&index).unwrap());
        assert_eq!(ids(&results), vec![1]);
        assert!(results.next_page(&index).unwrap());
        assert_eq!(ids(&results), vec![2]);
        assert_eq!(results.cursors, vec![None, Some(0), Some(1)]);
        assert!(!results.next_page(&index).unwrap());
        assert!(results.previous_page(&index).unwrap());
        assert!(results.previous_page(&index).unwrap());
        assert_eq!(ids(&results), vec![0]);
    }

    #[test]
    fn arrows_cross_pages_in_both_directions() {
        let (_dir, index) = indexed(7);
        let mut results = Results::new(&index, filters(), false, 3).unwrap();
        for id in 0..7 {
            assert_eq!(results.hits[results.selected].id, id);
            assert_eq!(results.page, id as usize / 3);
            assert_eq!(results.selected, id as usize % 3);
            assert_eq!(results.current_number(), id as u64 + 1);
            results.down(&index).unwrap();
        }
        assert_eq!(results.current_number(), 7);
        for id in (0..7).rev() {
            assert_eq!(results.hits[results.selected].id, id);
            assert_eq!(results.page, id as usize / 3);
            assert_eq!(results.selected, id as usize % 3);
            assert_eq!(results.current_number(), id as u64 + 1);
            results.up(&index).unwrap();
        }
        assert_eq!(results.current_number(), 1);
        assert_eq!(results.selected, 0);
    }

    #[test]
    fn page_keys_select_first_row_and_reuse_visited_cursors() {
        let (_dir, index) = indexed(8);
        let mut results = Results::new(&index, filters(), false, 3).unwrap();
        results.down(&index).unwrap();
        assert!(results.next_page(&index).unwrap());
        assert_eq!(ids(&results), vec![3, 4, 5]);
        assert_eq!(results.selected, 0);
        results.down(&index).unwrap();
        assert!(results.next_page(&index).unwrap());
        assert_eq!(ids(&results), vec![6, 7]);
        assert_eq!(results.selected, 0);
        results.down(&index).unwrap();
        assert!(results.previous_page(&index).unwrap());
        assert_eq!(ids(&results), vec![3, 4, 5]);
        assert_eq!(results.selected, 0);
        let cursors = results.cursors.clone();
        results.down(&index).unwrap();
        assert!(results.previous_page(&index).unwrap());
        assert_eq!(ids(&results), vec![0, 1, 2]);
        assert_eq!(results.selected, 0);
        assert!(results.next_page(&index).unwrap());
        assert_eq!(ids(&results), vec![3, 4, 5]);
        assert!(results.next_page(&index).unwrap());
        assert_eq!(ids(&results), vec![6, 7]);
        assert_eq!(results.selected, 0);
        assert_eq!(results.cursors, cursors);
    }

    #[test]
    fn empty_index_and_unmatched_filters_stay_empty() {
        for size in [0, 3] {
            let (_dir, index) = indexed(size);
            let mut results = Results::new(
                &index,
                vec![Filter::new("value", Op::Eq, Some("missing"), false)],
                false,
                2,
            )
            .unwrap();
            assert!(results.hits.is_empty());
            assert_eq!(results.total, 0);
            assert_eq!(results.current_number(), 0);
            let before = snapshot(&results);
            results.down(&index).unwrap();
            results.up(&index).unwrap();
            assert!(!results.next_page(&index).unwrap());
            assert!(!results.previous_page(&index).unwrap());
            assert_eq!(snapshot(&results), before);
        }
    }

    #[test]
    fn exact_and_partial_ends_preserve_page_selection_and_cursors() {
        for size in [1, 2, 3, 4, 6, 7, 8, 9] {
            let (_dir, index) = indexed(size);
            let mut results = Results::new(&index, filters(), false, 3).unwrap();
            assert_eq!(results.total, size as u64);
            while results.next_page(&index).unwrap() {
                assert_eq!(results.selected, 0);
                assert!(results.hits.len() <= 3);
            }
            let last_page = (size - 1) / 3;
            assert_eq!(results.page, last_page);
            assert_eq!(results.cursors.len(), last_page + 1);
            assert_eq!(
                ids(&results),
                ((last_page * 3) as i64..size as i64).collect::<Vec<_>>()
            );
            results.selected = results.hits.len() - 1;
            assert_eq!(results.current_number(), size as u64);
            let before = snapshot(&results);
            assert!(!results.next_page(&index).unwrap());
            results.down(&index).unwrap();
            assert!(!results.next_page(&index).unwrap());
            assert_eq!(snapshot(&results), before);
            while results.previous_page(&index).unwrap() {
                assert_eq!(results.selected, 0);
            }
            results.selected = results.hits.len() - 1;
            let before = snapshot(&results);
            assert!(!results.previous_page(&index).unwrap());
            assert_eq!(snapshot(&results), before);
            results.selected = 0;
            let before = snapshot(&results);
            results.up(&index).unwrap();
            assert_eq!(snapshot(&results), before);
        }
    }

    #[test]
    fn sql_failures_do_not_change_hits_selection_page_or_cursors() {
        let (_dir, index) = indexed(8);
        let (_broken_dir, broken) = broken(8);
        let mut results = Results::new(&index, filters(), false, 3).unwrap();
        results.selected = 2;
        let before = snapshot(&results);
        assert!(results.next_page(&broken).is_err());
        assert_eq!(snapshot(&results), before);
        assert!(results.down(&broken).is_err());
        assert_eq!(snapshot(&results), before);
        assert!(results.next_page(&index).unwrap());
        let before = snapshot(&results);
        assert!(results.previous_page(&broken).is_err());
        assert_eq!(snapshot(&results), before);
        assert!(results.up(&broken).is_err());
        assert_eq!(snapshot(&results), before);
        assert!(results.next_page(&index).unwrap());
        assert!(results.previous_page(&index).unwrap());
        results.selected = 2;
        let before = snapshot(&results);
        assert!(results.next_page(&broken).is_err());
        assert_eq!(snapshot(&results), before);
        assert!(results.down(&broken).is_err());
        assert_eq!(snapshot(&results), before);
        assert!(results.next_page(&index).unwrap());
        assert_eq!(ids(&results), vec![6, 7]);
        assert_eq!(results.selected, 0);
    }

    #[test]
    fn failure_while_reading_a_page_does_not_commit_partial_hits() {
        let (_dir, index) = indexed(6);
        let mut results = Results::new(&index, filters(), false, 3).unwrap();
        results.selected = 2;
        let dir = index.dir.clone();
        let position = 4 % index.manifest.shards.len();
        let shard = dir.join(&index.manifest.shards[position].file);
        drop(index);
        {
            let conn = rusqlite::Connection::open(&shard).unwrap();
            assert_eq!(
                conn.execute(
                    "UPDATE objects SET byte_length = 'invalid' WHERE id = 4",
                    []
                )
                .unwrap(),
                1
            );
        }
        let index = Index::open(&dir).unwrap();
        let before = snapshot(&results);
        assert!(results.next_page(&index).is_err());
        assert_eq!(snapshot(&results), before);
        assert!(results.down(&index).is_err());
        assert_eq!(snapshot(&results), before);
    }

    #[test]
    fn page_size_is_validated_without_clamping() {
        let (_dir, index) = indexed(1);
        for page_size in [0, 10_001, usize::MAX] {
            let error = Results::new(&index, filters(), false, page_size)
                .err()
                .unwrap();
            assert!(error.to_string().contains("entre 1 y 10000"));
        }
        for page_size in [1, 10_000] {
            let results = Results::new(&index, filters(), false, page_size).unwrap();
            assert_eq!(results.page_size, page_size);
            assert_eq!(ids(&results), vec![0]);
            assert_eq!(results.total, 1);
        }
        let (_broken_dir, broken) = broken(1);
        assert!(Results::new(&broken, filters(), false, 1).is_err());
        assert!(Results::new(&index, Vec::new(), false, 1).is_err());
    }

    #[test]
    fn sparse_ids_and_any_filters_use_id_cursors_not_row_numbers() {
        let (_dir, index) = indexed(10);
        let make_filters = || {
            vec![
                Filter::new("even", Op::Eq, Some("true"), false),
                Filter::new("value", Op::Eq, Some("5"), false),
            ]
        };
        let mut results = Results::new(&index, make_filters(), true, 2).unwrap();
        assert_eq!(results.total, 6);
        assert_eq!(ids(&results), vec![0, 2]);
        assert!(results.next_page(&index).unwrap());
        assert_eq!(ids(&results), vec![4, 5]);
        assert_eq!(results.current_number(), 3);
        assert!(results.next_page(&index).unwrap());
        assert_eq!(ids(&results), vec![6, 8]);
        assert_eq!(results.current_number(), 5);
        assert_eq!(results.cursors, vec![None, Some(2), Some(5)]);
        assert!(!results.next_page(&index).unwrap());
        assert!(results.previous_page(&index).unwrap());
        assert_eq!(ids(&results), vec![4, 5]);
        assert!(results.previous_page(&index).unwrap());
        assert_eq!(ids(&results), vec![0, 2]);
        let intersection = Results::new(&index, make_filters(), false, 2).unwrap();
        assert_eq!(intersection.total, 0);
        assert!(intersection.hits.is_empty());
    }

    #[test]
    fn visited_pages_are_refetched_in_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("data.json");
        fs::write(&json, serde_json::to_vec(&objects(4)).unwrap()).unwrap();
        let first = build(&json, &dir.path().join("indice1"), &["value", "even"]);
        let mut results = Results::new(&first, filters(), false, 2).unwrap();
        assert_eq!(results.hits[0].value, "0");
        assert!(results.next_page(&first).unwrap());
        assert_eq!(results.hits[0].value, "2");
        let mut updated = objects(4);
        for id in [0, 2] {
            updated.insert(
                format!("item{id:08}"),
                serde_json::json!({ "value": "updated", "even": id % 2 == 0 }),
            );
        }
        fs::write(&json, serde_json::to_vec(&updated).unwrap()).unwrap();
        let second = build(&json, &dir.path().join("indice2"), &["value", "even"]);
        assert!(results.previous_page(&second).unwrap());
        assert_eq!(ids(&results), vec![0, 1]);
        assert_eq!(results.hits[0].value, "updated");
        assert!(results.next_page(&second).unwrap());
        assert_eq!(ids(&results), vec![2, 3]);
        assert_eq!(results.hits[0].value, "updated");
        assert_eq!(results.cursors, vec![None, Some(1)]);
    }

    #[test]
    fn many_results_keep_only_one_page_of_hits() {
        let size = 10_037;
        let page_size = 37;
        let (_dir, index) = indexed(size);
        let mut results = Results::new(&index, filters(), false, page_size).unwrap();
        assert_eq!(results.total, size as u64);
        loop {
            assert!(results.hits.len() <= page_size);
            assert_eq!(results.selected, 0);
            assert_eq!(results.cursors.len(), results.page + 1);
            for (row, hit) in results.hits.iter().enumerate() {
                assert_eq!(hit.id, (results.page * page_size + row) as i64);
            }
            if !results.next_page(&index).unwrap() {
                break;
            }
        }
        let visited = results.cursors.len();
        assert_eq!(visited, size.div_ceil(page_size));
        assert_eq!(results.hits.len(), size % page_size);
        while results.previous_page(&index).unwrap() {
            assert!(results.hits.len() <= page_size);
            assert_eq!(results.hits[0].id, (results.page * page_size) as i64);
            assert_eq!(results.cursors.len(), visited);
        }
        assert_eq!(results.hits[0].id, 0);
        while results.next_page(&index).unwrap() {
            assert!(results.hits.len() <= page_size);
            assert_eq!(results.hits[0].id, (results.page * page_size) as i64);
            assert_eq!(results.cursors.len(), visited);
        }
    }
}
