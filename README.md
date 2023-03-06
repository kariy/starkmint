# Starkmint

## Getting started

### Setting up the Rollup

#### DA layer

Run Celestia consensus and DA bridge nodes.

```sh
cd local-da
docker compose -f ./docker/test-docker-compose.yml up
```

#### App Layer

```sh
cd starkmint
cargo run --bin starkmint
```

#### Rollkit

Install `rollkit/tendermint`.

```sh
git clone https://github.com/rollkit/tendermint.git
cd tendermint
git checkout 8be9b54c8c21
make install
```

Build Rollkit node.

```sh
cd rollkit-node
go build
```

Run Rollkit.

```sh
TMHOME="/tmp/starkmint" tendermint init
NAMESPACE_ID=$(echo $RANDOM | md5sum | head -c 16; echo;)
./rollkit-node -config "/tmp/starkmint/config/config.toml" -rollkit.namespace_id $NAMESPACE_ID -rollkit.da_start_height 1
```

That's it.

### Send an execution

To send executions to the sequencer you need to have a compiled Cairo program (\*.json files in the repo). Then you can send them like so:

```bash
cargo run --bin cli -- examples/programs/fibonacci.json main
```
