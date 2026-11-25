# ext-fmt

`hnl-ext-fmt` — `hnl fmt` as a dynamic `hnl` extension: a
`libhnl_ext_fmt.{so,dylib,dll}` cdylib the dumb `hnl` host discovers at
runtime. The formatter logic lives in `handl-plugin-ext-fmt`
(platform); this repo is the packaging — it exports the extension ABI
(`handl_ext_manifest` / `handl_ext_dispatch` / `handl_ext_free`) and
implements the `fmt` command surface.

## Layout

| path | role |
| --- | --- |
| `src/` | extension ABI exports + `fmt` command wiring |
| `xtask/` | vendored gate crate (`fmt`, `clippy`, `test`, `doc`, `gate`) |
| `workflows/gate.yaml` | per-push gate — `handl-gates` rust campaign via bupha |

## Contract

- Commands: `hnl fmt [--check] [--write] [paths...]` — stdin when no
  paths; skips dialect files; `--write` rewrites atomically.
- Exit codes: `0` ok, `1` not canonical / write errors, `2`
  usage/read errors.
- Builds `cdylib` + `rlib`; `cargo test` links the `rlib` statically.

## Verification

```bash
cargo run --manifest-path xtask/Cargo.toml -- gate   # fmt → clippy → test → doc
```

<!-- myco:agent-awareness -->

## Agent awareness

- Session start / any work discussion: run `myco status --prompt`
  before proposing next steps — know the board first.
- User describes new work → offer to capture it (`myco todo add`,
  `issue new`, `proposal new`). Don't wait to be asked.
- Picking up a task: `myco resume <ID>` is the briefing.
- Wrapping up: `myco handoff <ID>` anything resumable; keep statuses
  honest (claimed → in-progress → blocked/done).

<!-- /myco:agent-awareness -->
