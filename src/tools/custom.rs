//! Agent self-authored tools (§1): schemas from ~/.tursi/tools/registry.json,
//! scripts in ~/.tursi/tools/bin/, appended to the roster at session start.

use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;

use crate::api::ToolSchema;
use crate::tools::Toolbox;

pub struct Registry {
    pub entries: Vec<CustomTool>,
}

pub struct CustomTool {
    pub schema: ToolSchema,
    pub bin: PathBuf,
}

impl Registry {
    /// Missing registry = empty roster, never an error.
    pub fn load() -> Result<Registry> {
        let path = crate::config::Config::home_dir()?.join("tools/registry.json");
        if !path.exists() {
            return Ok(Registry { entries: Vec::new() });
        }
        #[derive(serde::Deserialize)]
        struct Entry {
            name: String,
            description: String,
            parameters: Value,
            bin: PathBuf,
        }
        let raw: Vec<Entry> = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let entries = raw
            .into_iter()
            .map(|e| CustomTool {
                schema: ToolSchema {
                    name: e.name,
                    description: e.description,
                    parameters: e.parameters,
                },
                bin: e.bin,
            })
            .collect();
        Ok(Registry { entries })
    }
}

/// Run the script in the sandbox; the tool-call arguments land as a JSON
/// file whose path is `$1` (step stdin is /dev/null by design, §6.2). Output
/// goes through the usual truncation/log pipeline.
pub async fn run(tb: &mut Toolbox, name: &str, args: &Value) -> Result<String> {
    use crate::sandbox::{OnError, Step, Streams};
    let Some(tool) = tb.custom.entries.iter().find(|t| t.schema.name == name) else {
        anyhow::bail!("unknown tool: {name}");
    };
    let scratch = tb.project.join(".tursi/scratch");
    std::fs::create_dir_all(&scratch)?;
    let args_file = scratch.join(format!("args-{}.json", uuid::Uuid::now_v7().simple()));
    std::fs::write(&args_file, serde_json::to_string(args)?)?;

    let step = Step {
        command: format!("'{}' '{}'", tool.bin.display(), args_file.display()),
        cwd: None,
        env: vec![],
        streams: Streams::Both,
        timeout: std::time::Duration::from_secs(120),
        tail_lines: 40,
    };
    let results = tb.sandbox.run_steps(vec![step], OnError::Stop).await;
    let _ = std::fs::remove_file(&args_file);
    let result = results?.pop().expect("one step in, one result out");

    let full = format!("stdout:\n{}\nstderr:\n{}", result.stdout, result.stderr);
    let id = crate::output::log_full(&tb.project, tb.agent, name, &full)?;
    match result.exit_code {
        Some(0) => Ok(format!(
            "{} [full: log#{}]",
            crate::output::truncate(&result.stdout, 40),
            id.0
        )),
        code => anyhow::bail!(
            "{name} failed (exit {code:?}): {} [full: log#{}]",
            crate::output::truncate(&format!("{}\n{}", result.stderr, result.stdout), 20),
            id.0
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ToolSchema;
    use crate::tools::testutil;

    #[tokio::test]
    async fn custom_tool_receives_args_file_and_returns_output() {
        let dir = testutil::tmp("custom");
        let script = dir.join("echo-args.sh");
        std::fs::write(&script, "#!/bin/sh\ncat \"$1\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (mut tb, _ui, _rx) = testutil::toolbox(&dir);
        tb.custom.entries.push(CustomTool {
            schema: ToolSchema {
                name: "echo_args".into(),
                description: "test".into(),
                parameters: serde_json::json!({}),
            },
            bin: script,
        });
        let out = run(&mut tb, "echo_args", &serde_json::json!({"probe": 7})).await.unwrap();
        assert!(out.contains("\"probe\":7"), "got: {out}");
        assert!(out.contains("[full: log#"));
    }
}
