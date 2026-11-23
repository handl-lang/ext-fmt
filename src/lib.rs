//! `hnl-ext-fmt` — the `fmt` extension for `hnl` (ADR-0149).
//!
//! The formatter logic lives in `handl-plugin-ext-fmt` (platform); this
//! crate is the *packaging*: it exports the ADR-0149 ABI
//! (`handl_ext_manifest` / `handl_ext_dispatch` / `handl_ext_free`) and
//! implements the `hnl fmt [--check] [--write] [paths...]` command
//! surface — byte-for-byte the semantics the built-in `fmt` arm had before
//! the host went dumb.
//!
//! Internally the extension is a *host of plugins* like any other
//! (ADR-0149 §Consequences): a `DefaultHost` running the `fmt` capability
//! of [`handl_plugin_ext_fmt::FmtPlugin`], with the canonical [`Language`] injected from
//! `handl-kit-data-prelude` (the same data the old host's kit supplied).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use handl_ext::{ExtRequest, ExtResponse, ExtensionManifest};
use handl_language::Language;
use handl_platform::{CapabilityRequest, DefaultHost};
use handl_plugin_ext_detect::{Dialect, detect_dialect};
use handl_plugin_ext_diag::render_diagnostic;
use serde_json::json;

handl_ext::export!(
    ExtensionManifest::new("fmt")
        .version(env!("CARGO_PKG_VERSION"))
        .command(
            "fmt",
            "fmt [--check] [--write] [--language <name>] [paths...]"
        ),
    dispatch
);

/// ABI dispatch: only the `fmt` command is declared.
fn dispatch(cmd: &str, req: &ExtRequest) -> ExtResponse {
    match cmd {
        "fmt" => run(&req.args, Path::new(&req.cwd)),
        other => ExtResponse::error(2, format!("fmt: unknown command `{other}`\n")),
    }
}

// ---------------------------------------------------------------------------
// `fmt [--check] [--write] [--prelude <path>] [--language <name>] [paths...]`
// ---------------------------------------------------------------------------

/// Output accumulator: the extension renders one `ExtResponse` instead of
/// printing, so stdout/stderr collect here and the code is returned.
#[derive(Default)]
struct Out {
    stdout: String,
    stderr: String,
    had_failure: bool,
}

/// Run the `fmt` command, resolving relative paths against `cwd`.
fn run(args: &[String], cwd: &Path) -> ExtResponse {
    let mut check = false;
    let mut write = false;
    let mut language: Option<String> = None;
    let mut paths = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--check" => check = true,
            "--write" => write = true,
            "--prelude" => {
                // Accepted for CLI consistency with `lint`; it has no effect
                // on formatting.
                i += 1;
                if i >= args.len() {
                    return err("fmt: --prelude requires a value\n");
                }
            }
            "--language" => {
                i += 1;
                if i >= args.len() {
                    return err("fmt: --language requires a value\n");
                }
                language = Some(args[i].clone());
            }
            "--" => {}
            s if s.starts_with("--") => {
                return err(format!("fmt: unknown flag `{s}`\n"));
            }
            s => paths.push(s.to_string()),
        }
        i += 1;
    }

    if check && write {
        return err("fmt: --check and --write are mutually exclusive\n");
    }
    if let Some(lang) = language
        && lang != "canonical"
    {
        return err(
            "custom languages are not yet supported by the CLI; use the programmatic `_with_language` APIs\n",
        );
    }

    // One host for the whole command — the same capability path a
    // third-party consumer uses (ADR-0085 wiring, step 4).
    let mut host = match tooling_host() {
        Ok(h) => h,
        Err(e) => return err(format!("fmt: tooling host unavailable: {e}\n")),
    };

    let mut out = Out::default();

    if paths.is_empty() {
        // Read from stdin.
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_err() {
            return err("fmt: failed to read stdin\n");
        }
        if fmt_one(
            &mut host,
            &src,
            "<stdin>",
            Path::new("<stdin>"),
            check,
            write,
            &mut out,
        ) == 2
        {
            return ExtResponse {
                code: 2,
                stdout: out.stdout,
                stderr: out.stderr,
            };
        }
        return finish(out);
    }

    for path in &paths {
        let disk = resolve(cwd, path);
        let src = match std::fs::read_to_string(&disk) {
            Ok(s) => s,
            Err(e) => {
                out.stderr
                    .push_str(&format!("fmt: failed to read {path}: {e}\n"));
                return ExtResponse {
                    code: 2,
                    stdout: out.stdout,
                    stderr: out.stderr,
                };
            }
        };
        if fmt_one(&mut host, &src, path, &disk, check, write, &mut out) == 2 {
            return ExtResponse {
                code: 2,
                stdout: out.stdout,
                stderr: out.stderr,
            };
        }
    }

    finish(out)
}

/// A host with the `fmt` plugin registered — the extension consumes the
/// same `DefaultHost`/`CapabilityRequest` machinery the built-in command
/// did; only the packaging changed (ADR-0149 §Consequences).
#[allow(clippy::result_large_err)]
fn tooling_host() -> Result<DefaultHost, handl_platform::PlatformError> {
    let mut host = DefaultHost::new();
    host.register_plugin(handl_plugin_ext_fmt::FmtPlugin::new(canonical_lang()));
    host.load()?;
    host.resolve_composition()
        .map_err(|e| handl_platform::PlatformError::Plugin(format!("tooling composition: {e}")))?;
    Ok(host)
}

/// The canonical language, assembled once per process.
fn canonical_lang() -> Arc<Language> {
    static LANG: std::sync::OnceLock<Arc<Language>> = std::sync::OnceLock::new();
    LANG.get_or_init(|| {
        Arc::new(
            handl_kit_data_prelude::language_from_yaml().expect("canonical language must assemble"),
        )
    })
    .clone()
}

/// Resolve `path` (possibly relative) against the request's `cwd`.
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Format a single source. Returns 2 on parse/dispatch error (and sets
/// `out.had_failure` for check failures); otherwise 0.
fn fmt_one(
    host: &mut DefaultHost,
    src: &str,
    path: &str,
    disk: &Path,
    check: bool,
    write: bool,
    out: &mut Out,
) -> i32 {
    if detect_dialect(src) != Dialect::Canonical {
        out.stderr
            .push_str(&format!("skipping dialect file: {path}\n"));
        return 0;
    }

    let request = CapabilityRequest {
        capability: "fmt".into(),
        payload: json!({ "source": src, "check": check }),
    };
    let response = match host.run(&request) {
        Ok(r) => r,
        Err(e) => {
            out.stderr.push_str(&format!("fmt: {path}: {e}\n"));
            return 2;
        }
    };

    match response.0.get("status").and_then(|v| v.as_str()) {
        Some("canonical") => 0,
        Some("not-canonical") => {
            out.stdout.push_str(&format!("{path}: not canonical\n"));
            out.had_failure = true;
            0
        }
        Some("formatted") => {
            let formatted = response
                .0
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if write {
                if let Err(e) = write_atomic(disk, formatted) {
                    out.stderr
                        .push_str(&format!("fmt: failed to write {path}: {e}\n"));
                    return 2;
                }
            } else {
                out.stdout.push_str(formatted);
            }
            0
        }
        Some("error") => {
            let diag: handl_plugin_ext_diag::Diagnostic = response
                .0
                .get("diagnostic")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            out.stderr.push_str(&render_diagnostic(&diag, path));
            out.stderr.push('\n');
            2
        }
        other => {
            out.stderr.push_str(&format!(
                "fmt: {path}: unexpected capability status {other:?}\n"
            ));
            2
        }
    }
}

/// Write `content` to `target` atomically (temp file + rename).
fn write_atomic(target: &Path, content: &str) -> std::io::Result<()> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".hnl-fmt-tmp-{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

fn err(msg: impl Into<String>) -> ExtResponse {
    ExtResponse::error(2, msg.into())
}

fn finish(out: Out) -> ExtResponse {
    ExtResponse {
        code: if out.had_failure { 1 } else { 0 },
        stdout: out.stdout,
        stderr: out.stderr,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// A unique temp dir per test — the tests run in parallel and share the
    /// process id, so a pid-only name lets one test's `remove_dir_all` pull
    /// another's fixture out from under it.
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hnl-ext-fmt-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Call `run` directly — the same path `handl_ext_dispatch` reaches.
    #[test]
    fn formats_a_file_to_stdout() {
        let dir = tmpdir("stdout");
        let file = dir.join("dirty.hnl");
        std::fs::write(&file, "x   =   1;").unwrap();

        let res = run(&[file.to_string_lossy().into_owned()], &dir);
        assert_eq!(res.code, 0, "stderr: {}", res.stderr);
        assert_eq!(res.stdout, "x = 1;\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_reports_not_canonical_with_code_1() {
        let dir = tmpdir("check");
        let file = dir.join("dirty.hnl");
        std::fs::write(&file, "x   =   1;").unwrap();

        let res = run(
            &["--check".into(), file.to_string_lossy().into_owned()],
            &dir,
        );
        assert_eq!(res.code, 1, "stdout: {} stderr: {}", res.stdout, res.stderr);
        assert!(res.stdout.contains("not canonical"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_rewrites_the_file() {
        let dir = tmpdir("write");
        let file = dir.join("dirty.hnl");
        std::fs::write(&file, "x   =   1;").unwrap();

        let res = run(
            &["--write".into(), file.to_string_lossy().into_owned()],
            &dir,
        );
        assert_eq!(res.code, 0, "stderr: {}", res.stderr);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x = 1;\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_and_write_conflict_is_usage_error() {
        let res = run(&["--check".into(), "--write".into()], Path::new("."));
        assert_eq!(res.code, 2);
        assert!(res.stderr.contains("mutually exclusive"));
    }

    /// The `export!`-generated manifest symbol, called directly.
    #[test]
    fn manifest_declares_fmt() {
        let raw = handl_ext_manifest();
        assert!(!raw.is_null());
        let json = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { handl_ext_free(raw) };
        let manifest: ExtensionManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest.name, "fmt");
        assert_eq!(manifest.commands[0].name, "fmt");
    }
}
