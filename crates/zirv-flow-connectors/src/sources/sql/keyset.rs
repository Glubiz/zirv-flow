//! Keyset paging shared by both database sources: which columns order a stream, the checkpoint
//! that resumes it, the record key, and the loop that turns pages into batches.
//!
//! A stream is read in pages of [`BATCH_ROWS`] rows ordered by `(cursor, primary key)`
//! (incremental) or by `(primary key)` (full refresh). The checkpoint is the key of the last row
//! read, in the text form the database printed. Because the key is unique and totally ordered,
//! rows that share a cursor value across a page boundary are neither skipped nor read twice.

use std::future::Future;

use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};
use serde_json::{Map, Value, json};

use super::error::NULL_KEY;
use super::settings::config_error;
use crate::sources::{Batch, Checkpoint, Record, SourceError, StreamConfig, SyncMode};

/// The most rows one batch, and one short read-only transaction, holds.
pub(crate) const BATCH_ROWS: usize = 500;

/// One row, every column as the database printed it; `None` is SQL `NULL`.
pub(crate) type Row = Vec<Option<String>>;

/// The columns a stream is ordered and resumed by.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KeySpec {
    /// The cursor column of an incremental stream.
    pub(crate) cursor: Option<String>,
    pub(crate) primary_key: Vec<String>,
}

impl KeySpec {
    /// A stream needs a primary key to be read (it cannot be resumed without one), and an
    /// incremental stream a cursor too.
    pub(crate) fn from_stream(stream: &StreamConfig) -> Result<Self, SourceError> {
        if stream.primary_key.is_empty() {
            return Err(config_error("primary_key", "is required"));
        }
        let cursor = match stream.sync_mode {
            SyncMode::FullRefresh => None,
            SyncMode::Incremental => Some(
                stream
                    .cursor_field
                    .clone()
                    .ok_or_else(|| config_error("cursor_field", "is required"))?,
            ),
        };
        Ok(Self {
            cursor,
            primary_key: stream.primary_key.clone(),
        })
    }

    pub(crate) fn width(&self) -> usize {
        usize::from(self.cursor.is_some()) + self.primary_key.len()
    }
}

/// The checkpoint's key values, or `None` when the stream starts from the beginning. A checkpoint
/// whose shape does not fit the stream (it was written for other settings) is an error rather
/// than a silent restart.
pub(crate) fn parse_checkpoint(
    checkpoint: Option<&Checkpoint>,
    spec: &KeySpec,
) -> Result<Option<Vec<String>>, SourceError> {
    let Some(checkpoint) = checkpoint else {
        return Ok(None);
    };
    let wrong = || config_error("checkpoint", "does not fit the stream settings");
    let object = checkpoint.0.as_object().ok_or_else(wrong)?;
    if object.get("v").and_then(Value::as_u64) != Some(1) {
        return Err(wrong());
    }
    let key = object
        .get("key")
        .and_then(Value::as_array)
        .ok_or_else(wrong)?;
    let key: Vec<String> = key
        .iter()
        .map(|value| value.as_str().map(str::to_owned).ok_or_else(wrong))
        .collect::<Result<_, _>>()?;
    if key.len() != spec.width() {
        return Err(wrong());
    }
    Ok(Some(key))
}

pub(crate) fn checkpoint_of(key: &[String]) -> Checkpoint {
    Checkpoint(json!({"v": 1, "key": key}))
}

/// The record key: a single primary-key value as is, several as a JSON array text.
pub(crate) fn record_key(values: &[String]) -> String {
    match values {
        [single] => single.clone(),
        many => Value::from(many.to_vec()).to_string(),
    }
}

/// Fetches pages of a stream, ordered as [`KeySpec`] says.
pub(crate) trait Pager: Send {
    /// Up to [`BATCH_ROWS`] rows after the key `after` (from the start when `None`), every
    /// column in the order the stream's columns were given to [`Paged::new`].
    fn page(
        &mut self,
        after: Option<&[String]>,
    ) -> impl Future<Output = Result<Vec<Row>, SourceError>> + Send;
}

/// Turns pages into batches with their checkpoints.
pub(crate) struct Paged<P> {
    pager: P,
    columns: Vec<String>,
    /// Where each key column sits in a row, in [`KeySpec::columns`] order.
    key_positions: Vec<usize>,
    /// How many of the key columns are the primary key (the last ones).
    primary_key_len: usize,
    after: Option<Vec<String>>,
    done: bool,
}

impl<P: Pager> Paged<P> {
    /// `columns` are the columns a page row carries, in order. Every key column must be one.
    pub(crate) fn new(
        pager: P,
        columns: Vec<String>,
        spec: &KeySpec,
        after: Option<Vec<String>>,
    ) -> Result<Self, SourceError> {
        let position = |name: &str, setting: &str| {
            columns
                .iter()
                .position(|column| column == name)
                .ok_or_else(|| config_error(setting, "names a column the stream does not have"))
        };
        let mut key_positions = Vec::with_capacity(spec.width());
        if let Some(cursor) = &spec.cursor {
            key_positions.push(position(cursor, "cursor_field")?);
        }
        for name in &spec.primary_key {
            key_positions.push(position(name, "primary_key")?);
        }
        Ok(Self {
            pager,
            columns,
            key_positions,
            primary_key_len: spec.primary_key.len(),
            after,
            done: false,
        })
    }

    /// The next batch, or `None` when the stream is exhausted. A page shorter than
    /// [`BATCH_ROWS`] is the last, so a stream whose size is a multiple of a page costs one empty
    /// query more.
    pub(crate) async fn next_batch(&mut self) -> Result<Option<Batch>, SourceError> {
        if self.done {
            return Ok(None);
        }
        let rows = self.pager.page(self.after.as_deref()).await?;
        if rows.len() < BATCH_ROWS {
            self.done = true;
        }
        let Some(last) = rows.last() else {
            return Ok(None);
        };
        let key = self.key_of(last)?;
        let first_primary = self.key_positions.len() - self.primary_key_len;
        let mut records = Vec::with_capacity(rows.len());
        for row in &rows {
            let primary: Vec<String> = self.key_positions[first_primary..]
                .iter()
                .map(|position| {
                    row[*position]
                        .clone()
                        .ok_or_else(|| SourceError::Other(NULL_KEY.to_owned()))
                })
                .collect::<Result<_, _>>()?;
            records.push(Record {
                key: record_key(&primary),
                data: self.data_of(row),
                deleted: false,
            });
        }
        let checkpoint = checkpoint_of(&key);
        self.after = Some(key);
        Ok(Some(Batch {
            records,
            checkpoint,
        }))
    }

    fn key_of(&self, row: &Row) -> Result<Vec<String>, SourceError> {
        self.key_positions
            .iter()
            .map(|position| {
                row[*position]
                    .clone()
                    .ok_or_else(|| SourceError::Other(NULL_KEY.to_owned()))
            })
            .collect()
    }

    fn data_of(&self, row: &Row) -> Value {
        let mut object = Map::with_capacity(self.columns.len());
        for (name, value) in self.columns.iter().zip(row) {
            object.insert(
                name.clone(),
                value.clone().map_or(Value::Null, Value::String),
            );
        }
        Value::Object(object)
    }
}

/// The batches of a stream: `open` connects and builds the [`Paged`] lazily, on first poll, and
/// every batch and failure is metered under `source_type`.
pub(crate) fn batches<'a, P, F>(
    source_type: &'static str,
    open: F,
) -> BoxStream<'a, Result<Batch, SourceError>>
where
    P: Pager + 'a,
    F: Future<Output = Result<Paged<P>, SourceError>> + Send + 'a,
{
    futures_util::stream::once(open)
        .map_ok(|paged| {
            futures_util::stream::try_unfold(paged, |mut paged| async move {
                Ok::<_, SourceError>(paged.next_batch().await?.map(|batch| (batch, paged)))
            })
        })
        .try_flatten()
        .inspect(move |result| match result {
            Ok(_) => super::metrics::batch(source_type),
            Err(error) => super::metrics::error(source_type, error),
        })
        .boxed()
}

#[cfg(test)]
mod tests {

    use futures_util::stream;

    use super::*;

    fn spec(cursor: Option<&str>, key: &[&str]) -> KeySpec {
        KeySpec {
            cursor: cursor.map(str::to_owned),
            primary_key: key.iter().map(|k| (*k).to_owned()).collect(),
        }
    }

    fn stream_config(mode: SyncMode, cursor: Option<&str>, key: &[&str]) -> StreamConfig {
        StreamConfig {
            name: "t".into(),
            sync_mode: mode,
            cursor_field: cursor.map(str::to_owned),
            primary_key: key.iter().map(|k| (*k).to_owned()).collect(),
        }
    }

    #[test]
    fn a_stream_without_a_primary_key_or_an_incremental_one_without_a_cursor_is_refused() {
        let no_key = KeySpec::from_stream(&stream_config(SyncMode::FullRefresh, None, &[]));
        let no_cursor = KeySpec::from_stream(&stream_config(SyncMode::Incremental, None, &["id"]));

        assert_eq!(no_key, Err(config_error("primary_key", "is required")));
        assert_eq!(no_cursor, Err(config_error("cursor_field", "is required")));
    }

    #[test]
    fn full_refresh_ignores_a_cursor_and_incremental_orders_by_it_first() {
        let full = KeySpec::from_stream(&stream_config(SyncMode::FullRefresh, Some("u"), &["a"]));
        let incremental = KeySpec::from_stream(&stream_config(
            SyncMode::Incremental,
            Some("u"),
            &["a", "b"],
        ));

        assert_eq!(full.unwrap(), spec(None, &["a"]));
        let incremental = incremental.unwrap();
        assert_eq!(incremental.width(), 3);
    }

    #[test]
    fn a_checkpoint_round_trips_and_a_misfit_one_fails_the_run() {
        let incremental = spec(Some("u"), &["id"]);
        let key = vec!["2024-01-01 00:00:00+00".to_owned(), "7".to_owned()];

        let checkpoint = checkpoint_of(&key);

        assert_eq!(
            checkpoint.0,
            json!({"v": 1, "key": ["2024-01-01 00:00:00+00", "7"]})
        );
        assert_eq!(
            parse_checkpoint(Some(&checkpoint), &incremental),
            Ok(Some(key))
        );
        assert_eq!(parse_checkpoint(None, &incremental), Ok(None));
        let misfit = [
            Checkpoint(json!({"v": 1, "key": ["7"]})),
            Checkpoint(json!({"v": 2, "key": ["a", "7"]})),
            Checkpoint(json!({"v": 1, "key": ["a", 7]})),
            Checkpoint(json!({"key": ["a", "7"]})),
            Checkpoint(json!([1])),
        ];
        for bad in misfit {
            assert_eq!(
                parse_checkpoint(Some(&bad), &incremental),
                Err(config_error(
                    "checkpoint",
                    "does not fit the stream settings"
                )),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_full_refresh_checkpoint_carries_only_the_primary_key() {
        let full = spec(None, &["id"]);
        let checkpoint = checkpoint_of(&["7".to_owned()]);

        assert_eq!(
            parse_checkpoint(Some(&checkpoint), &full),
            Ok(Some(vec!["7".to_owned()]))
        );
        let from_incremental = checkpoint_of(&["a".to_owned(), "7".to_owned()]);
        assert!(parse_checkpoint(Some(&from_incremental), &full).is_err());
    }

    #[test]
    fn one_key_value_is_the_record_key_as_is_and_several_are_a_json_array() {
        assert_eq!(record_key(&["42".to_owned()]), "42");
        assert_eq!(
            record_key(&["1".to_owned(), "a\"b".to_owned()]),
            r#"["1","a\"b"]"#
        );
    }

    /// A table of `(updated_at, id)` rows held in memory, paged the way the SQL does it:
    /// ordered by the key, strictly after it, `BATCH_ROWS` at a time.
    struct Table {
        rows: Vec<(String, String)>,
        pages: usize,
        incremental: bool,
    }

    impl Pager for Table {
        async fn page(&mut self, after: Option<&[String]>) -> Result<Vec<Row>, SourceError> {
            self.pages += 1;
            let mut rows = self.rows.clone();
            if self.incremental {
                rows.sort_by(|a, b| (&a.0, number(&a.1)).cmp(&(&b.0, number(&b.1))));
            } else {
                rows.sort_by_key(|row| number(&row.1));
            }
            let page = rows.into_iter().filter(|(updated, id)| match after {
                None => true,
                Some([cursor, key]) => (updated, number(id)) > (cursor, number(key)),
                Some([key]) => number(id) > number(key),
                Some(_) => unreachable!(),
            });
            Ok(page
                .take(BATCH_ROWS)
                .map(|(updated, id)| vec![Some(id), Some(updated)])
                .collect())
        }
    }

    fn number(text: &str) -> u64 {
        text.parse().unwrap()
    }

    /// 1,200 rows; ids 493 to 508 share one `updated_at`, which straddles the first batch end.
    fn table(incremental: bool) -> Table {
        let rows = (1..=1200)
            .map(|id: u64| {
                let day = if (493..=508).contains(&id) { 493 } else { id };
                (format!("{day:05}"), id.to_string())
            })
            .collect();
        Table {
            rows,
            pages: 0,
            incremental,
        }
    }

    async fn drain(paged: Paged<Table>) -> Vec<Batch> {
        stream::try_unfold(paged, |mut paged| async move {
            Ok::<_, SourceError>(paged.next_batch().await?.map(|batch| (batch, paged)))
        })
        .try_collect()
        .await
        .unwrap()
    }

    fn columns() -> Vec<String> {
        vec!["id".to_owned(), "updated_at".to_owned()]
    }

    #[tokio::test]
    async fn an_incremental_read_returns_every_row_once_across_a_shared_cursor_value() {
        let spec = spec(Some("updated_at"), &["id"]);
        let paged = Paged::new(table(true), columns(), &spec, None).unwrap();

        let batches = drain(paged).await;

        let sizes: Vec<usize> = batches.iter().map(|b| b.records.len()).collect();
        assert_eq!(sizes, [500, 500, 200]);
        let keys: Vec<u64> = batches
            .iter()
            .flat_map(|b| &b.records)
            .map(|r| number(&r.key))
            .collect();
        // The cursor is the primary order, ids break ties: 493..=508 stay in id order.
        let mut expected: Vec<u64> = (1..=1200).collect();
        expected.sort();
        assert_eq!(keys, expected);
        assert_eq!(
            batches[0].checkpoint.0,
            json!({"v": 1, "key": ["00493", "500"]})
        );
    }

    #[tokio::test]
    async fn resuming_from_the_checkpoint_of_any_batch_returns_exactly_the_rest() {
        let spec = spec(Some("updated_at"), &["id"]);
        let all = drain(Paged::new(table(true), columns(), &spec, None).unwrap()).await;
        let all_keys: Vec<String> = all
            .iter()
            .flat_map(|b| &b.records)
            .map(|r| r.key.clone())
            .collect();

        let mut seen = 0;
        for batch in &all {
            seen += batch.records.len();
            let after = parse_checkpoint(Some(&batch.checkpoint), &spec).unwrap();
            let rest = drain(Paged::new(table(true), columns(), &spec, after).unwrap()).await;
            let rest_keys: Vec<String> = rest
                .iter()
                .flat_map(|b| &b.records)
                .map(|r| r.key.clone())
                .collect();
            assert_eq!(rest_keys, all_keys[seen..], "after {seen} rows");
        }
    }

    #[tokio::test]
    async fn a_full_refresh_pages_by_primary_key_and_an_empty_stream_yields_no_batch() {
        let spec = spec(None, &["id"]);
        let batches = drain(Paged::new(table(false), columns(), &spec, None).unwrap()).await;
        let mut empty = table(false);
        empty.rows.clear();
        let none = drain(Paged::new(empty, columns(), &spec, None).unwrap()).await;

        assert_eq!(batches.len(), 3);
        assert_eq!(batches[2].checkpoint.0, json!({"v": 1, "key": ["1200"]}));
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn a_size_that_is_a_multiple_of_a_page_ends_on_an_empty_page() {
        let spec = spec(None, &["id"]);
        let mut thousand = table(false);
        thousand.rows.truncate(1000);
        let mut paged = Paged::new(thousand, columns(), &spec, None).unwrap();

        let mut batches = 0;
        while paged.next_batch().await.unwrap().is_some() {
            batches += 1;
        }

        assert_eq!(batches, 2);
        assert_eq!(paged.pager.pages, 3);
    }

    #[tokio::test]
    async fn a_null_primary_key_fails_the_read_without_a_value() {
        struct NullKey;
        impl Pager for NullKey {
            async fn page(&mut self, _: Option<&[String]>) -> Result<Vec<Row>, SourceError> {
                Ok(vec![vec![None, Some("secret-value".into())]])
            }
        }
        let spec = spec(None, &["id"]);
        let mut paged = Paged::new(NullKey, columns(), &spec, None).unwrap();

        let error = paged.next_batch().await.unwrap_err();

        assert_eq!(error, SourceError::Other(NULL_KEY.to_owned()));
        assert!(!error.to_string().contains("secret-value"));
    }

    #[test]
    fn a_key_column_the_stream_lacks_is_a_config_error_naming_the_setting() {
        let missing_pk = Paged::new(table(true), columns(), &spec(None, &["nope"]), None);
        let missing_cursor = Paged::new(table(true), columns(), &spec(Some("nope"), &["id"]), None);

        assert_eq!(
            missing_pk.err(),
            Some(config_error(
                "primary_key",
                "names a column the stream does not have"
            ))
        );
        assert_eq!(
            missing_cursor.err(),
            Some(config_error(
                "cursor_field",
                "names a column the stream does not have"
            ))
        );
    }

    #[tokio::test]
    async fn a_row_becomes_a_json_object_of_strings_and_nulls() {
        struct One;
        impl Pager for One {
            async fn page(&mut self, _: Option<&[String]>) -> Result<Vec<Row>, SourceError> {
                Ok(vec![vec![Some("1".into()), None]])
            }
        }
        let spec = spec(None, &["id"]);
        let mut paged = Paged::new(One, columns(), &spec, None).unwrap();

        let batch = paged.next_batch().await.unwrap().unwrap();

        assert_eq!(
            batch.records[0].data,
            json!({"id": "1", "updated_at": null})
        );
        assert!(!batch.records[0].deleted);
    }
}
