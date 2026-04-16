This is a tutorial to launch zaino, connected to a local validator.

Step 0: Git check out zaino.

Step 1: Set up zebra v3.1.0.
```
git clone git@github.com:ZcashFoundation/zebra.git
git checkout v3.1.0
cargo install --path zebrad --locked
```

EASY PATH:
Use included Testnet Configuration

In the zaino git root, run
```
zebrad -c example_configs/zebrad_config_3.1.0.toml
```
in another shell,
```
cargo run --release -- start -c example_configs/zainod.toml
```


