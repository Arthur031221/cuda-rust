/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

use std::{
    ffi::OsStr,
    io::{self, Write},
    path::Path,
    process::{Command, Output},
};

use super::Context;

const DEMO_INPUT: &str = "crates/lean-exporter/fixtures/arithmetic.mir";
const DEMO_FUNCTION: &str = "global_index";
const DEMO_NOTICE: &str = "Demo: hand-authored scalar MIR, not extracted from a Rust kernel.\n\n";

/// Display input MIR and the generated scalar Lean model without building the
/// device compiler, loading CUDA, or running Lean. The exporter remains a
/// separate workspace tool instead of a dependency of the installed CLI.
pub fn lean_export(ctx: &Context, input: Option<&Path>, function: Option<&str>) {
    let result = std::env::current_dir()
        .map_err(|error| format!("cannot determine current directory: {error}"))
        .and_then(|invocation_dir| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut command = exporter_command(ctx, &invocation_dir, input, function, &cargo)?;
            let output = command
                .output()
                .map_err(|error| format!("could not start the scalar MIR exporter: {error}"))?;
            let output = inspection_output(output, input.is_none())?;
            io::stdout()
                .lock()
                .write_all(output.as_bytes())
                .map_err(|error| format!("cannot write inspection output: {error}"))
        });
    if let Err(error) = result {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

fn exporter_command(
    ctx: &Context,
    invocation_dir: &Path,
    input: Option<&Path>,
    function: Option<&str>,
    cargo: &OsStr,
) -> Result<Command, String> {
    if !ctx.is_workspace
        || !ctx
            .workspace_root
            .join("crates/lean-exporter/Cargo.toml")
            .is_file()
    {
        return Err("`cargo oxide lean-export` must be run inside a cuda-oxide source checkout containing crates/lean-exporter".into());
    }
    let (input, function) = match (input, function) {
        (None, None) => (ctx.workspace_root.join(DEMO_INPUT), DEMO_FUNCTION),
        (Some(path), Some(function)) => {
            let absolute = if path.is_absolute() {
                path.to_owned()
            } else {
                invocation_dir.join(path)
            };
            (absolute, function)
        }
        _ => return Err("custom input and --function must be supplied together".into()),
    };
    let mut command = Command::new(cargo);
    command
        .current_dir(&ctx.workspace_root)
        .args(["run", "--quiet", "--manifest-path"])
        .arg(ctx.workspace_root.join("Cargo.toml"))
        .args(["-p", "lean-exporter", "--", "--inspect"])
        .arg(input)
        .arg(function);
    Ok(command)
}

fn inspection_output(output: Output, is_demo: bool) -> Result<String, String> {
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!("scalar MIR export failed ({})", output.status)
        } else {
            format!("scalar MIR export failed:\n{detail}")
        });
    }
    let output = String::from_utf8(output.stdout)
        .map_err(|error| format!("exporter returned invalid UTF-8: {error}"))?;
    Ok(if is_demo {
        format!("{DEMO_NOTICE}{output}")
    } else {
        output
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    struct Workspace(PathBuf);

    impl Workspace {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "cargo oxide lean export {} {unique}",
                std::process::id()
            ));
            fs::create_dir_all(root.join("crates/lean-exporter")).unwrap();
            fs::write(root.join("crates/lean-exporter/Cargo.toml"), "").unwrap();
            Self(root)
        }

        fn context(&self) -> Context {
            Context {
                workspace_root: self.0.clone(),
                codegen_crate: self.0.join("missing-backend"),
                examples_dir: self.0.join("missing-examples"),
                backend_so: self.0.join("missing-backend.so"),
                is_workspace: true,
                config: Default::default(),
            }
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lean_export_command_selects_demo_without_backend_or_prover() {
        let workspace = Workspace::new();
        let ctx = workspace.context();
        let command =
            exporter_command(&ctx, &workspace.0, None, None, OsStr::new("cargo")).unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(command.get_program(), "cargo");
        assert_eq!(command.get_current_dir(), Some(workspace.0.as_path()));
        assert_eq!(args[0..3], ["run", "--quiet", "--manifest-path"]);
        assert_eq!(args[3], workspace.0.join("Cargo.toml"));
        assert_eq!(args[4..8], ["-p", "lean-exporter", "--", "--inspect"]);
        assert_eq!(args[8], workspace.0.join(DEMO_INPUT));
        assert_eq!(args[9], DEMO_FUNCTION);
        assert_eq!(
            command.get_envs().count(),
            0,
            "no backend configuration is applied"
        );
    }

    #[test]
    fn lean_export_resolves_relative_paths_from_invocation_directory() {
        let workspace = Workspace::new();
        let caller = workspace.0.join("nested caller");
        let input = Path::new("input with spaces.mir");
        let command = exporter_command(
            &workspace.context(),
            &caller,
            Some(input),
            Some("chosen"),
            OsStr::new("cargo"),
        )
        .unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args[8], caller.join(input));
        assert_eq!(args[9], "chosen");

        let absolute = workspace.0.join("absolute file.mir");
        let command = exporter_command(
            &workspace.context(),
            &caller,
            Some(&absolute),
            Some("chosen"),
            OsStr::new("cargo"),
        )
        .unwrap();
        assert_eq!(command.get_args().nth(8), Some(absolute.as_os_str()));
    }

    #[test]
    fn lean_export_rejects_non_checkouts_and_partial_options() {
        let workspace = Workspace::new();
        let mut ctx = workspace.context();
        for (input, function) in [(Some(Path::new("x.mir")), None), (None, Some("x"))] {
            assert!(
                exporter_command(&ctx, &workspace.0, input, function, OsStr::new("cargo")).is_err()
            );
        }
        ctx.is_workspace = false;
        assert!(
            exporter_command(&ctx, &workspace.0, None, None, OsStr::new("cargo"))
                .unwrap_err()
                .contains("source checkout")
        );
    }

    #[cfg(unix)]
    #[test]
    fn lean_export_discards_partial_stdout_on_subprocess_failure() {
        let output = Command::new("sh")
            .args([
                "-c",
                "printf 'partial Lean'; printf 'unsupported operation' >&2; exit 7",
            ])
            .output()
            .unwrap();
        let error = inspection_output(output, true).unwrap_err();
        assert!(error.contains("unsupported operation"));
        assert!(!error.contains("partial Lean"));
        assert!(!error.contains("Demo:"));
    }

    #[cfg(unix)]
    #[test]
    fn lean_export_adds_demo_provenance_only_after_success() {
        let output = Command::new("sh")
            .args([
                "-c",
                "printf '=== MIR input ===\\nsource\\n=== Generated Lean ===\\nmodel\\n'",
            ])
            .output()
            .unwrap();
        let text = inspection_output(output, true).unwrap();
        assert!(text.starts_with(DEMO_NOTICE));
        assert!(text.contains("=== MIR input ===\nsource"));
        assert!(text.contains("=== Generated Lean ===\nmodel"));
    }
}
