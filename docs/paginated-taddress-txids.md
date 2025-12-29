# GetTaddressTxidsPaginated RPC

A Zaino-specific extension to the LightWallet Protocol that adds pagination support for transparent address transaction queries.

## Problem

The standard `GetTaddressTxids` RPC returns ALL transactions for a transparent address within a block range. For addresses with many transactions (500+), this causes:

- Slow page loads (1-2 minutes for 10K+ transactions)
- Wasted bandwidth when only 20 transactions are needed for a page
- Poor user experience in wallet UIs

## Solution

`GetTaddressTxidsPaginated` adds:

- `maxEntries` - Limit number of results returned
- `reverse` - Return newest transactions first
- `totalCount` - Total matching transactions (for pagination UI)
- `blockHeight` - Height in each response (for cursor-based pagination)

## Performance

| Method | Transactions | Time |
|--------|-------------|------|
| Paginated | 20 | 0.19s |
| Paginated | 100 | 0.79s |
| Original | 10,543 (all) | 84.5s |

For typical page loads (20 transactions), this is **~450x faster**.

## Proto Definition

Location: `zaino-proto/lightwallet-protocol/walletrpc/service.proto`

### Request Message

```protobuf
// Request for paginated transparent address transaction lookup.
// Results are sorted by height, enabling cursor-based pagination.
message GetTaddressTxidsPaginatedArg {
    string address = 1;      // t-address to query
    uint64 startHeight = 2;  // Start height (inclusive), 0 = genesis
    uint64 endHeight = 3;    // End height (inclusive), 0 = chain tip
    uint32 maxEntries = 4;   // Max transactions to return, 0 = unlimited
    bool reverse = 5;        // If true, return newest transactions first
}
```

### Response Message

```protobuf
// Response for paginated transparent address transaction lookup.
// The first response in the stream includes totalCount; subsequent responses have 0.
message PaginatedTxidsResponse {
    RawTransaction transaction = 1;  // The transaction data
    uint64 blockHeight = 2;          // Height where tx was mined (for cursor)
    uint32 txIndex = 3;              // Index within block
    uint64 totalCount = 4;           // Total transactions matching query (first response only)
    bytes txid = 5;                  // Transaction ID (32 bytes, little-endian)
}
```

### RPC Method

```protobuf
service CompactTxStreamer {
    // ... existing methods ...

    // Return paginated transactions for a t-address within a block range.
    // Supports limiting results, reverse ordering, and includes total count for pagination UI.
    rpc GetTaddressTxidsPaginated(GetTaddressTxidsPaginatedArg) returns (stream PaginatedTxidsResponse) {}
}
```

## Usage Examples

### Using grpcurl

```bash
# First page (20 newest transactions)
grpcurl -plaintext \
  -import-path zaino-proto/lightwallet-protocol/walletrpc \
  -proto service.proto \
  -d '{
    "address": "tmB2ZGgLAh75Ho7JqFPKKhCoqgAouAWokRv",
    "maxEntries": 20,
    "reverse": true
  }' \
  localhost:8137 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTaddressTxidsPaginated
```

### Response Format

```json
{
  "transaction": {
    "data": "BQAAgAo...",
    "height": "3757408"
  },
  "blockHeight": "3757408",
  "totalCount": "10543",
  "txid": "r9cTEgVy6AIkJOJ/d2hwiO8b4QoLcMk6tKM5xQzcBnk="
}
{
  "transaction": {
    "data": "BQAAgAo...",
    "height": "3757407"
  },
  "blockHeight": "3757407",
  "txid": "g3FjG0myegCrsZ3xnPN3n75q2YDJQAtY0glNg3q4IYQ="
}
```

Note: `totalCount` is only included in the first response. `txid` is base64-encoded (32 bytes, little-endian).

## Cursor-Based Pagination

Use `blockHeight` from the last response to fetch the next page:

### Forward Pagination (oldest first)

```bash
# Page 1
{ "address": "t1...", "maxEntries": 20, "reverse": false }
# Response ends with blockHeight: 3705586

# Page 2 - use last height + 1 as startHeight
{ "address": "t1...", "startHeight": 3705587, "maxEntries": 20, "reverse": false }
```

### Reverse Pagination (newest first)

```bash
# Page 1
{ "address": "t1...", "maxEntries": 20, "reverse": true }
# Response ends with blockHeight: 3757392

# Page 2 - use last height - 1 as endHeight
{ "address": "t1...", "endHeight": 3757391, "maxEntries": 20, "reverse": true }
```

## Client Integration

### Rust (using zaino-proto crate)

```rust
use zaino_proto::proto::service::{
    compact_tx_streamer_client::CompactTxStreamerClient,
    GetTaddressTxidsPaginatedArg,
};

let mut client = CompactTxStreamerClient::connect("http://localhost:8137").await?;

let request = GetTaddressTxidsPaginatedArg {
    address: "tmB2ZGgLAh75Ho7JqFPKKhCoqgAouAWokRv".to_string(),
    start_height: 0,
    end_height: 0,  // 0 = chain tip
    max_entries: 20,
    reverse: true,
};

let mut stream = client.get_taddress_txids_paginated(request).await?.into_inner();

let mut total_count = 0;
while let Some(response) = stream.message().await? {
    if response.total_count > 0 {
        total_count = response.total_count;
        println!("Total transactions: {}", total_count);
    }
    let txid_hex = hex::encode(&response.txid);
    println!("TX {} at height {}", txid_hex, response.block_height);
}
```

### Other Languages

Generate client code from the proto file using your language's protoc plugin:

```bash
# Python
python -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. service.proto

# Go
protoc --go_out=. --go-grpc_out=. service.proto

# JavaScript/TypeScript
grpc_tools_node_protoc --js_out=. --grpc_out=. service.proto
```

## Parameters Reference

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `address` | string | required | Transparent address (t-address) to query |
| `startHeight` | uint64 | 0 | Start block height (inclusive). 0 = genesis |
| `endHeight` | uint64 | 0 | End block height (inclusive). 0 = chain tip |
| `maxEntries` | uint32 | 0 | Maximum transactions to return. 0 = unlimited |
| `reverse` | bool | false | If true, return newest transactions first |

## Response Fields

| Field | Type | Description |
|-------|------|-------------|
| `transaction` | RawTransaction | Full transaction data (same as GetTaddressTxids) |
| `blockHeight` | uint64 | Block height where transaction was mined |
| `txIndex` | uint32 | Transaction index within the block |
| `totalCount` | uint64 | Total matching transactions (first response only, 0 otherwise) |
| `txid` | bytes | Transaction ID (32 bytes, little-endian) |

## Compatibility

- This is a **Zaino-specific extension** to the LightWallet Protocol
- The original `GetTaddressTxids` RPC remains unchanged and fully supported
- Existing clients continue to work without modification
- New clients can opt into pagination for better performance

## Files

| File | Description |
|------|-------------|
| `zaino-proto/lightwallet-protocol/walletrpc/service.proto` | Proto definitions |
| `zaino-proto/src/proto/service.rs` | Generated Rust types |
| `zaino-state/src/stream.rs` | `PaginatedTxidsStream` type |
| `zaino-state/src/indexer.rs` | `LightWalletIndexer` trait |
| `zaino-state/src/backends/fetch.rs` | FetchService implementation |
| `zaino-state/src/backends/state.rs` | StateService implementation |
| `zaino-serve/src/rpc/grpc/service.rs` | gRPC endpoint wiring |
