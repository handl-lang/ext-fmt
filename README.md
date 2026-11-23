# ext-fmt

`hnl-ext-fmt` — `hnl fmt` as a dynamic `hnl` extension.

An `ext-*` crate: a `libhnl_ext_fmt.{so,dylib,dll}` shared library for the
dumb `hnl` host. The formatter logic lives in `handl-plugin-ext-fmt`
(platform); this repo is the packaging — `hnl-ext-fmt` exports the extension
ABI (`handl_ext_manifest` / `handl_ext_dispatch` / `handl_ext_free`) and
implements the `fmt [--check] [--write] [paths...]` command surface.

## Commands

```text
hnl fmt [--check] [--write] [paths...]
```

Reads stdin when no paths are given; skips dialect files; `--write` rewrites
files atomically. Exit codes: `0` ok, `1` not canonical / write errors,
`2` usage/read errors.

## Development

```bash
cargo run --manifest-path xtask/Cargo.toml -- gate   # fmt → clippy → test → doc
```

`xtask/` provides the gate tasks (`fmt`, `clippy`, `test`, `doc`, `check`,
`gate`); `ci.sh` is the same gate entry for the bupha shell runner. The crate
builds a `cdylib` + `rlib`; `cargo test` links the `rlib` statically.

```bash
cargo build                        # target/debug/libhnl_ext_fmt.so

# point the host at it:
HANDL_EXT_DIR=$PWD/target/debug hnl fmt file.hnl
HANDL_EXT_DIR=$PWD/target/debug hnl fmt --check file.hnl
```

Deps are git deps on `branch = "main"`; locally the workspace-root
`.cargo/config.toml` patches them to sibling checkouts (dev-only, never
committed).

## License

MIT.
