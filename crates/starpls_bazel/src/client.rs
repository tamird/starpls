use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::str;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use serde::Deserialize;
use serde_json::Deserializer;

const DEFAULT_WORKSPACE_NAMES: &[&str] = &["__main__", "_main"];

#[derive(Default)]
pub struct BazelInfo {
    pub output_base: PathBuf,
    pub release: String,
    pub starlark_semantics: String,
    pub workspace: PathBuf,
    pub workspace_name: Option<String>,
}

/// The module selected by Bazel, with a version for registry releases.
#[derive(Debug, PartialEq, Eq)]
pub struct SelectedModule {
    pub name: String,
    pub version: Option<String>,
}

pub trait BazelClient: Send + Sync + 'static {
    fn build_language(&self) -> anyhow::Result<Vec<u8>>;
    fn info(&self) -> anyhow::Result<BazelInfo>;
    fn null_query_external_repo_targets(&self, repo: &str) -> anyhow::Result<()>;
    fn query_all_workspace_targets(&self) -> anyhow::Result<Vec<String>>;
    fn fetch_repo(&self, repo: &str) -> anyhow::Result<()>;
    fn dump_repo_mapping(&self, repo: &str) -> anyhow::Result<HashMap<String, String>>;
    /// Returns None when no module name is available (an extension or unnamed root).
    fn selected_module(&self, canonical_repo: &str) -> anyhow::Result<Option<SelectedModule>>;
}

pub struct BazelCLI {
    executable: PathBuf,
    working_directory: Option<PathBuf>,
}

impl BazelCLI {
    pub fn new(executable: impl AsRef<Path>) -> Self {
        Self {
            executable: executable.as_ref().to_path_buf(),
            ..Default::default()
        }
    }

    pub fn with_working_directory(mut self, directory: PathBuf) -> anyhow::Result<Self> {
        if self.executable.is_relative() && self.executable.components().count() > 1 {
            self.executable = std::env::current_dir()?.join(&self.executable);
        }
        self.working_directory = Some(directory);
        Ok(self)
    }

    fn run_command<I, S>(&self, args: I) -> anyhow::Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = Command::new(&self.executable);
        command.args(args);
        if let Some(directory) = &self.working_directory {
            command.current_dir(directory);
        }
        let output = command.output()?;
        if !output.status.success() {
            bail!(
                "failed to run Bazel command with exit status {}, stderr={:?}",
                output.status,
                str::from_utf8(&output.stderr)?,
            );
        }
        Ok(output.stdout)
    }
}

impl BazelClient for BazelCLI {
    fn build_language(&self) -> anyhow::Result<Vec<u8>> {
        self.run_command(["info", "build-language"])
    }

    fn info(&self) -> anyhow::Result<BazelInfo> {
        let output = self.run_command([
            "info",
            "execution_root",
            "output_base",
            "release",
            "starlark-semantics",
            "workspace",
        ])?;

        let output = str::from_utf8(&output)?;
        let mut output_base = None;
        let mut release = None;
        let mut starlark_semantics = None;
        let mut workspace = None;
        let mut workspace_name = None;
        for line in output.lines() {
            let (key, value) = match line.split_once(": ") {
                Some(pair) => pair,
                None => continue,
            };
            match key {
                "execution_root" => {
                    // Taken from https://github.com/cameron-martin/bazel-lsp/blob/92644f21aca7cfbba332c67ac1aa9cf43765e021/src/workspace.rs#L24.
                    workspace_name = PathBuf::from(value).file_name().and_then(|file_name| {
                        match file_name.to_string_lossy().to_string() {
                            name if DEFAULT_WORKSPACE_NAMES.contains(&name.as_str()) => None,
                            name => Some(name),
                        }
                    });
                }
                "output_base" => output_base = Some(value),
                "release" => release = Some(value),
                "starlark-semantics" => starlark_semantics = Some(value),
                "workspace" => workspace = Some(value),
                _ => {}
            }
        }

        Ok(BazelInfo {
            output_base: output_base
                .ok_or_else(|| anyhow!("failed to determine output_base from `bazel info`"))?
                .into(),
            release: release
                .ok_or_else(|| anyhow!("failed to determine release from `bazel info`"))?
                .into(),
            starlark_semantics: starlark_semantics
                .ok_or_else(|| anyhow!("failed to determine starlark-semantics from `bazel info`"))?
                .into(),
            workspace: workspace
                .ok_or_else(|| anyhow!("failed to determine workspace from `bazel info`"))?
                .into(),
            workspace_name,
        })
    }

    fn null_query_external_repo_targets(&self, repo: &str) -> anyhow::Result<()> {
        self.run_command(["query", "--keep_going", &format!("@@{}//...", repo)])?;
        Ok(())
    }

    fn query_all_workspace_targets(&self) -> anyhow::Result<Vec<String>> {
        let output = self.run_command(["query", "kind('.* rule', ...)"])?;
        let targets = str::from_utf8(&output)?
            .lines()
            .map(|line| line.to_string())
            .collect();
        Ok(targets)
    }

    fn fetch_repo(&self, repo: &str) -> anyhow::Result<()> {
        self.run_command(["fetch", "--repo", &format!("@@{}", repo)])?;
        Ok(())
    }

    fn dump_repo_mapping(&self, repo: &str) -> anyhow::Result<HashMap<String, String>> {
        let output = self.run_command(["mod", "--enable_bzlmod", "dump_repo_mapping", repo])?;
        let json = String::from_utf8(output)?;
        let mut mappings = Deserializer::from_str(&json).into_iter::<HashMap<String, String>>();
        Ok(mappings
            .next()
            .ok_or_else(|| anyhow!("missing repo mapping for repository: {:?}", repo))??)
    }

    fn selected_module(&self, canonical_repo: &str) -> anyhow::Result<Option<SelectedModule>> {
        let graph = self.run_command([
            "mod",
            "graph",
            &format!("--from=@@{canonical_repo}"),
            "--depth=1",
            "--output=json",
        ]);
        let graph_error = match graph {
            Ok(output) => return selected_module_from_graph(&output, canonical_repo),
            Err(error) => error,
        };
        if !canonical_repo.is_empty() {
            // Extension repositories have no module graph node. Bazel 9's
            // repository metadata can establish their extension provenance.
            let output = self
                .run_command([
                    "mod",
                    "show_repo",
                    &format!("@@{canonical_repo}"),
                    "--output=streamed_jsonproto",
                ])
                .with_context(|| {
                    format!(
                "cannot identify @@{canonical_repo}; module graph query failed: {graph_error:#}"
            )
                })?;
            if is_extension_repository(&output, canonical_repo)? {
                return Ok(None);
            }
        }
        Err(graph_error).with_context(|| format!("cannot identify module for @@{canonical_repo}"))
    }
}

fn selected_module_from_graph(
    output: &[u8],
    canonical_repo: &str,
) -> anyhow::Result<Option<SelectedModule>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Graph {
        key: String,
        name: Option<String>,
        dependencies: Vec<Module>,
        indirect_dependencies: Vec<Module>,
    }
    #[derive(Deserialize)]
    struct Module {
        key: String,
    }
    let Graph {
        key,
        name,
        mut dependencies,
        indirect_dependencies,
    } = serde_json::from_slice(output).context("invalid Bazel module graph JSON")?;
    if key != "<root>" {
        bail!("expected Bazel module graph root, got {key:?}");
    }
    if canonical_repo.is_empty() {
        return Ok(name
            .filter(|name| !name.is_empty())
            .map(|name| SelectedModule {
                name,
                version: None,
            }));
    }
    dependencies.extend(indirect_dependencies);
    let [Module { key }] = dependencies.as_slice() else {
        bail!("expected one selected module for @@{canonical_repo}");
    };
    let Some((name, version)) = key.split_once('@') else {
        bail!("invalid Bazel module key {key:?}");
    };
    if name.is_empty() || version.is_empty() || version.contains('@') {
        bail!("invalid Bazel module key {key:?}");
    }
    // Module keys mark non-registry overrides with `_`. The separate JSON
    // version field may still contain the override's declared MODULE version.
    Ok(Some(SelectedModule {
        name: name.to_owned(),
        version: (version != "_").then(|| version.to_owned()),
    }))
}

fn is_extension_repository(output: &[u8], canonical_repo: &str) -> anyhow::Result<bool> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repository {
        canonical_name: String,
        original_name: Option<String>,
    }
    let repositories = Deserializer::from_slice(output)
        .into_iter::<Repository>()
        .collect::<Result<Vec<_>, _>>()
        .context("invalid Bazel repository metadata JSON")?;
    let [Repository {
        canonical_name,
        original_name,
    }] = repositories.as_slice()
    else {
        if repositories.is_empty() {
            return Ok(false);
        }
        bail!("expected one repository description for @@{canonical_repo}");
    };
    if canonical_name != canonical_repo {
        bail!("expected repository @@{canonical_repo}, got @@{canonical_name}");
    }
    Ok(original_name.as_ref().is_some_and(|name| !name.is_empty()))
}

impl Default for BazelCLI {
    fn default() -> Self {
        Self {
            executable: "bazel".into(),
            working_directory: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_extension_repository;
    use super::selected_module_from_graph;
    use super::SelectedModule;

    #[test]
    fn module_keys_identify_selected_releases_and_overrides() {
        for (edge, key, expected) in [
            ("dependencies", "rules_foo@1.4.1", Some("1.4.1")),
            ("indirectDependencies", "rules_foo@1.4.1", Some("1.4.1")),
            ("dependencies", "rules_foo@_", None),
        ] {
            let mut graph = serde_json::json!({
                "key": "<root>", "dependencies": [], "indirectDependencies": []
            });
            // Older Bazel versions omit name/version when they agree with key;
            // overrides can report a declared version different from the key.
            graph[edge] = serde_json::json!([{"key": key, "version": "1.4.0"}]);
            let output = serde_json::to_vec(&graph).unwrap();
            assert_eq!(
                selected_module_from_graph(&output, "canonical-name").unwrap(),
                Some(SelectedModule {
                    name: "rules_foo".to_owned(),
                    version: expected.map(str::to_owned),
                }),
            );
        }
        let root = br#"{"key":"<root>","name":"main","version":"1.0.0","dependencies":[],"indirectDependencies":[]}"#;
        assert_eq!(
            selected_module_from_graph(root, "").unwrap(),
            Some(SelectedModule {
                name: "main".to_owned(),
                version: None
            }),
        );
        assert!(selected_module_from_graph(root, "missing").is_err());
        assert!(selected_module_from_graph(b"{}", "missing").is_err());
        let ambiguous = br#"{"key":"<root>","dependencies":[{"key":"one@1"},{"key":"two@2"}],"indirectDependencies":[]}"#;
        assert!(selected_module_from_graph(ambiguous, "repo").is_err());
    }

    #[test]
    fn nonmodules_require_positive_repository_provenance() {
        assert!(is_extension_repository(
            br#"{"canonicalName":"repo","originalName":"generated"}"#,
            "repo"
        )
        .unwrap());
        for output in [
            b"".as_slice(),
            br#"{"canonicalName":"repo"}"#,
            br#"{"canonicalName":"repo","originalName":""}"#,
        ] {
            assert!(!is_extension_repository(output, "repo").unwrap());
        }
        for output in [
            b"{}".as_slice(),
            br#"{"canonicalName":"other","originalName":"generated"}"#,
            b"{\"canonicalName\":\"repo\"}\n{\"canonicalName\":\"repo\"}",
        ] {
            assert!(is_extension_repository(output, "repo").is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn failed_queries_do_not_imply_an_unversioned_repository() {
        use std::os::unix::fs::PermissionsExt;

        use super::BazelCLI;
        use super::BazelClient;

        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("selected-module-client");
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("bazel");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
case "$2" in
  graph) response=graph ;;
  show_repo) response=repository ;;
  *) exit 99 ;;
esac
cat "${0%/*}/$response.json"
read status < "${0%/*}/$response.status"
exit "$status"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let response = |name: &str, status: i32, body: &str| {
            std::fs::write(root.join(format!("{name}.json")), body).unwrap();
            std::fs::write(root.join(format!("{name}.status")), format!("{status}\n")).unwrap();
        };
        let client = BazelCLI::new(&executable);
        response("graph", 1, "");
        response(
            "repository",
            0,
            r#"{"canonicalName":"repo","originalName":"extension"}"#,
        );
        assert_eq!(client.selected_module("repo").unwrap(), None);
        response("repository", 0, r#"{"canonicalName":"repo"}"#);
        assert!(client.selected_module("repo").is_err());
        response("repository", 1, "");
        assert!(client.selected_module("repo").is_err());
        response("graph", 0, "malformed graph JSON");
        response(
            "repository",
            0,
            r#"{"canonicalName":"repo","originalName":"extension"}"#,
        );
        assert!(client.selected_module("repo").is_err());
        response(
            "graph",
            0,
            r#"{"key":"<root>","dependencies":[{"key":"rules@1.2.0"}],"indirectDependencies":[]}"#,
        );
        response("repository", 1, "");
        assert_eq!(
            client.selected_module("repo").unwrap(),
            Some(SelectedModule {
                name: "rules".to_owned(),
                version: Some("1.2.0".to_owned()),
            })
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
