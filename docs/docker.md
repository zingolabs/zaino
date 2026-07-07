# Docker Usage

Image: `hhanh00/zaino:latest`

Binaries in the image: `zainod`, `zaino-admin`.

## Volumes

| Mount point   | Purpose              |
|---------------|----------------------|
| `/app/config` | Config directory     |
| `/app/data`   | LMDB data directory  |

## Config file

Minimal `zainod.toml` (place in the directory you mount to `/app/config`):

```toml
backend = 'fetch'
network = 'Mainnet'
block_store_max_concurrency = 8
start_height = 0

[grpc_settings]
listen_address = '127.0.0.1:9067'

[validator_settings]
validator_jsonrpc_listen_address = '127.0.0.1:8232'
validator_user = 'xxxxxx'
validator_password = 'xxxxxx'

[storage.database]
path = '/app/data'
size = 384
```

## Setup

Create the data directory:

```bash
mkdir -p ./data
```

## Bootstrap

Load blocks from a local Zebra RocksDB into the zaino LMDB store:

```bash
docker run --network host -v ./data:/app/data -v .:/app/config -v ~/.cache/zebra:/app/zebra --entrypoint zaino-admin hhanh00/zaino:latest bootstrap /app/zebra
```

## Run the server

```bash
docker run -d --network host -v ./data:/app/data -v .:/app/config hhanh00/zaino:latest -c /app/config/zainod.toml start
```

The default entrypoint runs `zainod` and forwards all arguments, so
`-c /app/config/zainod.toml start` is equivalent to
`zainod -c /app/config/zainod.toml start`.

## Running other tools

Override `--entrypoint` with `zaino-admin` and run the tool subcommand:

Ex: to compare blocks 3300000 to tip between zaino and zec.rocks

```bash
docker run --network host -v ./data:/app/data -v .:/app/config --entrypoint zaino-admin hhanh00/zaino:latest compare --start-height 3300000 --server-a http://localhost:9067 --server-b https://zec.rocks
```

Note: container runs in "network host" mode where the container has access to the ports of the host. It could be run in bridge mode but the network configuration is more complex.
