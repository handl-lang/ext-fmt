# ext-fmt

`hnl fmt` as a dynamic extension — `libhnl_ext_fmt.{so,dylib,dll}`
(ADR-0149).

The formatter logic lives in `handl-plugin-ext-fmt` (platform); this repo
is the packaging: `hnl-ext-fmt` exports the ADR-0149 ABI
(`handl_ext_manifest` / `handl_ext_dispatch` / `handl_ext_free`) and
implements the `fmt [--check] [--write] [paths...]` command surface with
the same semantics the old built-in `hnl fmt` had (exit 0/1/2, `not
canonical` reporting, `--write` atomic rewrite, stdin when no paths,
dialect-file skip).

```bash
cargo build                        # target/debug/libhnl_ext_fmt.so

# point the host at it:
HANDL_EXT_DIR=$PWD/target/debug hnl fmt file.hnl
HANDL_EXT_DIR=$PWD/target/debug hnl fmt --check file.hnl
```

Platform/kernel/lang crates are git dependencies (`branch = "main"`); the
workspace-root `[patch]` tables resolve them from sibling checkouts for
local development. To build without the remotes, rewrite the git URLs to
the local clones (same recipe as the other extracted repos):

```bash
CARGO_NET_GIT_FETCH_WITH_CLI=true \
GIT_CONFIG_COUNT=1 \
GIT_CONFIG_KEY_0='url.file:///home/luis/GitHub/handl-lang/.insteadOf' \
GIT_CONFIG_VALUE_0='https://github.com/handl-lang/' \
cargo build
```
