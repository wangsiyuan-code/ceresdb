// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

use std::{convert::TryFrom, sync::Arc};

use arrow::{
    array::{Array, ArrayData, ArrayRef},
    buffer::MutableBuffer,
    compute,
    record_batch::RecordBatch as ArrowRecordBatch,
    util::bit_util,
};
use common_types::{
    bytes::{BytesMut, SafeBufMut},
    datum::DatumKind,
    schema::{ArrowSchema, ArrowSchemaRef, DataType, Field},
};
use common_util::define_result;
use log::trace;
use parquet::{
    arrow::ArrowWriter,
    basic::Compression,
    file::{metadata::KeyValue, properties::WriterProperties},
};
use prost::Message;
use proto::sst::SstMetaData as SstMetaDataPb;
use snafu::{ensure, Backtrace, OptionExt, ResultExt, Snafu};

use crate::{
    sst::{
        file::SstMetaData,
        parquet::hybrid::{self, IndexedType},
    },
    table_options::{StorageFormat, StorageFormatOptions},
};

// TODO: Only support i32 offset now, consider i64 here?
const OFFSET_SIZE: usize = std::mem::size_of::<i32>();

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display(
        "Failed to encode sst meta data, err:{}.\nBacktrace:\n{}",
        source,
        backtrace
    ))]
    EncodeIntoPb {
        source: prost::EncodeError,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Failed to decode sst meta data, base64 of meta value:{}, err:{}.\nBacktrace:\n{}",
        meta_value,
        source,
        backtrace,
    ))]
    DecodeFromPb {
        meta_value: String,
        source: prost::DecodeError,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Invalid meta key, expect:{}, given:{}.\nBacktrace:\n{}",
        expect,
        given,
        backtrace
    ))]
    InvalidMetaKey {
        expect: String,
        given: String,
        backtrace: Backtrace,
    },

    #[snafu(display("Base64 meta value not found.\nBacktrace:\n{}", backtrace))]
    Base64MetaValueNotFound { backtrace: Backtrace },

    #[snafu(display(
        "Invalid base64 meta value length, base64 of meta value:{}.\nBacktrace:\n{}",
        meta_value,
        backtrace,
    ))]
    InvalidBase64MetaValueLen {
        meta_value: String,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Failed to decode base64 meta value, base64 of meta value:{}, err:{}",
        meta_value,
        source
    ))]
    DecodeBase64MetaValue {
        meta_value: String,
        source: base64::DecodeError,
    },

    #[snafu(display(
        "Invalid meta value length, base64 of meta value:{}.\nBacktrace:\n{}",
        meta_value,
        backtrace
    ))]
    InvalidMetaValueLen {
        meta_value: String,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Invalid meta value header, base64 of meta value:{}.\nBacktrace:\n{}",
        meta_value,
        backtrace
    ))]
    InvalidMetaValueHeader {
        meta_value: String,
        backtrace: Backtrace,
    },

    #[snafu(display("Failed to convert sst meta data from protobuf, err:{}", source))]
    ConvertSstMetaData { source: crate::sst::file::Error },

    #[snafu(display(
        "Failed to encode record batch into sst, err:{}.\nBacktrace:\n{}",
        source,
        backtrace
    ))]
    EncodeRecordBatch {
        source: Box<dyn std::error::Error + Send + Sync>,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Failed to decode hybrid record batch, err:{}.\nBacktrace:\n{}",
        source,
        backtrace
    ))]
    DecodeRecordBatch {
        source: Box<dyn std::error::Error + Send + Sync>,
        backtrace: Backtrace,
    },

    #[snafu(display(
    "Sst meta data collapsible_cols_idx is empty, fail to decode hybrid record batch.\nBacktrace:\n{}",
    backtrace
    ))]
    CollapsibleColsIdxEmpty { backtrace: Backtrace },

    #[snafu(display("Tsid is required for hybrid format.\nBacktrace:\n{}", backtrace))]
    TsidRequired { backtrace: Backtrace },

    #[snafu(display(
        "Key column must be string type. type:{}\nBacktrace:\n{}",
        type_name,
        backtrace
    ))]
    StringKeyColumnRequired {
        type_name: String,
        backtrace: Backtrace,
    },
}

define_result!(Error);

pub const META_KEY: &str = "meta";
pub const META_VALUE_HEADER: u8 = 0;

/// Encode the sst meta data into binary key value pair.
pub fn encode_sst_meta_data(meta_data: SstMetaData) -> Result<KeyValue> {
    let meta_data_pb = SstMetaDataPb::from(meta_data);

    let mut buf = BytesMut::with_capacity(meta_data_pb.encoded_len() as usize + 1);
    buf.try_put_u8(META_VALUE_HEADER)
        .expect("Should write header into the buffer successfully");

    // encode the sst meta data into protobuf binary
    meta_data_pb.encode(&mut buf).context(EncodeIntoPb)?;
    Ok(KeyValue {
        key: META_KEY.to_string(),
        value: Some(base64::encode(buf.as_ref())),
    })
}

/// Decode the sst meta data from the binary key value pair.
pub fn decode_sst_meta_data(kv: &KeyValue) -> Result<SstMetaData> {
    ensure!(
        kv.key == META_KEY,
        InvalidMetaKey {
            expect: META_KEY,
            given: &kv.key,
        }
    );

    let meta_value = kv.value.as_ref().context(Base64MetaValueNotFound)?;
    ensure!(
        !meta_value.is_empty(),
        InvalidBase64MetaValueLen { meta_value }
    );

    let raw_bytes = base64::decode(meta_value).context(DecodeBase64MetaValue { meta_value })?;

    ensure!(!raw_bytes.is_empty(), InvalidMetaValueLen { meta_value });

    ensure!(
        raw_bytes[0] == META_VALUE_HEADER,
        InvalidMetaValueHeader { meta_value }
    );

    let meta_data_pb: SstMetaDataPb =
        Message::decode(&raw_bytes[1..]).context(DecodeFromPb { meta_value })?;

    SstMetaData::try_from(meta_data_pb).context(ConvertSstMetaData)
}

/// RecordEncoder is used for encoding ArrowBatch.
///
/// TODO: allow pre-allocate buffer
trait RecordEncoder {
    /// Encode vector of arrow batch, return encoded row number
    fn encode(&mut self, arrow_record_batch_vec: Vec<ArrowRecordBatch>) -> Result<usize>;

    /// Return encoded bytes
    /// Note: trait method cannot receive `self`, so take a &mut self here to
    /// indicate this encoder is already consumed
    fn close(&mut self) -> Result<Vec<u8>>;
}

struct ColumnarRecordEncoder {
    // wrap in Option so ownership can be taken out behind `&mut self`
    arrow_writer: Option<ArrowWriter<Vec<u8>>>,
    arrow_schema: ArrowSchemaRef,
}

impl ColumnarRecordEncoder {
    fn try_new(
        num_rows_per_row_group: usize,
        compression: Compression,
        meta_data: SstMetaData,
    ) -> Result<Self> {
        let arrow_schema = meta_data.schema.to_arrow_schema_ref();

        let write_props = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![encode_sst_meta_data(meta_data)?]))
            .set_max_row_group_size(num_rows_per_row_group)
            .set_compression(compression)
            .build();

        let arrow_writer =
            ArrowWriter::try_new(Vec::new(), arrow_schema.clone(), Some(write_props))
                .map_err(|e| Box::new(e) as _)
                .context(EncodeRecordBatch)?;

        Ok(Self {
            arrow_writer: Some(arrow_writer),
            arrow_schema,
        })
    }
}

impl RecordEncoder for ColumnarRecordEncoder {
    fn encode(&mut self, arrow_record_batch_vec: Vec<ArrowRecordBatch>) -> Result<usize> {
        assert!(self.arrow_writer.is_some());

        let record_batch = compute::concat_batches(&self.arrow_schema, &arrow_record_batch_vec)
            .map_err(|e| Box::new(e) as _)
            .context(EncodeRecordBatch)?;

        self.arrow_writer
            .as_mut()
            .unwrap()
            .write(&record_batch)
            .map_err(|e| Box::new(e) as _)
            .context(EncodeRecordBatch)?;

        Ok(record_batch.num_rows())
    }

    fn close(&mut self) -> Result<Vec<u8>> {
        assert!(self.arrow_writer.is_some());

        let arrow_writer = self.arrow_writer.take().unwrap();
        let bytes = arrow_writer
            .into_inner()
            .map_err(|e| Box::new(e) as _)
            .context(EncodeRecordBatch)?;

        Ok(bytes)
    }
}

struct HybridRecordEncoder {
    // wrap in Option so ownership can be taken out behind `&mut self`
    arrow_writer: Option<ArrowWriter<Vec<u8>>>,
    arrow_schema: ArrowSchemaRef,
    tsid_type: IndexedType,
    non_collapsible_col_types: Vec<IndexedType>,
    // columns that can be collpased into list
    collapsible_col_types: Vec<IndexedType>,
}

impl HybridRecordEncoder {
    fn try_new(
        num_rows_per_row_group: usize,
        compression: Compression,
        mut meta_data: SstMetaData,
    ) -> Result<Self> {
        // TODO: What we really want here is a unique ID, tsid is one case
        // Maybe support other cases later.
        let tsid_idx = meta_data.schema.index_of_tsid().context(TsidRequired)?;
        let tsid_type = IndexedType {
            idx: tsid_idx,
            data_type: meta_data.schema.column(tsid_idx).data_type,
        };

        let mut non_collapsible_col_types = Vec::new();
        let mut collapsible_col_types = Vec::new();
        for (idx, col) in meta_data.schema.columns().iter().enumerate() {
            if idx == tsid_idx {
                continue;
            }

            if meta_data.schema.is_collapsible_column(idx) {
                collapsible_col_types.push(IndexedType {
                    idx,
                    data_type: meta_data.schema.column(idx).data_type,
                });
                meta_data
                    .storage_format_opts
                    .collapsible_cols_idx
                    .push(idx as u32);
            } else {
                // TODO: support non-string key columns
                ensure!(
                    matches!(col.data_type, DatumKind::String),
                    StringKeyColumnRequired {
                        type_name: col.data_type.to_string(),
                    }
                );
                non_collapsible_col_types.push(IndexedType {
                    idx,
                    data_type: col.data_type,
                });
            }
        }

        let arrow_schema = hybrid::build_hybrid_arrow_schema(&meta_data.schema);

        let write_props = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![encode_sst_meta_data(meta_data)?]))
            .set_max_row_group_size(num_rows_per_row_group)
            .set_compression(compression)
            .build();

        let arrow_writer =
            ArrowWriter::try_new(Vec::new(), arrow_schema.clone(), Some(write_props))
                .map_err(|e| Box::new(e) as _)
                .context(EncodeRecordBatch)?;
        Ok(Self {
            arrow_writer: Some(arrow_writer),
            arrow_schema,
            tsid_type,
            non_collapsible_col_types,
            collapsible_col_types,
        })
    }
}

impl RecordEncoder for HybridRecordEncoder {
    fn encode(&mut self, arrow_record_batch_vec: Vec<ArrowRecordBatch>) -> Result<usize> {
        assert!(self.arrow_writer.is_some());

        let record_batch = hybrid::convert_to_hybrid_record(
            &self.tsid_type,
            &self.non_collapsible_col_types,
            &self.collapsible_col_types,
            self.arrow_schema.clone(),
            arrow_record_batch_vec,
        )
        .map_err(|e| Box::new(e) as _)
        .context(EncodeRecordBatch)?;

        self.arrow_writer
            .as_mut()
            .unwrap()
            .write(&record_batch)
            .map_err(|e| Box::new(e) as _)
            .context(EncodeRecordBatch)?;

        // The num in row group will always be less than `num_rows_per_row_group`,
        // so we need to flush manually here.
        // TODO: maybe we should merge multiple hybrid record batch to one row group.
        self.arrow_writer
            .as_mut()
            .unwrap()
            .flush()
            .map_err(|e| Box::new(e) as _)
            .context(EncodeRecordBatch)?;

        Ok(record_batch.num_rows())
    }

    fn close(&mut self) -> Result<Vec<u8>> {
        assert!(self.arrow_writer.is_some());

        let arrow_writer = self.arrow_writer.take().unwrap();
        let bytes = arrow_writer
            .into_inner()
            .map_err(|e| Box::new(e) as _)
            .context(EncodeRecordBatch)?;
        Ok(bytes)
    }
}

pub struct ParquetEncoder {
    record_encoder: Box<dyn RecordEncoder + Send>,
}

impl ParquetEncoder {
    pub fn try_new(
        num_rows_per_row_group: usize,
        compression: Compression,
        meta_data: SstMetaData,
    ) -> Result<Self> {
        let record_encoder: Box<dyn RecordEncoder + Send> = match meta_data.storage_format() {
            StorageFormat::Hybrid => Box::new(HybridRecordEncoder::try_new(
                num_rows_per_row_group,
                compression,
                meta_data,
            )?),
            StorageFormat::Columnar => Box::new(ColumnarRecordEncoder::try_new(
                num_rows_per_row_group,
                compression,
                meta_data,
            )?),
        };

        Ok(ParquetEncoder { record_encoder })
    }

    /// Encode the record batch with [ArrowWriter] and the encoded contents is
    /// written to the buffer.
    pub fn encode_record_batch(
        &mut self,
        arrow_record_batch_vec: Vec<ArrowRecordBatch>,
    ) -> Result<usize> {
        if arrow_record_batch_vec.is_empty() {
            return Ok(0);
        }

        self.record_encoder.encode(arrow_record_batch_vec)
    }

    pub fn close(mut self) -> Result<Vec<u8>> {
        self.record_encoder.close()
    }
}

/// RecordDecoder is used for decoding ArrowRecordBatch based on
/// `schema.StorageFormat`
trait RecordDecoder {
    fn decode(&self, arrow_record_batch: ArrowRecordBatch) -> Result<ArrowRecordBatch>;
}

struct ColumnarRecordDecoder {}

impl RecordDecoder for ColumnarRecordDecoder {
    fn decode(&self, arrow_record_batch: ArrowRecordBatch) -> Result<ArrowRecordBatch> {
        Ok(arrow_record_batch)
    }
}

struct HybridRecordDecoder {
    storage_format_opts: StorageFormatOptions,
}

impl HybridRecordDecoder {
    /// Convert `ListArray` fields to underlying data type
    fn convert_schema(arrow_schema: ArrowSchemaRef) -> ArrowSchemaRef {
        let new_fields: Vec<_> = arrow_schema
            .fields()
            .iter()
            .map(|f| {
                if let DataType::List(nested_field) = f.data_type() {
                    Field::new(f.name(), nested_field.data_type().clone(), true)
                } else {
                    f.clone()
                }
            })
            .collect();
        Arc::new(ArrowSchema::new_with_metadata(
            new_fields,
            arrow_schema.metadata().clone(),
        ))
    }

    /// Stretch hybrid collpased column into columnar column.
    /// `value_offsets` specify offsets each value occupied, which means that
    /// the number of a `value[n]` is `value_offsets[n] - value_offsets[n-1]`.
    /// Ex:
    ///
    /// `array_ref` is `a b c`, `value_offsets` is `[0, 3, 5, 6]`, then
    /// output array is `a a a b b c`
    ///
    /// Note: caller should ensure offsets is not empty.
    fn stretch_variable_length_column(
        array_ref: &ArrayRef,
        value_offsets: &[i32],
    ) -> Result<ArrayRef> {
        assert_eq!(array_ref.len() + 1, value_offsets.len());

        let values_num = *value_offsets.last().unwrap() as usize;
        let offset_slices = array_ref.data().buffers()[0].as_slice();
        let value_slices = array_ref.data().buffers()[1].as_slice();
        let null_bitmap = array_ref.data().null_bitmap();
        trace!(
            "raw buffer slice, offsets:{:#02x?}, values:{:#02x?}, bitmap:{:#02x?}",
            offset_slices,
            value_slices,
            null_bitmap.map(|v| v.buffer_ref().as_slice())
        );

        let i32_offsets = Self::get_array_offsets(offset_slices);
        let mut value_bytes = 0;
        for (idx, (current, prev)) in i32_offsets[1..].iter().zip(&i32_offsets).enumerate() {
            let value_len = current - prev;
            let value_num = value_offsets[idx + 1] - value_offsets[idx];
            value_bytes += value_len * value_num;
        }

        // construct new expanded array
        let mut new_offsets_buffer = MutableBuffer::new(OFFSET_SIZE * values_num);
        let mut new_values_buffer = MutableBuffer::new(value_bytes as usize);
        let mut new_null_buffer = hybrid::new_ones_buffer(values_num);
        let null_slice = new_null_buffer.as_slice_mut();
        let mut value_length_so_far: i32 = 0;
        new_offsets_buffer.push(value_length_so_far);
        let mut bitmap_length_so_far: usize = 0;

        for (idx, (current, prev)) in i32_offsets[1..].iter().zip(&i32_offsets).enumerate() {
            let value_len = current - prev;
            let value_num = value_offsets[idx + 1] - value_offsets[idx];

            if let Some(bitmap) = null_bitmap {
                if !bitmap.is_set(idx) {
                    for i in 0..value_num {
                        bit_util::unset_bit(null_slice, bitmap_length_so_far + i as usize);
                    }
                }
            }
            bitmap_length_so_far += value_num as usize;
            new_values_buffer
                .extend(value_slices[*prev as usize..*current as usize].repeat(value_num as usize));
            for _ in 0..value_num {
                value_length_so_far += value_len;
                new_offsets_buffer.push(value_length_so_far);
            }
        }
        trace!(
            "new buffer slice, offsets:{:#02x?}, values:{:#02x?}, bitmap:{:#02x?}",
            new_offsets_buffer.as_slice(),
            new_values_buffer.as_slice(),
            new_null_buffer.as_slice(),
        );

        let array_data = ArrayData::builder(array_ref.data_type().clone())
            .len(values_num)
            .add_buffer(new_offsets_buffer.into())
            .add_buffer(new_values_buffer.into())
            .null_bit_buffer(Some(new_null_buffer.into()))
            .build()
            .map_err(|e| Box::new(e) as _)
            .context(DecodeRecordBatch)?;

        Ok(array_data.into())
    }

    /// Like `stretch_variable_length_column`, but array value is fixed-size
    /// type.
    ///
    /// Note: caller should ensure offsets is not empty.
    fn stretch_fixed_length_column(
        array_ref: &ArrayRef,
        value_size: usize,
        value_offsets: &[i32],
    ) -> Result<ArrayRef> {
        assert!(!value_offsets.is_empty());

        let values_num = *value_offsets.last().unwrap() as usize;
        let old_values_buffer = array_ref.data().buffers()[0].as_slice();
        let old_null_bitmap = array_ref.data().null_bitmap();

        let mut new_values_buffer = MutableBuffer::new(value_size * values_num);
        let mut new_null_buffer = hybrid::new_ones_buffer(values_num);
        let null_slice = new_null_buffer.as_slice_mut();
        let mut length_so_far = 0;

        for (idx, offset) in (0..old_values_buffer.len()).step_by(value_size).enumerate() {
            let value_num = (value_offsets[idx + 1] - value_offsets[idx]) as usize;
            if let Some(bitmap) = old_null_bitmap {
                if !bitmap.is_set(idx) {
                    for i in 0..value_num {
                        bit_util::unset_bit(null_slice, length_so_far + i as usize);
                    }
                }
            }
            length_so_far += value_num;
            new_values_buffer
                .extend(old_values_buffer[offset..offset + value_size].repeat(value_num))
        }
        let array_data = ArrayData::builder(array_ref.data_type().clone())
            .add_buffer(new_values_buffer.into())
            .null_bit_buffer(Some(new_null_buffer.into()))
            .len(values_num)
            .build()
            .map_err(|e| Box::new(e) as _)
            .context(DecodeRecordBatch)?;

        Ok(array_data.into())
    }

    /// Decode offset slices into Vec<i32>
    fn get_array_offsets(offset_slices: &[u8]) -> Vec<i32> {
        let mut i32_offsets = Vec::with_capacity(offset_slices.len() / OFFSET_SIZE);
        for i in (0..offset_slices.len()).step_by(OFFSET_SIZE) {
            let offset = i32::from_le_bytes(offset_slices[i..i + OFFSET_SIZE].try_into().unwrap());
            i32_offsets.push(offset);
        }

        i32_offsets
    }
}

impl RecordDecoder for HybridRecordDecoder {
    /// Decode records from hybrid to columnar format
    fn decode(&self, arrow_record_batch: ArrowRecordBatch) -> Result<ArrowRecordBatch> {
        let new_arrow_schema = Self::convert_schema(arrow_record_batch.schema());
        let arrays = arrow_record_batch.columns();

        let mut value_offsets = None;
        // Find value offsets from the first col in collapsible_cols_idx.
        if let Some(idx) = self.storage_format_opts.collapsible_cols_idx.first() {
            let offset_slices = arrays[*idx as usize].data().buffers()[0].as_slice();
            value_offsets = Some(Self::get_array_offsets(offset_slices));
        } else {
            CollapsibleColsIdxEmpty.fail()?;
        }

        let value_offsets = value_offsets.unwrap();
        let arrays = arrays
            .iter()
            .map(|array_ref| {
                let data_type = array_ref.data_type();
                match data_type {
                    // TODO:
                    // 1. we assume the datatype inside the List is primitive now
                    // Ensure this when create table
                    // 2. Although nested structure isn't support now, but may will someday in
                    // future. So We should keep metadata about which columns
                    // are collapsed by hybrid storage format, to differentiate
                    // List column in original records
                    DataType::List(_nested_field) => {
                        Ok(array_ref.data().child_data()[0].clone().into())
                    }
                    _ => {
                        let datum_kind = DatumKind::from_data_type(data_type).unwrap();
                        match datum_kind.size() {
                            None => Self::stretch_variable_length_column(array_ref, &value_offsets),
                            Some(value_size) => Self::stretch_fixed_length_column(
                                array_ref,
                                value_size,
                                &value_offsets,
                            ),
                        }
                    }
                }
            })
            .collect::<Result<Vec<_>>>()?;

        ArrowRecordBatch::try_new(new_arrow_schema, arrays)
            .map_err(|e| Box::new(e) as _)
            .context(EncodeRecordBatch)
    }
}

pub struct ParquetDecoder {
    record_decoder: Box<dyn RecordDecoder>,
}

impl ParquetDecoder {
    pub fn new(storage_format_opts: StorageFormatOptions) -> Self {
        let record_decoder: Box<dyn RecordDecoder> = match storage_format_opts.format {
            StorageFormat::Hybrid => Box::new(HybridRecordDecoder {
                storage_format_opts,
            }),
            StorageFormat::Columnar => Box::new(ColumnarRecordDecoder {}),
        };

        Self { record_decoder }
    }

    pub fn decode_record_batch(
        &self,
        arrow_record_batch: ArrowRecordBatch,
    ) -> Result<ArrowRecordBatch> {
        self.record_decoder.decode(arrow_record_batch)
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::{Int32Array, StringArray, TimestampMillisecondArray, UInt64Array};
    use common_types::{
        bytes::Bytes,
        column_schema,
        schema::{Builder, Schema, TSID_COLUMN},
        time::{TimeRange, Timestamp},
    };
    use parquet::{arrow::arrow_reader::ParquetRecordBatchReaderBuilder, file::footer};

    use super::*;
    use crate::table_options::StorageFormatOptions;

    fn build_schema() -> Schema {
        Builder::new()
            .auto_increment_column_id(true)
            .add_key_column(
                column_schema::Builder::new(TSID_COLUMN.to_string(), DatumKind::UInt64)
                    .build()
                    .unwrap(),
            )
            .unwrap()
            .add_key_column(
                column_schema::Builder::new("timestamp".to_string(), DatumKind::Timestamp)
                    .build()
                    .unwrap(),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("host".to_string(), DatumKind::String)
                    .is_tag(true)
                    .build()
                    .unwrap(),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("region".to_string(), DatumKind::String)
                    .is_tag(true)
                    .build()
                    .unwrap(),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("value".to_string(), DatumKind::Int32)
                    .build()
                    .unwrap(),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("string_value".to_string(), DatumKind::String)
                    .build()
                    .unwrap(),
            )
            .unwrap()
            .build()
            .unwrap()
    }

    fn string_array(values: Vec<Option<&str>>) -> ArrayRef {
        Arc::new(StringArray::from(values))
    }

    fn int32_array(values: Vec<Option<i32>>) -> ArrayRef {
        Arc::new(Int32Array::from(values))
    }

    fn timestamp_array(values: Vec<i64>) -> ArrayRef {
        Arc::new(TimestampMillisecondArray::from(values))
    }

    #[test]
    fn stretch_int32_column() {
        let testcases = [
            // (input, value_offsets, expected)
            (
                vec![Some(1), Some(2)],
                vec![0, 2, 4],
                vec![Some(1), Some(1), Some(2), Some(2)],
            ),
            (
                vec![Some(1), None, Some(2)],
                vec![0, 2, 4, 5],
                vec![Some(1), Some(1), None, None, Some(2)],
            ),
        ];

        for (input, value_offsets, expected) in testcases {
            let input = int32_array(input);
            let expected = int32_array(expected);
            let actual = HybridRecordDecoder::stretch_fixed_length_column(
                &input,
                std::mem::size_of::<i32>(),
                &value_offsets,
            )
            .unwrap();
            assert_eq!(
                actual.as_any().downcast_ref::<Int32Array>().unwrap(),
                expected.as_any().downcast_ref::<Int32Array>().unwrap(),
            );
        }
    }

    #[test]
    fn stretch_string_column() {
        let testcases = [
            // (input, value_offsets, values_num, expected)
            //
            // value with same length
            (
                vec![Some("a"), Some("b"), Some("c")],
                vec![0, 3, 5, 6],
                vec![
                    Some("a"),
                    Some("a"),
                    Some("a"),
                    Some("b"),
                    Some("b"),
                    Some("c"),
                ],
            ),
            // value with different length
            (
                vec![Some("hello"), Some("ceresdb")],
                vec![0, 1, 3],
                vec![Some("hello"), Some("ceresdb"), Some("ceresdb")],
            ),
            // value with none
            (
                vec![None, None, Some("hello"), None],
                vec![0, 1, 3, 4, 5],
                vec![None, None, None, Some("hello"), None],
            ),
        ];

        for (input, value_offsets, expected) in testcases {
            let input = string_array(input);
            let expected = string_array(expected);
            let actual =
                HybridRecordDecoder::stretch_variable_length_column(&input, &value_offsets)
                    .unwrap();
            assert_eq!(
                actual.as_any().downcast_ref::<StringArray>().unwrap(),
                expected.as_any().downcast_ref::<StringArray>().unwrap(),
            );
        }
    }

    fn collect_collapsible_cols_idx(schema: &Schema, collapsible_cols_idx: &mut Vec<u32>) {
        for (idx, _col) in schema.columns().iter().enumerate() {
            if schema.is_collapsible_column(idx) {
                collapsible_cols_idx.push(idx as u32);
            }
        }
    }

    #[test]
    fn test_hybrid_record_encode_and_decode() {
        let schema = build_schema();
        let storage_format_opts = StorageFormatOptions::new(StorageFormat::Hybrid);

        let mut meta_data = SstMetaData {
            min_key: Bytes::from_static(b"100"),
            max_key: Bytes::from_static(b"200"),
            time_range: TimeRange::new_unchecked(Timestamp::new(100), Timestamp::new(101)),
            max_sequence: 200,
            schema: schema.clone(),
            size: 10,
            row_num: 4,
            storage_format_opts,
            bloom_filter: Default::default(),
        };
        let mut encoder =
            HybridRecordEncoder::try_new(100, Compression::ZSTD, meta_data.clone()).unwrap();

        let columns = vec![
            Arc::new(UInt64Array::from(vec![1, 1, 2])) as ArrayRef,
            timestamp_array(vec![100, 101, 100]),
            string_array(vec![Some("host1"), Some("host1"), Some("host2")]),
            string_array(vec![Some("region1"), Some("region1"), Some("region2")]),
            int32_array(vec![Some(1), Some(2), Some(11)]),
            string_array(vec![
                Some("string_value1"),
                Some("string_value2"),
                Some("string_value3"),
            ]),
        ];

        let columns2 = vec![
            Arc::new(UInt64Array::from(vec![1, 2, 1, 2])) as ArrayRef,
            timestamp_array(vec![100, 101, 100, 101]),
            string_array(vec![
                Some("host1"),
                Some("host2"),
                Some("host1"),
                Some("host2"),
            ]),
            string_array(vec![
                Some("region1"),
                Some("region2"),
                Some("region1"),
                Some("region2"),
            ]),
            int32_array(vec![Some(1), Some(2), Some(11), Some(12)]),
            string_array(vec![
                Some("string_value1"),
                Some("string_value2"),
                Some("string_value3"),
                Some("string_value4"),
            ]),
        ];

        let input_record_batch =
            ArrowRecordBatch::try_new(schema.to_arrow_schema_ref(), columns).unwrap();
        let input_record_batch2 =
            ArrowRecordBatch::try_new(schema.to_arrow_schema_ref(), columns2).unwrap();
        let row_nums = encoder
            .encode(vec![input_record_batch, input_record_batch2])
            .unwrap();
        assert_eq!(2, row_nums);

        // read encoded records back, and then compare with input records
        let encoded_bytes = encoder.close().unwrap();
        let mut reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(encoded_bytes))
            .unwrap()
            .build()
            .unwrap();
        let hybrid_record_batch = reader.next().unwrap().unwrap();
        collect_collapsible_cols_idx(
            &meta_data.schema,
            &mut meta_data.storage_format_opts.collapsible_cols_idx,
        );

        let decoder = HybridRecordDecoder {
            storage_format_opts: meta_data.storage_format_opts,
        };
        let decoded_record_batch = decoder.decode(hybrid_record_batch).unwrap();

        // Note: decode record batch's schema doesn't have metadata
        // It's encoded in metadata of every fields
        // assert_eq!(decoded_record_batch.schema(), input_record_batch.schema());

        let expected_columns = vec![
            Arc::new(UInt64Array::from(vec![1, 1, 1, 1, 2, 2, 2])) as ArrayRef,
            timestamp_array(vec![100, 101, 100, 100, 100, 101, 101]),
            string_array(vec![
                Some("host1"),
                Some("host1"),
                Some("host1"),
                Some("host1"),
                Some("host2"),
                Some("host2"),
                Some("host2"),
            ]),
            string_array(vec![
                Some("region1"),
                Some("region1"),
                Some("region1"),
                Some("region1"),
                Some("region2"),
                Some("region2"),
                Some("region2"),
            ]),
            int32_array(vec![
                Some(1),
                Some(2),
                Some(1),
                Some(11),
                Some(11),
                Some(2),
                Some(12),
            ]),
            string_array(vec![
                Some("string_value1"),
                Some("string_value2"),
                Some("string_value1"),
                Some("string_value3"),
                Some("string_value3"),
                Some("string_value2"),
                Some("string_value4"),
            ]),
        ];

        let expect_record_batch =
            ArrowRecordBatch::try_new(schema.to_arrow_schema_ref(), expected_columns).unwrap();
        assert_eq!(
            decoded_record_batch.columns(),
            expect_record_batch.columns()
        );
    }

    #[test]
    fn test_hybrid_flush() {
        let schema = build_schema();
        let storage_format_opts = StorageFormatOptions::new(StorageFormat::Hybrid);

        let meta_data = SstMetaData {
            min_key: Bytes::from_static(b"100"),
            max_key: Bytes::from_static(b"200"),
            time_range: TimeRange::new_unchecked(Timestamp::new(100), Timestamp::new(101)),
            max_sequence: 200,
            schema: schema.clone(),
            size: 10,
            row_num: 4,
            storage_format_opts,
            bloom_filter: Default::default(),
        };
        let mut encoder = HybridRecordEncoder::try_new(10, Compression::ZSTD, meta_data).unwrap();

        let columns = vec![
            Arc::new(UInt64Array::from(vec![1, 1, 2])) as ArrayRef,
            timestamp_array(vec![100, 101, 100]),
            string_array(vec![Some("host1"), Some("host1"), Some("host2")]),
            string_array(vec![Some("region1"), Some("region1"), Some("region2")]),
            int32_array(vec![Some(1), Some(2), Some(11)]),
            string_array(vec![
                Some("string_value1"),
                Some("string_value2"),
                Some("string_value3"),
            ]),
        ];

        let columns2 = vec![
            Arc::new(UInt64Array::from(vec![1, 2, 1, 2])) as ArrayRef,
            timestamp_array(vec![100, 101, 100, 101]),
            string_array(vec![
                Some("host1"),
                Some("host2"),
                Some("host1"),
                Some("host2"),
            ]),
            string_array(vec![
                Some("region1"),
                Some("region2"),
                Some("region1"),
                Some("region2"),
            ]),
            int32_array(vec![Some(1), Some(2), Some(11), Some(12)]),
            string_array(vec![
                Some("string_value1"),
                Some("string_value2"),
                Some("string_value3"),
                Some("string_value4"),
            ]),
        ];

        let columns3 = vec![
            Arc::new(UInt64Array::from(vec![1, 2, 3, 4, 5, 6, 7, 8])) as ArrayRef,
            timestamp_array(vec![100, 101, 100, 100, 101, 100, 102, 103]),
            string_array(vec![
                Some("host1"),
                Some("host1"),
                Some("host2"),
                Some("host3"),
                Some("host4"),
                Some("host2"),
                Some("host3"),
                Some("host4"),
            ]),
            string_array(vec![
                Some("region1"),
                Some("region1"),
                Some("region2"),
                Some("region3"),
                Some("region1"),
                Some("region1"),
                Some("region2"),
                Some("region3"),
            ]),
            int32_array(vec![
                Some(1),
                Some(2),
                Some(11),
                Some(12),
                Some(1),
                Some(2),
                Some(11),
                Some(12),
            ]),
            string_array(vec![
                Some("string_value1"),
                Some("string_value2"),
                Some("string_value3"),
                Some("string_value4"),
                Some("string_value1"),
                Some("string_value2"),
                Some("string_value3"),
                Some("string_value4"),
            ]),
        ];

        let input_record_batch =
            ArrowRecordBatch::try_new(schema.to_arrow_schema_ref(), columns).unwrap();
        let input_record_batch2 =
            ArrowRecordBatch::try_new(schema.to_arrow_schema_ref(), columns2).unwrap();
        let row_nums = encoder
            .encode(vec![input_record_batch, input_record_batch2])
            .unwrap();
        assert_eq!(2, row_nums);

        let input_record_batch3 =
            ArrowRecordBatch::try_new(schema.to_arrow_schema_ref(), columns3).unwrap();
        let row_nums2 = encoder.encode(vec![input_record_batch3]).unwrap();
        assert_eq!(8, row_nums2);

        let sst = encoder.close().unwrap();
        let bytes = Bytes::from(sst);
        let parquet_metadata = footer::parse_metadata(&bytes).unwrap();
        assert_eq!(2, parquet_metadata.num_row_groups());
    }
}
