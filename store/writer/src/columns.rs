//! Arrow column construction shared by recording and derived-table writers.

use std::sync::Arc;

use anyhow::Result;
use arrow::array::{ArrayRef, Float64Array, Int64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;

/// Accumulates named Arrow columns into one `RecordBatch`. Integer columns are
/// signed so that the materialized tables keep the SQLite affinities their
/// consumers were written against; identity and address columns stay unsigned.
#[derive(Default)]
pub struct Columns {
    fields: Vec<Field>,
    arrays: Vec<ArrayRef>,
}

impl Columns {
    fn push(&mut self, name: &str, ty: DataType, nullable: bool, array: ArrayRef) {
        self.fields.push(Field::new(name, ty, nullable));
        self.arrays.push(array);
    }

    /// Append a non-null unsigned integer column.
    pub fn u64(&mut self, name: &str, values: Vec<u64>) {
        self.push(
            name,
            DataType::UInt64,
            false,
            Arc::new(UInt64Array::from(values)),
        );
    }

    /// Append a non-null signed integer column.
    pub fn i64(&mut self, name: &str, values: Vec<i64>) {
        self.push(
            name,
            DataType::Int64,
            false,
            Arc::new(Int64Array::from(values)),
        );
    }

    /// Append a nullable signed integer column.
    pub fn i64_opt(&mut self, name: &str, values: Vec<Option<i64>>) {
        self.push(
            name,
            DataType::Int64,
            true,
            Arc::new(Int64Array::from(values)),
        );
    }

    /// Append a non-null floating-point column.
    pub fn f64(&mut self, name: &str, values: Vec<f64>) {
        self.push(
            name,
            DataType::Float64,
            false,
            Arc::new(Float64Array::from(values)),
        );
    }

    /// Append a nullable floating-point column.
    pub fn f64_opt(&mut self, name: &str, values: Vec<Option<f64>>) {
        self.push(
            name,
            DataType::Float64,
            true,
            Arc::new(Float64Array::from(values)),
        );
    }

    /// Append a non-null UTF-8 string column.
    pub fn text(&mut self, name: &str, values: Vec<String>) {
        self.push(
            name,
            DataType::Utf8,
            false,
            Arc::new(StringArray::from(values)),
        );
    }

    /// Append a nullable UTF-8 string column.
    pub fn text_opt(&mut self, name: &str, values: Vec<Option<String>>) {
        self.push(
            name,
            DataType::Utf8,
            true,
            Arc::new(StringArray::from(values)),
        );
    }

    /// Build the batch, rejecting columns with different lengths.
    pub fn finish(self) -> Result<RecordBatch> {
        let schema = Arc::new(Schema::new(self.fields));
        Ok(RecordBatch::try_new(schema, self.arrays)?)
    }
}
