-- mlkmeans extension smoke: ml_kmeans(x, y, k) computes k-means centroids via
-- Python/numpy on the single resident, shared compose:dynlink pylon provider.
--
-- REQUIRES the pylon provider registered via the DUCKLINK_PROVIDERS env var:
--   DUCKLINK_PROVIDERS=pylon=/abs/pylon-endpoint-numpy.component.wasm:/lib=/abs/cpython/Lib;/app=/abs/pylib
-- Two well-separated clusters -> centroids near (0.05,0) and (5.05,5),
-- returned as a JSON string. The point order in the result is
-- seeding-deterministic (the provider uses a fixed-seed k-means++).
SELECT ml_kmeans(x, y, 2) AS centroids
FROM (VALUES (0.0,0.0),(0.1,0.0),(5.0,5.0),(5.1,5.0)) t(x,y);
