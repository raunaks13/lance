// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

use arrow::pyarrow::{PyArrowType, ToPyArrow};
use arrow_array::{Array, FixedSizeListArray};
use arrow_data::ArrayData;
use lance::index::vector::ivf::builder::write_vector_storage;
use lance::index::vector::ivf::io::write_pq_partitions;
use lance_index::vector::ivf::shuffler::{shuffle_vectors, load_partitioned_shuffles};
use lance_index::vector::{
    ivf::{storage::IvfModel, IvfBuildParams},
    pq::{PQBuildParams, ProductQuantizer},
};
use lance_linalg::distance::DistanceType;
use pyo3::{
    pyfunction,
    types::{PyList, PyModule},
    wrap_pyfunction, PyObject, PyResult, Python
};

use crate::fragment::FileFragment;
use crate::{dataset::Dataset, error::PythonErrorExt, file::object_store_from_uri_or_path, RT};
use lance_io::traits::WriteExt;
use lance_file::format::MAGIC;
use lance_index::pb::Index;
use lance::index::vector::ivf::IvfPQIndexMetadata;

async fn do_train_ivf_model(
    dataset: &Dataset,
    column: &str,
    dimension: usize,
    num_partitions: u32,
    distance_type: &str,
    sample_rate: u32,
    max_iters: u32,
) -> PyResult<ArrayData> {
    // We verify distance_type earlier so can unwrap here
    let distance_type = DistanceType::try_from(distance_type).unwrap();
    let params = IvfBuildParams {
        max_iters: max_iters as usize,
        sample_rate: sample_rate as usize,
        num_partitions: num_partitions as usize,
        ..Default::default()
    };
    let ivf_model = lance::index::vector::ivf::build_ivf_model(
        dataset.ds.as_ref(),
        column,
        dimension,
        distance_type,
        &params,
    )
    .await
    .infer_error()?;
    let centroids = ivf_model.centroids.unwrap();
    Ok(centroids.into_data())
}

#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn train_ivf_model(
    py: Python<'_>,
    dataset: &Dataset,
    column: &str,
    dimension: usize,
    num_partitions: u32,
    distance_type: &str,
    sample_rate: u32,
    max_iters: u32,
) -> PyResult<PyObject> {
    let centroids = RT.block_on(
        Some(py),
        do_train_ivf_model(
            dataset,
            column,
            dimension,
            num_partitions,
            distance_type,
            sample_rate,
            max_iters,
        ),
    )??;
    centroids.to_pyarrow(py)
}

#[allow(clippy::too_many_arguments)]
async fn do_train_pq_model(
    dataset: &Dataset,
    column: &str,
    dimension: usize,
    num_subvectors: u32,
    distance_type: &str,
    sample_rate: u32,
    max_iters: u32,
    ivf_model: IvfModel,
) -> PyResult<ArrayData> {
    // We verify distance_type earlier so can unwrap here
    let distance_type = DistanceType::try_from(distance_type).unwrap();
    let params = PQBuildParams {
        num_sub_vectors: num_subvectors as usize,
        num_bits: 8,
        max_iters: max_iters as usize,
        sample_rate: sample_rate as usize,
        ..Default::default()
    };
    let pq_model = lance::index::vector::pq::build_pq_model(
        dataset.ds.as_ref(),
        column,
        dimension,
        distance_type,
        &params,
        Some(&ivf_model),
    )
    .await
    .infer_error()?;
    Ok(pq_model.codebook.into_data())
}

#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn train_pq_model(
    py: Python<'_>,
    dataset: &Dataset,
    column: &str,
    dimension: usize,
    num_subvectors: u32,
    distance_type: &str,
    sample_rate: u32,
    max_iters: u32,
    ivf_centroids: PyArrowType<ArrayData>,
) -> PyResult<PyObject> {
    let ivf_centroids = ivf_centroids.0;
    let ivf_centroids = FixedSizeListArray::from(ivf_centroids);
    let ivf_model = IvfModel {
        centroids: Some(ivf_centroids),
        offsets: vec![],
        lengths: vec![],
    };
    let codebook = RT.block_on(
        Some(py),
        do_train_pq_model(
            dataset,
            column,
            dimension,
            num_subvectors,
            distance_type,
            sample_rate,
            max_iters,
            ivf_model,
        ),
    )??;
    codebook.to_pyarrow(py)
}

async fn do_transform_vectors(
    dataset: &Dataset,
    column: &str,
    distance_type: DistanceType,
    ivf_centroids: FixedSizeListArray,
    pq_model: ProductQuantizer,
    dst_uri: &str,
    fragments: Vec<FileFragment>,
) -> PyResult<()> {
    let num_rows = dataset.ds.count_rows(None).await.infer_error()?;
    let fragments = fragments.iter().map(|item| item.metadata().inner).collect();
    let transform_input = dataset
        .ds
        .scan()
        .with_fragments(fragments)
        .project(&[column])
        .infer_error()?
        .with_row_id()
        .batch_size(8192)
        .try_into_stream()
        .await
        .infer_error()?;

    let (obj_store, path) = object_store_from_uri_or_path(dst_uri).await?;
    let writer = obj_store.create(&path).await.infer_error()?;
    write_vector_storage(
        transform_input,
        num_rows as u64,
        ivf_centroids,
        pq_model,
        distance_type,
        column,
        writer,
    )
    .await
    .infer_error()?;
    Ok(())
}

#[pyfunction]
#[allow(clippy::too_many_arguments)]
pub fn transform_vectors(
    py: Python<'_>,
    dataset: &Dataset,
    column: &str,
    dimension: usize,
    num_subvectors: u32,
    distance_type: &str,
    ivf_centroids: PyArrowType<ArrayData>,
    pq_codebook: PyArrowType<ArrayData>,
    dst_uri: &str,
    fragments: Vec<FileFragment>,
) -> PyResult<()> {
    let ivf_centroids = ivf_centroids.0;
    let ivf_centroids = FixedSizeListArray::from(ivf_centroids);
    let codebook = pq_codebook.0;
    let codebook = FixedSizeListArray::from(codebook);
    let distance_type = DistanceType::try_from(distance_type).unwrap();
    let pq = ProductQuantizer::new(
        num_subvectors as usize,
        /*num_bits=*/ 8,
        dimension,
        codebook,
        distance_type,
    );
    RT.block_on(
        Some(py),
        do_transform_vectors(
            dataset,
            column,
            distance_type,
            ivf_centroids,
            pq,
            dst_uri,
            fragments,
        ),
    )?
}

async fn do_shuffle_transformed_vectors(
    unsorted_filenames: Vec<String>,
    dir_path: &str,
    ivf_centroids: FixedSizeListArray,
    shuffle_output_root_filename: &str,
) -> PyResult<Vec<String>> {
    let partition_files = shuffle_vectors(unsorted_filenames, dir_path, ivf_centroids, shuffle_output_root_filename)
        .await
        .infer_error()?;
    Ok(partition_files)
}

#[pyfunction]
#[allow(clippy::too_many_arguments)]
pub fn shuffle_transformed_vectors(
    py: Python<'_>,
    unsorted_filenames: Vec<String>,
    dir_path: &str,
    ivf_centroids: PyArrowType<ArrayData>,
    shuffle_output_root_filename: &str,
) -> PyResult<PyObject> {
    let ivf_centroids = ivf_centroids.0;
    let ivf_centroids = FixedSizeListArray::from(ivf_centroids);

    let result = RT.block_on(
        None,
        do_shuffle_transformed_vectors(unsorted_filenames, dir_path, ivf_centroids, shuffle_output_root_filename),
    )?;

    match result {
        Ok(partition_files) => {
            let py_list = PyList::new(py, partition_files);
            Ok(py_list.into())
        }
        Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e.to_string())),
    }
}

async fn do_load_shuffled_vectors(
    filenames: Vec<String>,
    dir_path: &str,
    dataset: &Dataset,
    column: &str,
    mut ivf_model: IvfModel,
    pq_model: ProductQuantizer,
) -> PyResult<()> {
    let (obj_store, path) = object_store_from_uri_or_path(dir_path).await?;
    let streams = load_partitioned_shuffles(path.clone(), filenames).await.infer_error()?;

    let obj_store = dataset.ds.object_store();
    let path = dataset.ds.indices_dir().child("shuffled_vectors.idx");
    let mut writer = obj_store.create(&path).await.infer_error()?;
    println!("Path to write: {:?}", path);
    write_pq_partitions(&mut writer, &mut ivf_model, Some(streams), None);

    let metadata = IvfPQIndexMetadata::new(
        "ivf_pq_index".to_string(),
        column.to_string(),
        ivf_model.dimension() as u32,
        dataset.ds.version().version,
        pq_model.distance_type,
        ivf_model,
        pq_model,
        vec![],
    );

    let metadata = Index::try_from(&metadata).infer_error()?;
    let pos = writer.write_protobuf(&metadata).await.infer_error()?;
    writer.write_magics(pos, 0, 1, MAGIC).await.infer_error()?;
    writer.shutdown().await.infer_error()?;

    Ok(())
}

#[pyfunction]
#[allow(clippy::too_many_arguments)]
pub fn load_shuffled_vectors(
    py: Python<'_>,
    filenames: Vec<String>,
    dir_path: &str,
    dataset: &Dataset,
    column: &str,
    ivf_centroids: PyArrowType<ArrayData>,
    pq_codebook: PyArrowType<ArrayData>,
    pq_dimension: usize,
    num_subvectors: u32, 
    distance_type: &str,
) -> PyResult<()> {
    let ivf_centroids = ivf_centroids.0;
    let ivf_centroids = FixedSizeListArray::from(ivf_centroids);

    let ivf_model = IvfModel {
        centroids: Some(ivf_centroids),
        offsets: vec![],
        lengths: vec![],
    };

    let codebook = pq_codebook.0;
    let codebook = FixedSizeListArray::from(codebook);

    let distance_type = DistanceType::try_from(distance_type).unwrap();
    let pq_model = ProductQuantizer::new(
        num_subvectors as usize,
        /*num_bits=*/ 8,
        pq_dimension,
        codebook,
        distance_type,
    );

    RT.block_on(
        None,
        do_load_shuffled_vectors(filenames, dir_path, dataset, column, ivf_model, pq_model),
    )?
}

pub fn register_indices(py: Python, m: &PyModule) -> PyResult<()> {
    let indices = PyModule::new(py, "indices")?;
    indices.add_wrapped(wrap_pyfunction!(train_ivf_model))?;
    indices.add_wrapped(wrap_pyfunction!(train_pq_model))?;
    indices.add_wrapped(wrap_pyfunction!(transform_vectors))?;
    indices.add_wrapped(wrap_pyfunction!(shuffle_transformed_vectors))?;
    indices.add_wrapped(wrap_pyfunction!(load_shuffled_vectors))?;
    m.add_submodule(indices)?;
    Ok(())
}
