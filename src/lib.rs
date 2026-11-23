//! `hnl-ext-fmt` — the `fmt` extension for `hnl` (ADR-0149).
//!
//! The formatter logic lives in `handl-plugin-ext-fmt` (platform) and
//! `hnl-tool-fmt`; this crate is the *packaging*: it exports the
//! ADR-0149 ABI (`handl_ext_manifest` / `handl_ext_dispatch` /
//! `handl_ext_free`) and implements the `hnl fmt [--check] [--write]
//! [paths...]` command surface.
//!
//! PL-004 rewiring: file paths run the **desired-state pipeline** —
//! `hnl_tool_fmt::register` seats the `"fmt"` source/emitter/target
//! on a [`CodegenRegistry`], so `hnl fmt` resolves the same
//! `Source → IR → Emitter → Change → Engine` lifecycle `hnl codegen
//! fmt` does. `--write` applies through [`Engine::apply`] with the
//! manifest + write-ahead journal under `<root>/.handl/codegen/`
//! (revertable via `hnl codegen revert`/`Engine::revert`); `--check`
//! is `Engine::plan` reporting drift; the default mode prints the
//! materialized formatted bytes without touching the store.
//!
//! Stdin has no artifact to journal, so it keeps the capability-JSON
//! path (`FmtPlugin` over `DefaultHost`) — `--write` on stdin is a
//! usage error rather than a silent non-write.
//!
//! Apply passes `prior = None` deliberately (the codemod precedent):
//! `hnl fmt` formats files *in place* and does not own an artifact
//! set, so a per-file run must never plan manifest-owned deletes of
//! siblings a previous run formatted.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use handl_ext::{ExtRequest, ExtResponse, ExtensionManifest};
use handl_language::Language;
use handl_platform::{CapabilityRequest, DefaultHost};
use handl_plugin_codegen::{
    ApplyOptions, CodegenRegistry, CodegenRequest, CodegenTargetError, Engine, FsStore, RunMeta,
};
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

    let mut out = Out::default();

    if paths.is_empty() {
        // Stdin has no artifact — the desired-state pipeline is
        // file-based, so stdin keeps the capability path. `--write`
        // cannot journal a non-file; refuse rather than silently
        // printing.
        if write {
            return err("fmt: --write requires a file path (stdin has no artifact to journal)\n");
        }
        let mut host = match tooling_host() {
            Ok(h) => h,
            Err(e) => return err(format!("fmt: tooling host unavailable: {e}\n")),
        };
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_err() {
            return err("fmt: failed to read stdin\n");
        }
        let code = fmt_stdin(&mut host, &src, check, &mut out);
        if code == 2 {
            return ExtResponse {
                code: 2,
                stdout: out.stdout,
                stderr: out.stderr,
            };
        }
        return finish(out);
    }

    // The desired-state registry: `hnl fmt` resolves the same `"fmt"`
    // target `hnl codegen fmt` does (PL-004).
    let mut registry = CodegenRegistry::new();
    hnl_tool_fmt::register(&mut registry);
    let lang = canonical_lang();

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
        if fmt_file(
            &registry,
            lang.as_ref(),
            cwd,
            &disk,
            &src,
            path,
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
    }

    finish(out)
}

/// A host with the `fmt` plugin registered — the stdin path consumes
/// the same `DefaultHost`/`CapabilityRequest` machinery it always did;
/// only file paths moved to the codegen lifecycle (PL-004).
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

/// The `FsStore` root for `disk` under `cwd`: the cwd when the file
/// lives beneath it — so the journal/manifest land at
/// `<cwd>/.handl/codegen/`, the same bookkeeping root `hnl codemod`
/// and `hnl codegen fmt --output <dir>` use — else the file's own
/// directory (the artifact ref must be store-relative; an absolute
/// spec outside the root is a `FmtSource` error).
fn store_root(cwd: &Path, disk: &Path) -> PathBuf {
    let canon_disk = disk.canonicalize().unwrap_or_else(|_| disk.to_path_buf());
    let canon_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if let Ok(rel) = canon_disk.strip_prefix(&canon_cwd)
        && !rel.as_os_str().is_empty()
    {
        return cwd.to_path_buf();
    }
    disk.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf())
}

/// Format one on-disk file through the `"fmt"` codegen target.
///
/// `src` is the already-read file text (dialect gate + read-error
/// parity with the old path); [`FmtSource`] re-reads `spec` itself —
/// the kernel `Source` owns the file→IR lift.
///
/// Returns 2 on usage/read/materialize errors; sets `out.had_failure`
/// for `--check` drift; otherwise 0.
#[allow(clippy::too_many_arguments)]
fn fmt_file(
    registry: &CodegenRegistry,
    lang: &Language,
    cwd: &Path,
    disk: &Path,
    src: &str,
    path: &str,
    check: bool,
    write: bool,
    out: &mut Out,
) -> i32 {
    if detect_dialect(src) != Dialect::Canonical {
        out.stderr
            .push_str(&format!("skipping dialect file: {path}\n"));
        return 0;
    }

    let root = store_root(cwd, disk);
    let request = CodegenRequest {
        spec: disk,
        source_url: None,
        output: &root,
        target: None,
        include: None,
        exclude: None,
        config: None,
        emit_spec: None,
        lang,
        cache_dir: None,
    };
    let changes = match registry.materialize("fmt", &request) {
        Ok(changes) => changes,
        Err(e) => {
            // `CodegenTargetError` has no `Display`; unwrap the message.
            let msg = match &e {
                CodegenTargetError::Usage(m) | CodegenTargetError::Failed(m) => m.as_str(),
            };
            out.stderr.push_str(&format!("fmt: {path}: {msg}\n"));
            return 2;
        }
    };
    let store = FsStore::new(&root);
    let engine = Engine::new(&store);

    if check {
        // `prior = None`: fmt does not own an artifact set (see module
        // docs) — a stale manifest must not plan deletes.
        return match engine.plan(&changes, None) {
            Ok(plan) if plan.is_clean() => 0,
            Ok(_) => {
                out.stdout.push_str(&format!("{path}: not canonical\n"));
                out.had_failure = true;
                0
            }
            Err(e) => {
                out.stderr.push_str(&format!("fmt: {path}: {e}\n"));
                2
            }
        };
    }

    if write {
        // The journaled desired-state apply — manifest + write-ahead
        // journal under `<root>/.handl/codegen/`, drift-checked and
        // revertable, exactly like `hnl codegen fmt --output <root>`.
        let mut meta = RunMeta::new("fmt", env!("CARGO_PKG_VERSION"));
        meta.new_run_id();
        return match engine.apply(&changes, None, &meta, &ApplyOptions::default()) {
            Ok(report) if report.clean() => 0,
            Ok(report) => {
                for (artifact, e) in &report.failures {
                    out.stderr.push_str(&format!("fmt: {artifact}: {e}\n"));
                }
                if let Some(fatal) = &report.fatal {
                    out.stderr.push_str(&format!("fmt: {fatal}\n"));
                }
                1
            }
            Err(e) => {
                out.stderr
                    .push_str(&format!("fmt: {path}: apply failed: {e}\n"));
                1
            }
        };
    }

    // Default: the materialized formatted bytes to stdout — no store
    // mutation, no journal.
    match changes
        .iter()
        .find_map(|c| c.content())
        .map(|b| String::from_utf8_lossy(b).into_owned())
    {
        Some(formatted) => {
            out.stdout.push_str(&formatted);
            0
        }
        None => {
            out.stderr
                .push_str(&format!("fmt: {path}: no formatted output\n"));
            2
        }
    }
}

/// Format stdin through the `fmt` capability — the pre-PL-004 path,
/// kept because the desired-state pipeline needs a file artifact.
/// Returns 2 on parse/dispatch error (and sets `out.had_failure` for
/// check failures); otherwise 0. `--write` is rejected by the caller.
fn fmt_stdin(host: &mut DefaultHost, src: &str, check: bool, out: &mut Out) -> i32 {
    let request = CapabilityRequest {
        capability: "fmt".into(),
        payload: json!({ "source": src, "check": check }),
    };
    let response = match host.run(&request) {
        Ok(r) => r,
        Err(e) => {
            out.stderr.push_str(&format!("fmt: <stdin>: {e}\n"));
            return 2;
        }
    };

    match response.0.get("status").and_then(|v| v.as_str()) {
        Some("canonical") => 0,
        Some("not-canonical") => {
            out.stdout.push_str("<stdin>: not canonical\n");
            out.had_failure = true;
            0
        }
        Some("formatted") => {
            let formatted = response
                .0
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            out.stdout.push_str(formatted);
            0
        }
        Some("error") => {
            let diag: handl_plugin_ext_diag::Diagnostic = response
                .0
                .get("diagnostic")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            out.stderr.push_str(&render_diagnostic(&diag, "<stdin>"));
            out.stderr.push('\n');
            2
        }
        other => {
            out.stderr.push_str(&format!(
                "fmt: <stdin>: unexpected capability status {other:?}\n"
            ));
            2
        }
    }
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
    fn write_rewrites_the_file_journaled() {
        let dir = tmpdir("write");
        let file = dir.join("dirty.hnl");
        std::fs::write(&file, "x   =   1;").unwrap();

        let res = run(
            &["--write".into(), file.to_string_lossy().into_owned()],
            &dir,
        );
        assert_eq!(res.code, 0, "stderr: {}", res.stderr);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x = 1;\n");
        // The desired-state apply journals under `.handl/codegen/`.
        assert!(
            dir.join(".handl/codegen/journal").exists()
                || dir
                    .join(".handl/codegen")
                    .read_dir()
                    .is_ok_and(|mut d| d.next().is_some()),
            "expected .handl/codegen bookkeeping under {}",
            dir.display()
        );
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
