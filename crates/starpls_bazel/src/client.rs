use std::collections::HashMap;
use std::io::BufRead;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::str;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use serde::Deserialize;
use serde_json::Deserializer;

const DEFAULT_WORKSPACE_NAMES: &[&str] = &["__main__", "_main"];

pub type RepoMapping = Arc<HashMap<String, String>>;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum MappingFormat {
    #[default]
    Compact,
    Json,
}

#[derive(Default)]
struct MappingState {
    values: HashMap<Box<[u8]>, Weak<HashMap<String, String>>>,
    format: MappingFormat,
}

struct MappingOutput {
    mappings: Vec<RepoMapping>,
    empty_stdout: bool,
}

impl MappingState {
    fn intern(&mut self, json: &[u8]) -> anyhow::Result<RepoMapping> {
        if let Some(mapping) = self.values.get(json).and_then(Weak::upgrade) {
            return Ok(mapping);
        }
        let mapping = Arc::new(serde_json::from_slice(json)?);
        self.values.insert(json.into(), Arc::downgrade(&mapping));
        Ok(mapping)
    }
}

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
    fn fetch_repos(&self, repos: &[&str]) -> anyhow::Result<()>;
    fn fetch_repo(&self, repo: &str) -> anyhow::Result<()> {
        self.fetch_repos(&[repo])
    }
    /// Returns mappings in the same order as the canonical repository names.
    fn dump_repo_mappings(&self, repos: &[&str]) -> anyhow::Result<Vec<RepoMapping>>;
    fn dump_repo_mapping(&self, repo: &str) -> anyhow::Result<RepoMapping> {
        let mut mappings = self.dump_repo_mappings(&[repo])?;
        if mappings.len() != 1 {
            bail!(
                "expected one repository mapping, received {}",
                mappings.len()
            );
        }
        Ok(mappings.pop().expect("validated mapping count"))
    }
    /// Returns None when no module name is available (an extension or unnamed root).
    fn selected_module(&self, canonical_repo: &str) -> anyhow::Result<Option<SelectedModule>>;
}

pub struct BazelCLI {
    executable: PathBuf,
    working_directory: Option<PathBuf>,
    mappings: Mutex<MappingState>,
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

    fn fetch_repos(&self, repos: &[&str]) -> anyhow::Result<()> {
        if repos.is_empty() {
            bail!("repository fetch requires at least one repository");
        }
        let args = std::iter::once("fetch".to_owned())
            .chain(repos.iter().map(|repo| format!("--repo=@@{repo}")));
        self.run_command(args)?;
        Ok(())
    }

    fn dump_repo_mappings(&self, repos: &[&str]) -> anyhow::Result<Vec<RepoMapping>> {
        if repos.is_empty() {
            bail!("repository mapping query requires at least one repository");
        }
        // Serialize mapping readers before they can contend for Bazel's server
        // lock with an unread stdout pipe and this interner lock held elsewhere.
        let mut state = self.mappings.lock().expect("mapping state was poisoned");
        loop {
            let mut command = Command::new(&self.executable);
            command.args(["mod", "--enable_bzlmod", "dump_repo_mapping"]);
            if state.format == MappingFormat::Compact {
                command.arg("--output=compact_json");
            }
            command.args(repos);
            if let Some(directory) = &self.working_directory {
                command.current_dir(directory);
            }
            let mut child = command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let stdout = child.stdout.take().expect("stdout was piped");
            let mut stderr = child.stderr.take().expect("stderr was piped");
            let (result, status, errors) = std::thread::scope(|scope| -> anyhow::Result<_> {
                let errors = scope.spawn(move || {
                    let mut bytes = Vec::new();
                    stderr.read_to_end(&mut bytes).map(|_| bytes)
                });
                let result =
                    parse_repo_mappings(std::io::BufReader::new(stdout), repos.len(), &mut state);
                if result.is_err() {
                    // A malformed stream can leave the child blocked on stdout.
                    let _ = child.kill();
                }
                let status = child.wait()?;
                let errors = errors.join().expect("stderr reader panicked")?;
                Ok((result, status, errors))
            })?;
            let MappingOutput {
                mappings,
                empty_stdout,
            } = result.with_context(|| {
                format!(
                    "Bazel repository mapping query ({status}): {}",
                    String::from_utf8_lossy(&errors)
                )
            })?;
            if state.format == MappingFormat::Compact
                && empty_stdout
                && status.code() == Some(2)
                && unsupported_compact_mapping_format(&errors)
            {
                state.format = MappingFormat::Json;
                continue;
            }
            if !status.success() {
                bail!(
                    "failed to query repository mappings with {status}: {}",
                    String::from_utf8_lossy(&errors)
                );
            }
            if mappings.len() != repos.len() {
                bail!(
                    "expected {} repository mappings, received {}",
                    repos.len(),
                    mappings.len()
                );
            }
            return Ok(mappings);
        }
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

fn parse_repo_mappings(
    mut output: impl BufRead,
    expected: usize,
    interner: &mut MappingState,
) -> anyhow::Result<MappingOutput> {
    interner
        .values
        .retain(|_, mapping| mapping.strong_count() > 0);
    let mut mappings = Vec::with_capacity(expected);
    let mut line = Vec::new();
    let mut empty_stdout = true;
    loop {
        line.clear();
        if output.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        empty_stdout = false;
        let json = line.trim_ascii();
        if json.is_empty() {
            continue;
        }
        if mappings.len() == expected {
            bail!("unexpected extra repository mapping");
        }
        let mapping = if json.starts_with(b"{") || interner.format == MappingFormat::Json {
            interner.intern(json)?
        } else {
            let index = serde_json::from_slice::<usize>(json)?;
            let referenced = mappings.get(index).with_context(|| {
                format!("repository mapping reference {index} does not identify an earlier record")
            })?;
            Arc::clone(referenced)
        };
        mappings.push(mapping);
    }
    Ok(MappingOutput {
        mappings,
        empty_stdout,
    })
}

fn unsupported_compact_mapping_format(stderr: &[u8]) -> bool {
    let Ok(stderr) = str::from_utf8(stderr) else {
        return false;
    };
    stderr.lines().any(|line| {
        line.starts_with("ERROR: While parsing option --output=compact_json: Not a valid output format: 'compact_json' (should be ")
            && line.ends_with(')')
    })
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
            mappings: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_extension_repository;
    use super::selected_module_from_graph;
    use super::SelectedModule;

    #[cfg(unix)]
    #[test]
    fn repository_fetches_use_repeated_canonical_arguments() {
        use std::os::unix::fs::PermissionsExt;

        use super::BazelClient;
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("fetch-arguments");
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("bazel");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
printf '%s\n' "$@" >> "${0%/*}/arguments"
read status < "${0%/*}/status"
echo 'native fetch failure' >&2
exit "$status"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let client = super::BazelCLI::new(executable);
        assert!(client.fetch_repos(&[]).is_err());
        assert!(!root.join("arguments").exists());
        std::fs::write(root.join("status"), "0\n").unwrap();
        client
            .fetch_repos(&["first+", "rules++ext+second"])
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("arguments")).unwrap(),
            "fetch\n--repo=@@first+\n--repo=@@rules++ext+second\n"
        );
        std::fs::write(root.join("status"), "1\n").unwrap();
        let error = client.fetch_repo("broken+").unwrap_err().to_string();
        assert!(error.contains("native fetch failure"), "{error}");
        assert!(std::fs::read_to_string(root.join("arguments"))
            .unwrap()
            .ends_with("fetch\n--repo=@@broken+\n"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn equal_mapping_outputs_share_storage_across_batches() {
        let mut interner = super::MappingState::default();
        let output = b"{\"dep\":\"same+\"}\n{\"dep\":\"same+\"}\n";
        let first = super::parse_repo_mappings(output.as_slice(), 2, &mut interner)
            .unwrap()
            .mappings;
        let [one, two] = first.as_slice() else {
            panic!("expected two mappings");
        };
        assert!(std::sync::Arc::ptr_eq(one, two));
        let second = super::parse_repo_mappings(output.as_slice(), 2, &mut interner)
            .unwrap()
            .mappings;
        assert!(std::sync::Arc::ptr_eq(one, second.first().unwrap()));
        assert_eq!(interner.values.len(), 1);
        drop(first);
        drop(second);
        let third = super::parse_repo_mappings(b"{}\n".as_slice(), 1, &mut interner)
            .unwrap()
            .mappings;
        assert!(third.first().unwrap().is_empty());
        assert_eq!(
            interner.values.len(),
            1,
            "obsolete JSON keys must be released"
        );
    }

    #[cfg(unix)]
    #[test]
    fn streamed_mapping_queries_drain_stderr_and_check_exit_status() {
        use std::os::unix::fs::PermissionsExt;

        use super::BazelClient;
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("streamed-mappings");
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("bazel");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
test "$#" = 6 && test "$4" = '--output=compact_json' && test "$5" = '' && test "$6" = 'repo+' || exit 99
i=0
while test "$i" -lt 2000; do
  printf 'Bazel progress message while stdout is consumed\n' >&2
  i=$((i+1))
done
printf '{"dep":"main+"}\n{"dep":"repo+"}\n'
read status < "${0%/*}/status"
exit "$status"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let client = super::BazelCLI::new(executable);
        std::fs::write(root.join("status"), "0\n").unwrap();
        let mappings = client.dump_repo_mappings(&["", "repo+"]).unwrap();
        let [main, repository] = mappings.as_slice() else {
            panic!("expected mappings");
        };
        assert_eq!(main["dep"], "main+");
        assert_eq!(repository["dep"], "repo+");
        std::fs::write(root.join("status"), "1\n").unwrap();
        assert!(client.dump_repo_mappings(&["", "repo+"]).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_mapping_batches_preserve_order_and_reject_invalid_records() {
        let output = b"{\"dep\":\"first+\"}\n{\"dep\":\"second+\"}\n";
        let mappings = super::parse_repo_mappings(output.as_slice(), 2, &mut Default::default())
            .unwrap()
            .mappings;
        let [first, second] = mappings.as_slice() else {
            panic!("expected two mappings: {mappings:?}");
        };
        assert_eq!(first.get("dep").unwrap(), "first+");
        assert_eq!(second.get("dep").unwrap(), "second+");
        for (output, count) in [
            (output.as_slice(), 1),
            (b"{}\nmalformed".as_slice(), 2),
            (b"{}\n{\"dep\":42}".as_slice(), 2),
        ] {
            assert!(super::parse_repo_mappings(output, count, &mut Default::default()).is_err());
        }
    }

    #[test]
    fn compact_mappings_reference_only_earlier_records() {
        let output = b"{\"dep\":\"first+\"}\n0\n{\"dep\":\"second+\"}\n1\n2\n";
        let parsed =
            super::parse_repo_mappings(output.as_slice(), 5, &mut Default::default()).unwrap();
        let [first, second, third, fourth, fifth] = parsed.mappings.as_slice() else {
            panic!("expected five records");
        };
        assert!(std::sync::Arc::ptr_eq(first, second));
        assert!(std::sync::Arc::ptr_eq(second, fourth));
        assert!(std::sync::Arc::ptr_eq(third, fifth));
        assert_eq!(first["dep"], "first+");
        assert_eq!(third["dep"], "second+");
        for invalid in [
            "0\n",
            "{}\n1\n",
            "{}\n-1\n",
            "{}\n0.0\n",
            "{}\ntrue\n",
            "{}\n[]\n",
            "{}\n18446744073709551616\n",
        ] {
            assert!(
                super::parse_repo_mappings(invalid.as_bytes(), 2, &mut Default::default()).is_err(),
                "{invalid}"
            );
        }
        let mut legacy = super::MappingState {
            format: super::MappingFormat::Json,
            ..Default::default()
        };
        assert!(super::parse_repo_mappings(b"{}\n0\n".as_slice(), 2, &mut legacy).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn compact_mapping_negotiation_requires_the_exact_option_failure() {
        use std::os::unix::fs::PermissionsExt;

        use super::BazelClient;

        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("mapping-format");
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("bazel");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
mode=json
if test "$4" = '--output=compact_json'; then mode=compact; fi
printf '%s\n' "$mode" >> "${0%/*}/calls"
cat "${0%/*}/$mode.stdout"
cat "${0%/*}/$mode.stderr" >&2
read status < "${0%/*}/$mode.status"
exit "$status"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let unsupported = "ERROR: While parsing option --output=compact_json: Not a valid output format: 'compact_json' (should be text, json, graph, streamed_proto or streamed_jsonproto)\n";
        let compact = "{\"dep\":\"repo+\"}\n0\n";
        std::fs::write(
            root.join("json.stdout"),
            "{\"dep\":\"repo+\"}\n{\"dep\":\"repo+\"}\n",
        )
        .unwrap();
        std::fs::write(root.join("json.stderr"), "").unwrap();
        std::fs::write(root.join("json.status"), "0\n").unwrap();
        for (output, errors, status, succeeds, calls) in [
            (compact, "", "0\n", true, "compact\ncompact\n"),
            ("", unsupported, "2\n", true, "compact\njson\njson\n"),
            ("", unsupported, "1\n", false, "compact\n"),
            ("\n", unsupported, "2\n", false, "compact\n"),
            ("{}\n", unsupported, "2\n", false, "compact\n"),
            (
                "",
                "ERROR: repository evaluation failed\n",
                "2\n",
                false,
                "compact\n",
            ),
            ("", "", "0\n", false, "compact\n"),
            ("{}\n", "", "0\n", false, "compact\n"),
            ("{}\n0\n0\n", "", "0\n", false, "compact\n"),
            ("{}\ninvalid\n", unsupported, "2\n", false, "compact\n"),
            (compact, "evaluation failed\n", "2\n", false, "compact\n"),
        ] {
            std::fs::write(root.join("calls"), "").unwrap();
            std::fs::write(root.join("compact.stdout"), output).unwrap();
            std::fs::write(root.join("compact.stderr"), errors).unwrap();
            std::fs::write(root.join("compact.status"), status).unwrap();
            let client = super::BazelCLI::new(&executable);
            let result = client.dump_repo_mappings(&["", "repo+"]);
            assert_eq!(
                result.is_ok(),
                succeeds,
                "stdout={output:?}, stderr={errors:?}, status={status:?}: {result:?}"
            );
            if succeeds {
                let maps = result.unwrap();
                let [first, second] = maps.as_slice() else {
                    panic!("expected two mappings");
                };
                assert!(std::sync::Arc::ptr_eq(first, second));
                client.dump_repo_mappings(&["", "repo+"]).unwrap();
            }
            assert_eq!(std::fs::read_to_string(root.join("calls")).unwrap(), calls);
            if !succeeds {
                std::fs::write(root.join("compact.stdout"), compact).unwrap();
                std::fs::write(root.join("compact.stderr"), "").unwrap();
                std::fs::write(root.join("compact.status"), "0\n").unwrap();
                client.dump_repo_mappings(&["", "repo+"]).unwrap();
                assert_eq!(
                    std::fs::read_to_string(root.join("calls")).unwrap(),
                    "compact\ncompact\n"
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

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
