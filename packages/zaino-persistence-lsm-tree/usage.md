# zaino-persistence-lsm-tree

The on-disk backend for the `zaino-persistence` port: positional files for
sequence namespaces, an LSM tree for map namespaces, and one checksummed
manifest as the atomic commit point. Under construction: the file layer
(`fs`), page checksums, the manifest and the index directory are in place;
the LSM, sequences and the `Backend` adapter follow.
