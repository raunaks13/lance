# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: Copyright The Lance Authors
import lance
import numpy as np
import pyarrow as pa
import pytest
from lance.file import LanceFileReader
from lance.indices import IndicesBuilder, IvfModel, PqModel

NUM_ROWS_PER_FRAGMENT = 10000
DIMENSION = 128
NUM_SUBVECTORS = 8
NUM_FRAGMENTS = 3
NUM_ROWS = NUM_ROWS_PER_FRAGMENT * NUM_FRAGMENTS
NUM_PARTITIONS = round(np.sqrt(NUM_ROWS))


@pytest.fixture(params=[np.float16, np.float32, np.float64], ids=["f16", "f32", "f64"])
def rand_dataset(tmpdir, request):
    vectors = np.random.randn(NUM_ROWS, DIMENSION).astype(request.param)
    vectors.shape = -1
    vectors = pa.FixedSizeListArray.from_arrays(vectors, DIMENSION)
    table = pa.Table.from_arrays([vectors], names=["vectors"])
    uri = str(tmpdir / "dataset")

    ds = lance.write_dataset(table, uri, max_rows_per_file=NUM_ROWS_PER_FRAGMENT)

    return ds


def test_ivf_centroids(tmpdir, rand_dataset):
    ivf = IndicesBuilder(rand_dataset, "vectors").train_ivf(sample_rate=16)

    assert ivf.distance_type == "l2"
    assert len(ivf.centroids) == NUM_PARTITIONS

    ivf.save(str(tmpdir / "ivf"))
    reloaded = IvfModel.load(str(tmpdir / "ivf"))
    assert reloaded.distance_type == "l2"
    assert ivf.centroids == reloaded.centroids


@pytest.mark.cuda
def test_ivf_centroids_cuda(rand_dataset):
    ivf = IndicesBuilder(rand_dataset, "vectors").train_ivf(
        sample_rate=16, accelerator="cuda"
    )

    assert ivf.distance_type == "l2"
    assert len(ivf.centroids) == NUM_PARTITIONS


def test_ivf_centroids_distance_type(tmpdir, rand_dataset):
    def check(distance_type):
        ivf = IndicesBuilder(rand_dataset, "vectors").train_ivf(
            sample_rate=16, distance_type=distance_type
        )
        assert ivf.distance_type == distance_type
        ivf.save(str(tmpdir / "ivf"))
        reloaded = IvfModel.load(str(tmpdir / "ivf"))
        assert reloaded.distance_type == distance_type

    check("l2")
    check("cosine")
    check("dot")


def test_num_partitions(rand_dataset):
    ivf = IndicesBuilder(rand_dataset, "vectors").train_ivf(
        sample_rate=16, num_partitions=10
    )
    assert ivf.num_partitions == 10


@pytest.fixture
def rand_ivf(rand_dataset):
    dtype = rand_dataset.schema.field("vectors").type.value_type.to_pandas_dtype()
    centroids = np.random.rand(DIMENSION * 100).astype(dtype)
    centroids = pa.FixedSizeListArray.from_arrays(centroids, DIMENSION)
    return IvfModel(centroids, "l2")


def test_gen_pq(tmpdir, rand_dataset, rand_ivf):
    pq = IndicesBuilder(rand_dataset, "vectors").train_pq(rand_ivf, sample_rate=2)
    assert pq.dimension == DIMENSION
    assert pq.num_subvectors == NUM_SUBVECTORS

    pq.save(str(tmpdir / "pq"))
    reloaded = PqModel.load(str(tmpdir / "pq"))
    assert pq.dimension == reloaded.dimension
    assert pq.codebook == reloaded.codebook


@pytest.mark.cuda
def test_assign_partitions(rand_dataset, rand_ivf):
    builder = IndicesBuilder(rand_dataset, "vectors")

    partitions_uri = builder.assign_ivf_partitions(rand_ivf, accelerator="cuda")

    partitions = lance.dataset(partitions_uri)
    found_row_ids = set()
    for batch in partitions.to_batches():
        row_ids = batch["row_id"]
        for row_id in row_ids:
            found_row_ids.add(row_id)
        part_ids = batch["partition"]
        for part_id in part_ids:
            assert part_id.as_py() < 100
    assert len(found_row_ids) == rand_dataset.count_rows()


@pytest.fixture
def rand_pq(rand_dataset, rand_ivf):
    dtype = rand_dataset.schema.field("vectors").type.value_type.to_pandas_dtype()
    codebook = np.random.rand(DIMENSION * 256).astype(dtype)
    codebook = pa.FixedSizeListArray.from_arrays(codebook, DIMENSION)
    pq = PqModel(NUM_SUBVECTORS, codebook)
    return pq


def test_vector_transform(tmpdir, rand_dataset, rand_ivf, rand_pq):
    fragments = list(rand_dataset.get_fragments())

    builder = IndicesBuilder(rand_dataset, "vectors")
    uri = str(tmpdir / "transformed")
    builder.transform_vectors(rand_ivf, rand_pq, uri, fragments=fragments)

    reader = LanceFileReader(uri)

    assert reader.metadata().num_rows == (NUM_ROWS_PER_FRAGMENT * len(fragments))
    data = next(reader.read_all(batch_size=10000).to_batches())

    row_id = data.column("_rowid")
    assert row_id.type == pa.uint64()

    pq_code = data.column("__pq_code")
    assert pq_code.type == pa.list_(pa.uint8(), 8)

    part_id = data.column("__ivf_part_id")
    assert part_id.type == pa.uint32()

    # test when fragments = None
    builder.transform_vectors(rand_ivf, rand_pq, uri, fragments=None)
    reader = LanceFileReader(uri)

    assert reader.metadata().num_rows == (NUM_ROWS_PER_FRAGMENT * NUM_FRAGMENTS)
