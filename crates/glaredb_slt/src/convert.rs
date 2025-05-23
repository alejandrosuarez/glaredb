use glaredb_core::arrays::batch::Batch;
use glaredb_core::arrays::datatype::DataTypeId;
use glaredb_core::arrays::field::ColumnSchema;
use glaredb_core::arrays::format::{BinaryFormat, FormatOptions, Formatter};
use glaredb_error::Result;
use sqllogictest::DefaultColumnType;

/// Converts batches to rows.
pub fn batches_to_rows(batches: Vec<Batch>) -> Result<Vec<Vec<String>>> {
    const OPTS: FormatOptions = FormatOptions {
        null: "NULL",
        empty_string: "(empty)",
        binary_format: BinaryFormat::Hex,
    };
    let formatter = Formatter::new(OPTS);

    let mut rows = Vec::new();

    for batch in batches {
        for row_idx in 0..batch.num_rows() {
            let col_strings = batch
                .arrays()
                .iter()
                .map(|arr| {
                    formatter
                        .format_array_value(arr, row_idx)
                        .map(|v| v.to_string())
                })
                .collect::<Result<Vec<_>>>()?;

            match transform_multiline_cols_to_rows(&col_strings) {
                Some(new_rows) => rows.extend(new_rows),
                None => rows.push(col_strings),
            }
        }
    }

    Ok(rows)
}

pub fn schema_to_types(schema: &ColumnSchema) -> Vec<DefaultColumnType> {
    let mut typs = Vec::new();
    for field in &schema.fields {
        let typ = match field.datatype.id() {
            DataTypeId::Int8
            | DataTypeId::Int16
            | DataTypeId::Int32
            | DataTypeId::Int64
            | DataTypeId::UInt8
            | DataTypeId::UInt16
            | DataTypeId::UInt32
            | DataTypeId::UInt64 => DefaultColumnType::Integer,
            DataTypeId::Float32 | DataTypeId::Float64 => DefaultColumnType::FloatingPoint,
            DataTypeId::Utf8 | DataTypeId::Boolean => DefaultColumnType::Text,
            _ => DefaultColumnType::Any,
        };
        typs.push(typ);
    }

    typs
}

/// Transforms a row with a potentially multiline column into multiple rows with
/// each column containing a single line.
///
/// Columns that don't have content for that row will instead have a '.'.
///
/// For example, the following output:
/// ```text
/// +-------------+---------------------------------------------+
/// | type        | plan                                        |
/// +-------------+---------------------------------------------+
/// | logical     | Order (expressions = [#0 DESC NULLS FIRST]) |
/// |             |   Projection (expressions = [#0])           |
/// |             |     ExpressionList                          |
/// +-------------+---------------------------------------------+
/// | pipeline    | Pipeline 1                                  |
/// |             |   Pipeline 2                                |
/// +-------------+---------------------------------------------+
/// ```
///
/// Would get trasnformed into:
/// ```text
/// logical   Order (expressions = [#0 DESC NULLS FIRST])
/// .           Projection (expressions = [#0])
/// .             ExpressionList
/// pipeline  Pipeline 1
/// .           Pipeline 2
/// ```
/// Where each line is a new "row".
///
/// This allows for nicely formatted SLTs for queries that return multiline
/// results (like EXPLAIN).
fn transform_multiline_cols_to_rows<S: AsRef<str>>(cols: &[S]) -> Option<Vec<Vec<String>>> {
    let max = cols.iter().fold(0, |curr, col| {
        let col_lines = col.as_ref().lines().count();
        if col_lines > curr { col_lines } else { curr }
    });

    if max > 1 {
        let mut new_rows = Vec::new();
        for row_idx in 0..max {
            let new_row: Vec<_> = cols
                .iter()
                .map(|col| col.as_ref().lines().nth(row_idx).unwrap_or(".").to_string())
                .collect();
            new_rows.push(new_row)
        }
        Some(new_rows)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_not_needed() {
        let orig_row = &["col1", "col2", "col3"];
        let out = transform_multiline_cols_to_rows(orig_row);
        assert_eq!(None, out);
    }

    #[test]
    fn transform_multiline_col() {
        let orig_row = &["col1", "col2\ncol2a\ncol2b", "col3"];
        let out = transform_multiline_cols_to_rows(orig_row);

        let expected = vec![
            vec!["col1".to_string(), "col2".to_string(), "col3".to_string()],
            vec![".".to_string(), "col2a".to_string(), ".".to_string()],
            vec![".".to_string(), "col2b".to_string(), ".".to_string()],
        ];

        assert_eq!(Some(expected), out);
    }

    #[test]
    fn transform_multiple_multiline_cols() {
        let orig_row = &["col1", "col2\ncol2a\ncol2b", "col3\ncol3a"];
        let out = transform_multiline_cols_to_rows(orig_row);

        let expected = vec![
            vec!["col1".to_string(), "col2".to_string(), "col3".to_string()],
            vec![".".to_string(), "col2a".to_string(), "col3a".to_string()],
            vec![".".to_string(), "col2b".to_string(), ".".to_string()],
        ];

        assert_eq!(Some(expected), out);
    }
}
