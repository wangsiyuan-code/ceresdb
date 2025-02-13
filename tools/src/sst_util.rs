// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

use analytic_engine::sst::{file::SstMetaData, parquet::encoding};
use object_store::{ObjectStoreRef, Path};
use parquet::file::footer;

/// Extract the meta data from the sst file.
pub async fn meta_from_sst(store: &ObjectStoreRef, sst_path: &Path) -> SstMetaData {
    let get_result = store.get(sst_path).await.unwrap();
    let chunk_reader = get_result.bytes().await.unwrap();
    let metadata = footer::parse_metadata(&chunk_reader).unwrap();
    let kv_metas = metadata.file_metadata().key_value_metadata().unwrap();

    encoding::decode_sst_meta_data(&kv_metas[0]).unwrap()
}
