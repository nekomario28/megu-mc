# Neighbor finalization

First contract: the exposed-face sequence is exactly native DiscreteVoxelShape,
independent of insertion order, duplicate writes, cavities and disconnected cells.
Extract the existing tree traversal semantics into one geometry owner and route
both tree and template through it. Check actual native forAllFaces output and
existing native tree edge cases. Kill axis/coordinate/face-order mutants through
the shared owner. Do not claim block-state finalization from face ordering alone.

Next contract: invoke native updateShapeAtEdge/updateFromNeighbourShapes on
scripted levels with nontrivial neighbors, capture state writes and ordered
block/fluid reservations, and isolate each first divergence. Inventory existing
runtime owners before adding a state-transition module. Unknown native operations
must fail, not silently become no-ops. Cover waterlogged, connected, fragile,
falling and mixed-fluid states; do not assume a full-cube output makes neighbors
unchangeable. Production callers and both-template write visibility stay pinned.
