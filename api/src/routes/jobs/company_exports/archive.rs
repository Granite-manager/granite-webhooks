//! Builds a company's export: one CSV per exported table, the company's
//! stored files, `files.csv` (every file the rows point at and what became
//! of it), `manifest.json` (row counts, columns) and `README.txt`, in a zip.

use super::tables::{self, Bucket, FileColumn, Rule};
use crate::state::AppState;
use chrono::{DateTime, Datelike, NaiveDate, Timelike};
use chrono_tz::Tz;
use futures_util::TryStreamExt as _;
use serde::Serialize;
use sha2::Digest as _;
use sqlx::Row as _;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Cursor, Write as _};
use zip::write::SimpleFileOptions;

/// Most bytes of stored files put in one zip; files past it wait for the
/// next day's export (they are not marked as sent).
pub const MAX_FILE_BYTES: usize = 300 * 1024 * 1024;

/// A column of a table, as `information_schema` describes it.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    pub primary: bool,
}

/// What goes into one export.
pub struct Request<'a> {
    pub company_id: i32,
    pub company_name: &'a str,
    pub export_date: NaiveDate,
    /// When the export was made, in the company's time zone.
    pub generated_at: DateTime<Tz>,
    /// Put every file in the zip, not only those not sent before.
    pub full_files: bool,
    /// `file_hash` of the files sent in earlier exports.
    pub sent_before: &'a HashSet<String>,
}

/// A stored file put in this zip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludedFile {
    pub bucket: String,
    pub key: String,
    pub hash: String,
}

/// One exported table in the manifest.
#[derive(Debug, Clone, Serialize)]
pub struct TableEntry {
    pub table: String,
    pub file: String,
    pub rows: usize,
    pub columns: Vec<String>,
    /// Columns left out (passwords, keys, tokens).
    pub left_out_columns: Vec<String>,
}

/// A table left out, and why.
#[derive(Debug, Clone, Serialize)]
pub struct SkippedTable {
    pub table: String,
    pub reason: String,
}

/// How many files ended up how.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FileCounts {
    /// Put in this zip.
    pub included: usize,
    /// Sent in an earlier export (listed in `files.csv`, not in the zip).
    pub sent_before: usize,
    /// Links to other websites (listed, not copied).
    pub external: usize,
    /// Not found in storage.
    pub missing: usize,
    /// Storage refused or failed (tried again next export).
    pub errors: usize,
    /// Past the size limit of one zip (sent with the next export).
    pub deferred: usize,
    /// Keys outside the company's folder (never read).
    pub refused: usize,
}

/// The `manifest.json` of the zip.
#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub company_id: i32,
    pub company_name: String,
    pub export_date: String,
    pub generated_at: String,
    /// Every stored file is in this zip, not only the new ones.
    pub full_files: bool,
    pub tables: Vec<TableEntry>,
    pub skipped_tables: Vec<SkippedTable>,
    /// Tables of the database the export does not know (left out).
    pub unknown_tables: Vec<String>,
    /// Exported tables not in this database.
    pub missing_tables: Vec<String>,
    pub row_count: usize,
    pub files: FileCounts,
}

/// A built export.
pub struct Built {
    pub zip: Vec<u8>,
    pub manifest: Manifest,
    pub included_files: Vec<IncludedFile>,
}

/// Why an export could not be built.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("zip: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest: {0}")]
    Json(#[from] serde_json::Error),
}

/// The base tables of the database and their columns, in column order.
pub async fn schema(pool: &sqlx::MySqlPool) -> Result<BTreeMap<String, Vec<Column>>, sqlx::Error> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT CAST(c.TABLE_NAME AS CHAR), CAST(c.COLUMN_NAME AS CHAR),
                CAST(c.DATA_TYPE AS CHAR), CAST(c.COLUMN_KEY AS CHAR)
           FROM information_schema.COLUMNS c
           JOIN information_schema.TABLES t
             ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME
          WHERE c.TABLE_SCHEMA = DATABASE() AND t.TABLE_TYPE = 'BASE TABLE'
          ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION",
    )
    .fetch_all(pool)
    .await?;
    let mut tables: BTreeMap<String, Vec<Column>> = BTreeMap::new();
    for (table, name, data_type, key) in rows {
        tables.entry(table).or_default().push(Column {
            name,
            data_type: data_type.to_ascii_lowercase(),
            primary: key == "PRI",
        });
    }
    Ok(tables)
}

fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// The column as text: binary values as hex, the rest as MySQL prints them.
fn select_expr(column: &Column) -> String {
    let name = quote_ident(&column.name);
    match column.data_type.as_str() {
        "binary" | "varbinary" | "blob" | "tinyblob" | "mediumblob" | "longblob" => {
            format!("HEX({name})")
        }
        "bit" => format!("CAST({name} + 0 AS CHAR)"),
        "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
        | "multipolygon" | "geometrycollection" | "geomcollection" => {
            format!("ST_AsText({name})")
        }
        _ => format!("CAST({name} AS CHAR)"),
    }
}

/// One CSV field (RFC 4180): NULL is empty, an empty string is `""`.
fn csv_field(out: &mut String, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    if value.is_empty()
        || value.contains([',', '"', '\n', '\r'])
        || value.starts_with(' ')
        || value.ends_with(' ')
    {
        out.push('"');
        out.push_str(&value.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(value);
    }
}

fn csv_line(fields: &[Option<&str>]) -> String {
    let mut line = String::new();
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            line.push(',');
        }
        csv_field(&mut line, *field);
    }
    line.push_str("\r\n");
    line
}

/// Excel reads CSV as UTF-8 with this mark.
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// A row's reference to a stored file.
#[derive(Debug, Clone)]
struct FileRef {
    table: &'static str,
    column: &'static str,
    row_id: Option<String>,
    value: String,
}

/// Where a referenced file is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Location {
    Stored {
        /// Folder in the zip (`main`, `signatures`, `recordings`).
        label: &'static str,
        bucket: String,
        key: String,
    },
    External,
    Refused,
    /// The bucket is not configured here.
    Unavailable,
}

/// The `(table, column)`s holding files, by table.
fn file_columns_by_table() -> HashMap<&'static str, Vec<FileColumn>> {
    let mut by_table: HashMap<&'static str, Vec<FileColumn>> = HashMap::new();
    for file in tables::FILE_COLUMNS {
        let (FileColumn::Url(table, _) | FileColumn::Key(table, ..)) = file;
        by_table.entry(table).or_default().push(*file);
    }
    by_table
}

const fn file_column_name(file: &FileColumn) -> &'static str {
    match file {
        FileColumn::Url(_, column) | FileColumn::Key(_, column, ..) => column,
    }
}

/// A path inside the zip for a storage key: no leading `/`, no `..`.
fn zip_path(label: &str, key: &str) -> String {
    let clean: Vec<&str> = key
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect();
    format!("files/{label}/{}", clean.join("/"))
}

pub fn file_hash(bucket: &str, key: &str) -> String {
    hex(&sha2::Sha256::digest(format!("{bucket}/{key}").as_bytes()))
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Where `value` of `file` points.
fn locate(state: &AppState, company_id: i32, file: &FileColumn, value: &str) -> Location {
    let main_bucket = state.storage.main_bucket().unwrap_or_default().to_string();
    match file {
        FileColumn::Url(..) => match state.storage.locate(value) {
            Some((None, key)) => Location::Stored {
                label: "main",
                bucket: main_bucket,
                key,
            },
            Some((Some(bucket), key)) if bucket == main_bucket => Location::Stored {
                label: "main",
                bucket,
                key,
            },
            // Another bucket, or another website: listed, never read.
            _ => Location::External,
        },
        FileColumn::Key(_, _, bucket, prefix) => {
            let prefix = prefix.replace("{company}", &company_id.to_string());
            if !value.starts_with(&prefix) || value.split('/').any(|part| part == "..") {
                return Location::Refused;
            }
            let (label, bucket) = match bucket {
                Bucket::Main => ("main", Some(main_bucket)),
                Bucket::Signatures => (
                    "signatures",
                    state.signature_bucket.as_deref().map(str::to_string),
                ),
                Bucket::Recordings => (
                    "recordings",
                    state.phone.recordings_bucket.as_deref().map(str::to_string),
                ),
            };
            match bucket {
                Some(bucket) => Location::Stored {
                    label,
                    bucket,
                    key: value.to_string(),
                },
                None => Location::Unavailable,
            }
        }
    }
}

fn options_for(request: &Request<'_>, method: zip::CompressionMethod) -> SimpleFileOptions {
    let at = request.generated_at;
    let mut options = SimpleFileOptions::default()
        .compression_method(method)
        .unix_permissions(0o644);
    if let Ok(time) = zip::DateTime::from_date_and_time(
        u16::try_from(at.year()).unwrap_or(1980),
        u8::try_from(at.month()).unwrap_or(1),
        u8::try_from(at.day()).unwrap_or(1),
        u8::try_from(at.hour()).unwrap_or(0),
        u8::try_from(at.minute()).unwrap_or(0),
        u8::try_from(at.second()).unwrap_or(0),
    ) {
        options = options.last_modified_time(time);
    }
    options
}

/// Builds the export of `request.company_id`.
pub async fn build(state: &AppState, request: &Request<'_>) -> Result<Built, BuildError> {
    let schema = schema(&state.pool).await?;
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let deflate = options_for(request, zip::CompressionMethod::Deflated);
    let store = options_for(request, zip::CompressionMethod::Stored);
    let files_by_table = file_columns_by_table();

    let mut manifest = Manifest {
        company_id: request.company_id,
        company_name: request.company_name.to_string(),
        export_date: request.export_date.to_string(),
        generated_at: request.generated_at.to_rfc3339(),
        full_files: request.full_files,
        tables: Vec::new(),
        skipped_tables: Vec::new(),
        unknown_tables: Vec::new(),
        missing_tables: Vec::new(),
        row_count: 0,
        files: FileCounts::default(),
    };
    let mut file_refs: Vec<FileRef> = Vec::new();

    for name in schema.keys() {
        if tables::table(name).is_none() {
            manifest.unknown_tables.push(name.clone());
        }
    }
    for table in tables::TABLES {
        let Rule::Export(_) = table.rule else {
            if let Rule::Skip(reason) = table.rule {
                manifest.skipped_tables.push(SkippedTable {
                    table: table.name.to_string(),
                    reason: reason.to_string(),
                });
            }
            continue;
        };
        let Some(columns) = schema.get(table.name) else {
            manifest.missing_tables.push(table.name.to_string());
            continue;
        };
        let Some((condition, binds)) = tables::condition(table.name) else {
            continue;
        };
        let (kept, left_out): (Vec<&Column>, Vec<&Column>) = columns
            .iter()
            .partition(|column| !tables::column_is_left_out(table.name, &column.name));
        if kept.is_empty() {
            continue;
        }
        let order: Vec<String> = {
            let primary: Vec<String> = kept
                .iter()
                .filter(|column| column.primary)
                .map(|column| quote_ident(&column.name))
                .collect();
            if primary.is_empty() {
                vec![quote_ident(&kept[0].name)]
            } else {
                primary
            }
        };
        let row_id_index = kept.iter().position(|column| column.primary);
        let file_indexes: Vec<(usize, FileColumn)> = files_by_table
            .get(table.name)
            .map(|files| {
                files
                    .iter()
                    .filter_map(|file| {
                        let column = file_column_name(file);
                        kept.iter()
                            .position(|kept| kept.name.eq_ignore_ascii_case(column))
                            .map(|index| (index, *file))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sql = format!(
            "SELECT {} FROM {} WHERE {condition} ORDER BY {}",
            kept.iter()
                .map(|column| select_expr(column))
                .collect::<Vec<_>>()
                .join(", "),
            quote_ident(table.name),
            order.join(", "),
        );
        let file_name = format!("data/{}.csv", table.name);
        zip.start_file(&file_name, deflate)?;
        zip.write_all(UTF8_BOM)?;
        let header: Vec<Option<&str>> = kept
            .iter()
            .map(|column| Some(column.name.as_str()))
            .collect();
        zip.write_all(csv_line(&header).as_bytes())?;

        // Names come from `TABLES` and `information_schema`, quoted; the
        // company id is bound.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for _ in 0..binds {
            query = query.bind(request.company_id);
        }
        let mut rows = query.fetch(&state.pool);
        let mut count = 0usize;
        while let Some(row) = rows.try_next().await? {
            let mut values: Vec<Option<String>> = Vec::with_capacity(kept.len());
            for index in 0..kept.len() {
                values.push(row.try_get::<Option<String>, _>(index)?);
            }
            for (index, file) in &file_indexes {
                if let Some(value) = values[*index].as_deref().map(str::trim)
                    && !value.is_empty()
                    && !matches!(value, "undefined" | "null")
                {
                    file_refs.push(FileRef {
                        table: table.name,
                        column: file_column_name(file),
                        row_id: row_id_index.and_then(|i| values[i].clone()),
                        value: value.to_string(),
                    });
                }
            }
            let fields: Vec<Option<&str>> = values.iter().map(Option::as_deref).collect();
            zip.write_all(csv_line(&fields).as_bytes())?;
            count += 1;
        }
        drop(rows);
        manifest.row_count += count;
        manifest.tables.push(TableEntry {
            table: table.name.to_string(),
            file: file_name,
            rows: count,
            columns: kept.iter().map(|column| column.name.clone()).collect(),
            left_out_columns: left_out.iter().map(|column| column.name.clone()).collect(),
        });
    }

    // The files, each stored object once.
    let mut index = String::from_utf8(UTF8_BOM.to_vec()).unwrap_or_default();
    index.push_str(&csv_line(&[
        Some("table"),
        Some("row_id"),
        Some("column"),
        Some("value"),
        Some("zip_path"),
        Some("status"),
    ]));
    let mut outcome: HashMap<(String, String), (String, &'static str)> = HashMap::new();
    let mut included_files = Vec::new();
    let mut file_bytes = 0usize;
    for file_ref in &file_refs {
        let file = tables::FILE_COLUMNS
            .iter()
            .find(|file| {
                let (FileColumn::Url(table, column) | FileColumn::Key(table, column, ..)) = file;
                *table == file_ref.table && *column == file_ref.column
            })
            .copied()
            .expect("file_refs come from FILE_COLUMNS");
        let (path, status): (String, &'static str) =
            match locate(state, request.company_id, &file, &file_ref.value) {
                Location::External => {
                    manifest.files.external += 1;
                    (String::new(), "external link (not copied)")
                }
                Location::Refused => {
                    manifest.files.refused += 1;
                    (String::new(), "outside the company's folder (not copied)")
                }
                Location::Unavailable => {
                    manifest.files.errors += 1;
                    (String::new(), "storage not configured")
                }
                Location::Stored { label, bucket, key } => {
                    let path = zip_path(label, &key);
                    if let Some((path, status)) = outcome.get(&(bucket.clone(), key.clone())) {
                        (path.clone(), *status)
                    } else {
                        let hash = file_hash(&bucket, &key);
                        let status = if !request.full_files && request.sent_before.contains(&hash) {
                            manifest.files.sent_before += 1;
                            "sent in an earlier export"
                        } else if file_bytes >= MAX_FILE_BYTES {
                            manifest.files.deferred += 1;
                            "in the next export (size limit)"
                        } else {
                            match state.storage.get_in(&bucket, &key).await {
                                Ok(Some(object)) => {
                                    file_bytes += object.bytes.len();
                                    zip.start_file(&path, store)?;
                                    zip.write_all(&object.bytes)?;
                                    manifest.files.included += 1;
                                    included_files.push(IncludedFile {
                                        bucket: bucket.clone(),
                                        key: key.clone(),
                                        hash,
                                    });
                                    "included"
                                }
                                Ok(None) => {
                                    manifest.files.missing += 1;
                                    "missing from storage"
                                }
                                Err(error) => {
                                    lambda_http::tracing::warn!(
                                        %error,
                                        company_id = request.company_id,
                                        "Export file not read"
                                    );
                                    manifest.files.errors += 1;
                                    "could not be read (tried again next export)"
                                }
                            }
                        };
                        outcome.insert((bucket, key), (path.clone(), status));
                        (path, status)
                    }
                }
            };
        let shown_path = if status == "included" || status == "sent in an earlier export" {
            Some(path.as_str())
        } else {
            None
        };
        index.push_str(&csv_line(&[
            Some(file_ref.table),
            file_ref.row_id.as_deref(),
            Some(file_ref.column),
            Some(&file_ref.value),
            shown_path,
            Some(status),
        ]));
    }
    zip.start_file("files.csv", deflate)?;
    zip.write_all(index.as_bytes())?;

    zip.start_file("manifest.json", deflate)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    zip.start_file("README.txt", deflate)?;
    zip.write_all(readme(&manifest).as_bytes())?;

    let zip = zip.finish()?.into_inner();
    Ok(Built {
        zip,
        manifest,
        included_files,
    })
}

fn readme(manifest: &Manifest) -> String {
    let files = if manifest.full_files {
        "This export holds every stored file (pictures, documents, recordings)."
    } else {
        "This export holds the stored files added since the last export; every file is \
         in the Monday export. files.csv lists every file the data points at and where \
         it is."
    };
    format!(
        "{company} - data export of {date}\r\n\
         \r\n\
         data/<table>.csv   one file per table, UTF-8, comma-separated, first line = column names.\r\n\
         \x20                  An empty field is NULL; \"\" is an empty text. Binary values are hex.\r\n\
         files/             pictures and documents, in the folders of the app's storage.\r\n\
         files.csv          every stored file a row points at: table, row id, column, value,\r\n\
         \x20                  path in this zip and status.\r\n\
         manifest.json      row count and columns of every table; tables and columns left out.\r\n\
         \r\n\
         {files}\r\n\
         \r\n\
         Left out: passwords, sign-in sessions and codes, devices, and the keys of\r\n\
         Telnyx, CloudTalk, RingCentral, QuickBooks and other providers.\r\n\
         \r\n\
         {tables} tables, {rows} rows, {included} files in this zip.\r\n",
        company = manifest.company_name,
        date = manifest.export_date,
        tables = manifest.tables.len(),
        rows = manifest.row_count,
        included = manifest.files.included,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_fields_are_quoted_when_needed() {
        assert_eq!(csv_line(&[Some("a"), None, Some("")]), "a,,\"\"\r\n");
        assert_eq!(
            csv_line(&[Some("x,y"), Some("say \"hi\""), Some("two\nlines")]),
            "\"x,y\",\"say \"\"hi\"\"\",\"two\nlines\"\r\n"
        );
        assert_eq!(csv_line(&[Some(" pad")]), "\" pad\"\r\n");
    }

    #[test]
    fn zip_paths_stay_inside_the_files_folder() {
        assert_eq!(
            zip_path("main", "/dynamic-images/../stones/a.jpg"),
            "files/main/dynamic-images/stones/a.jpg"
        );
        assert_eq!(zip_path("main", "a\\b.png"), "files/main/a/b.png");
    }

    #[test]
    fn binary_columns_are_hex() {
        let column = |data_type: &str| Column {
            name: "c".into(),
            data_type: data_type.into(),
            primary: false,
        };
        assert_eq!(select_expr(&column("blob")), "HEX(`c`)");
        assert_eq!(select_expr(&column("varchar")), "CAST(`c` AS CHAR)");
        assert_eq!(quote_ident("we`ird"), "`we``ird`");
    }
}
