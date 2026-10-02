use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use annotate_snippets::Level;
use annotate_snippets::Renderer;
use anyhow::anyhow;
use anyhow::Context;
use clap::Args;
use ruff_db::diagnostic::Diagnostic;
use ruff_db::diagnostic::DisplayDiagnosticConfig;
use serde::Serialize;
use starpls_bazel::client::BazelCLI;
use starpls_bazel::client::BazelInfo;
use starpls_common::Dialect;
use starpls_common::File;
use starpls_common::FileInfo;
use starpls_common::Severity;
use starpls_ide::Analysis;
use starpls_ide::AnalysisSnapshot;
use starpls_ide::LoadDependency;
use starpls_ide::LoadResolution;
use walkdir::WalkDir;

use crate::bazel::BazelContext;
use crate::commands::InferenceOptions;
use crate::document::is_ignored_name;
use crate::document::DefaultFileLoader;
use crate::document::{self};
use crate::event_loop::FetchExternalRepoRequest;
use crate::event_loop::Task;
use crate::server::load_bazel_builtins;

#[derive(Args, Default)]
pub(crate) struct CheckCommand {
    /// Paths to typecheck.
    pub(crate) paths: Vec<String>,

    /// Read newline-delimited paths from a file, or '-' for standard input.
    #[clap(long, value_name = "PATH")]
    files_from: Option<PathBuf>,

    /// Read a Bazel source from PHYSICAL and check it at LOGICAL; repeat for more files.
    #[clap(long, value_name = "LOGICAL=PHYSICAL")]
    source_overlay: Vec<SourceOverlay>,

    /// Select Bazel sources and .bzli interfaces.
    #[clap(long)]
    bazel_only: bool,

    /// Audit every transitive load and report load cycles.
    #[clap(long)]
    audit_loads: bool,

    /// Report dependency discovery and checked-file progress on stderr.
    #[clap(long)]
    progress: bool,

    /// Write a JSON coverage and diagnostics summary.
    #[clap(long, value_name = "PATH")]
    report: Option<PathBuf>,

    /// Path to the Bazel output base.
    #[clap(long = "output_base")]
    pub(crate) output_base: Option<String>,

    /// Exclude a basename anywhere, or a workspace-relative file/directory path.
    #[clap(long = "ignore_pattern")]
    pub(crate) ignore_patterns: Vec<String>,

    #[clap(long = "ext")]
    pub(crate) extensions: Vec<String>,

    #[command(flatten)]
    pub(crate) inference_options: InferenceOptions,

    #[command(flatten)]
    pub(crate) type_interfaces: super::type_interface::TypeInterfaceOptions,
}

#[derive(Clone, Debug)]
struct SourceOverlay {
    logical: PathBuf,
    physical: PathBuf,
}

impl FromStr for SourceOverlay {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((logical, physical)) = value.split_once('=') else {
            return Err("expected LOGICAL=PHYSICAL".to_owned());
        };
        if logical.is_empty() || physical.is_empty() {
            return Err("LOGICAL and PHYSICAL must both be nonempty paths".to_owned());
        }
        Ok(Self {
            logical: logical.into(),
            physical: physical.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CheckCommand;
    use super::Checker;
    use crate::document::source_tests::TestBazelClient;
    use crate::document::DefaultFileLoader;

    fn fetch_checker(
        name: &str,
        bzlmod_enabled: bool,
    ) -> (Checker, Arc<TestBazelClient>, std::path::PathBuf) {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap()).join(name);
        let external = root.join("external");
        std::fs::create_dir_all(&external).unwrap();
        let source = if bzlmod_enabled {
            "load('@rules//:defs.bzl', 'value')\nprint(value)\n"
        } else {
            "load('@rules+//:defs.bzl', 'value')\nprint(value)\n"
        };
        let paths = ["BUILD", "second.bzl"].map(|name| {
            let path = root.join(name);
            std::fs::write(&path, source).unwrap();
            path.to_str().unwrap().to_owned()
        });
        let client = Arc::new(TestBazelClient::default());
        let (sender, receiver) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            client.clone(),
            root.clone(),
            None,
            external.clone(),
            sender,
            bzlmod_enabled,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: root,
            ..Default::default()
        };
        let options = CheckCommand::default();
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        let checker = Checker::new(
            analysis,
            info,
            paths.to_vec(),
            &[],
            loader,
            receiver,
            &options,
        )
        .unwrap();
        (checker, client, external)
    }

    fn local_checker(name: &str, sources: &[(&str, &str)], audit_loads: bool) -> Checker {
        local_checker_with_options(
            name,
            sources,
            CheckCommand {
                paths: vec!["BUILD".to_owned()],
                audit_loads,
                ..Default::default()
            },
        )
    }

    fn local_checker_with_options(
        name: &str,
        sources: &[(&str, &str)],
        mut options: CheckCommand,
    ) -> Checker {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap()).join(name);
        std::fs::create_dir_all(&root).unwrap();
        for (path, contents) in sources {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let (sender, receiver) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            root.clone(),
            None,
            None,
            sender,
            false,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: root.clone(),
            ..Default::default()
        };
        if let Some(path) = &mut options.files_from {
            *path = root.join(&*path);
        }
        let paths = options
            .input_paths()
            .unwrap()
            .iter()
            .map(|path| root.join(path).to_str().unwrap().to_owned())
            .collect();
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        Checker::new(analysis, info, paths, &[], loader, receiver, &options).unwrap()
    }

    #[test]
    fn removed_stub_validation_flag_is_rejected() {
        let error = <CheckCommand as clap::Args>::augment_args(clap::Command::new("check"))
            .try_get_matches_from(["check", "--validate-stubs"])
            .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn unused_suppressions_fail_the_check() {
        for (kind, suppression) in [
            ("ty", "# ty: ignore[invalid-assignment]"),
            ("type", "# type: ignore"),
            ("type-rule", "# type: ignore[ty:invalid-assignment]"),
        ] {
            for (value, expected_errors) in [("1", 0), ("'valid'", 1)] {
                let mut checker = local_checker_with_options(
                    &format!("checker-suppression-{kind}-{expected_errors}"),
                    &[(
                        "source.bzl",
                        &format!("value: str = {value} {suppression}\n"),
                    )],
                    CheckCommand {
                        paths: vec!["source.bzl".to_owned()],
                        ..Default::default()
                    },
                );
                let report_path = checker.bazel_info.workspace.join("coverage.json");
                let result = checker.report_diagnostics(Some(&report_path));
                assert_eq!(result.is_err(), expected_errors != 0, "{result:?}");
                let report: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
                assert_eq!(report["diagnostics"]["errors"], expected_errors);
                assert_eq!(report["diagnostics"]["warnings"], 0);
            }
        }
    }

    #[test]
    fn invalid_suppressions_fail_the_check() {
        for (kind, suppression) in [
            ("malformed", "# ty: ignore["),
            ("unknown", "# ty: ignore[nonexistent-rule]"),
            ("type-malformed", "# type: ignore[ty:"),
            ("type-unknown", "# type: ignore[ty:nonexistent-rule]"),
        ] {
            let mut checker = local_checker_with_options(
                &format!("checker-suppression-{kind}"),
                &[("source.bzl", &format!("value = 42 {suppression}\n"))],
                CheckCommand {
                    paths: vec!["source.bzl".to_owned()],
                    ..Default::default()
                },
            );
            let report_path = checker.bazel_info.workspace.join("coverage.json");
            assert!(checker.report_diagnostics(Some(&report_path)).is_err());
            let report: serde_json::Value =
                serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
            assert_eq!(report["diagnostics"]["errors"], 1);
        }
    }

    fn overlay_checker(
        name: &str,
        overlays: &[(&str, &str, &str)],
    ) -> (Checker, Arc<TestBazelClient>) {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap()).join(name);
        let external = root.join("external");
        std::fs::create_dir_all(&external).unwrap();
        let mut options = CheckCommand::default();
        for (logical, physical, contents) in overlays {
            let physical = root.join(physical);
            std::fs::write(&physical, contents).unwrap();
            options.source_overlay.push(super::SourceOverlay {
                logical: external.join(logical),
                physical,
            });
        }
        let client = Arc::new(TestBazelClient::default());
        let (sender, receiver) = crossbeam_channel::unbounded();
        let loader =
            DefaultFileLoader::new(client.clone(), root.clone(), None, external, sender, true);
        let info = starpls_bazel::client::BazelInfo {
            workspace: root,
            ..Default::default()
        };
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        (
            Checker::new(analysis, info, vec![], &[], loader, receiver, &options).unwrap(),
            client,
        )
    }

    #[test]
    fn source_overlays_check_unfetched_build_files() {
        let (mut checker, client) = overlay_checker(
            "overlay-unfetched-build",
            &[(
                "stubs+/BUILD.bazel",
                "build.txt",
                "print(glob(['*.txt']))\n",
            )],
        );
        checker.report_diagnostics(None).unwrap();
        assert_eq!(checker.files.len(), 1);
        assert!(client.fetch_requests.lock().unwrap().is_empty());
        assert!(!checker
            .bazel_info
            .workspace
            .join("external/stubs+")
            .exists());
    }

    #[test]
    fn source_overlays_use_virtual_package_boundaries() {
        let (mut checker, client) = overlay_checker(
            "overlay-virtual-packages",
            &[
                ("stubs+/nested/BUILD.bazel", "build.txt", ""),
                (
                    "stubs+/nested/sub/helper.bzl",
                    "helper.txt",
                    "load(':dep.bzl', 'value')\nresult: str = value\n",
                ),
                ("stubs+/nested/dep.bzl", "dep.txt", "value = 'right'\n"),
                ("stubs+/dep.bzl", "wrong.txt", "value = 42\n"),
            ],
        );
        checker.report_diagnostics(None).unwrap();
        assert_eq!(checker.files.len(), 4);
        assert!(client.fetch_requests.lock().unwrap().is_empty());
    }

    #[test]
    fn source_overlays_fetch_dependencies_in_the_installed_repository() {
        let (mut checker, client) = overlay_checker(
            "overlay-demanded-dependency",
            &[(
                "stubs+/BUILD.bazel",
                "build.txt",
                "load('@dep//:defs.bzl', 'consume')\nconsume(42)\n",
            )],
        );
        client.fetch_files.lock().unwrap().insert(
            "rules+".to_owned(),
            (
                checker
                    .bazel_info
                    .workspace
                    .join("external/rules+/defs.bzl"),
                "def consume(value: str) -> None:\n    print(value)\n".to_owned(),
            ),
        );
        let result = checker.check_files().unwrap();
        assert!(result.loads.unresolved.is_empty());
        let [(_, diagnostics)] = result.diagnostics.as_slice() else {
            panic!("{:?}", result.diagnostics)
        };
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("{diagnostics:?}")
        };
        assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
        assert_eq!(*client.fetch_requests.lock().unwrap(), ["rules+"]);
        assert!(!checker
            .bazel_info
            .workspace
            .join("external/stubs+")
            .exists());
    }

    #[test]
    fn source_overlays_preserve_missing_dependency_errors() {
        let (mut checker, client) = overlay_checker(
            "overlay-missing-dependency",
            &[(
                "stubs+/nested/BUILD.bazel",
                "build.txt",
                "load(':missing.bzl', 'value')\nprint(value)\n",
            )],
        );
        client
            .fetch_failures
            .lock()
            .unwrap()
            .insert("stubs+".to_owned(), "archive unavailable".to_owned());
        let report_path = checker.bazel_info.workspace.join("report.json");
        assert!(checker.report_diagnostics(Some(&report_path)).is_err());
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["complete"], false);
        assert_eq!(report["unresolved_loads"].as_array().unwrap().len(), 1);
        assert_eq!(*client.fetch_requests.lock().unwrap(), ["stubs+"]);
    }

    #[test]
    fn source_overlays_validate_physical_siblings() {
        let (mut checker, client) = overlay_checker(
            "overlay-physical-sibling",
            &[(
                "stubs+/BUILD.bazel",
                "build.txt",
                "load(':dep.bzl', 'value')\nlen(value)\n",
            )],
        );
        let dependency = checker.bazel_info.workspace.join("external/stubs+/dep.bzl");
        std::fs::create_dir_all(dependency.parent().unwrap()).unwrap();
        std::fs::write(&dependency, "value = 'stale'\n").unwrap();
        client
            .fetch_files
            .lock()
            .unwrap()
            .insert("stubs+".to_owned(), (dependency, "value = 42\n".to_owned()));
        let result = checker.check_files().unwrap();
        assert!(result.loads.unresolved.is_empty());
        let [(_, diagnostics)] = result.diagnostics.as_slice() else {
            panic!("expected one overlay: {:?}", result.diagnostics)
        };
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("expected fresh sibling type: {diagnostics:?}")
        };
        assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
        assert_eq!(*client.fetch_requests.lock().unwrap(), ["stubs+"]);
    }

    #[test]
    #[cfg(unix)]
    fn source_overlays_share_warm_buffers_with_distinct_repository_contexts() {
        let name = "overlay-shared-buffer";
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap()).join(name);
        for repository in ["stubs+", "other+"] {
            let directory = root.join("external").join(repository);
            std::fs::create_dir_all(&directory).unwrap();
            std::os::unix::fs::symlink(root.join("shared.txt"), directory.join("BUILD.bazel"))
                .unwrap();
        }
        let source = "load('@dep//:defs.bzl', 'consume')\nconsume(42)\n";
        let (mut checker, client) = overlay_checker(
            name,
            &[
                ("stubs+/BUILD.bazel", "shared.txt", source),
                ("other+/BUILD.bazel", "shared.txt", source),
            ],
        );
        for (repository, annotation) in [("rules+", "str"), ("wrong+", "int")] {
            client.fetch_files.lock().unwrap().insert(
                repository.to_owned(),
                (
                    root.join("external").join(repository).join("defs.bzl"),
                    format!("def consume(value: {annotation}) -> None:\n    print(value)\n"),
                ),
            );
        }
        let result = checker.check_files().unwrap();
        assert!(result.loads.unresolved.is_empty());
        assert_eq!(result.diagnostics.len(), 2);
        let snapshot = checker.analysis.snapshot();
        for (file, diagnostics) in result.diagnostics {
            let expected = usize::from(snapshot.path(file).ends_with("stubs+/BUILD.bazel"));
            assert_eq!(diagnostics.len(), expected, "{diagnostics:?}");
            if let Some(diagnostic) = diagnostics.first() {
                assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
            }
        }
        assert_eq!(*client.fetch_requests.lock().unwrap(), ["rules+", "wrong+"]);
    }

    #[test]
    fn demand_checking_skips_unused_transitive_loads() {
        for audit in [false, true] {
            let mut checker = local_checker(
                &format!("checker-unused-load-{audit}"),
                &[
                    ("BUILD", "load(':dep.bzl', 'value')\nprint(value)\n"),
                    ("dep.bzl", "load(':missing.bzl', 'unused')\nvalue = 42\n"),
                ],
                audit,
            );
            let report_path = checker.bazel_info.workspace.join("coverage.json");
            let result = checker.report_diagnostics(Some(&report_path));
            assert_eq!(result.is_err(), audit, "{result:?}");
            let report: serde_json::Value =
                serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
            assert_eq!(report["version"], 2);
            assert_eq!(
                report["load_scope"],
                if audit { "transitive" } else { "requested" }
            );
            assert_eq!(report["complete"], !audit);
            assert_eq!(
                report["unresolved_loads"].as_array().unwrap().len(),
                usize::from(audit)
            );
        }
    }

    #[test]
    fn demanded_transitive_failures_survive_cached_queries() {
        let (mut checker, client, external) = fetch_checker("checker-demanded-failure", true);
        client.fetch_files.lock().unwrap().insert(
            "rules+".to_owned(),
            (
                external.join("rules+/defs.bzl"),
                "load('@dep//:child.bzl', 'child')\nvalue = child\n".to_owned(),
            ),
        );
        client
            .fetch_failures
            .lock()
            .unwrap()
            .insert("wrong+".to_owned(), "access refused".to_owned());
        let report_path = checker.bazel_info.workspace.join("coverage.json");
        for _ in 0..2 {
            assert!(checker.report_diagnostics(Some(&report_path)).is_err());
            let report: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
            assert_eq!(report["complete"], false);
            assert_eq!(report["checked_files"].as_array().unwrap().len(), 2);
            let loads = report["unresolved_loads"].as_array().unwrap();
            let [load] = loads.as_slice() else {
                panic!("expected demanded failure: {loads:?}")
            };
            assert_eq!(load["module"], "@dep//:child.bzl");
            assert!(load["message"].as_str().unwrap().contains("access refused"));
            assert_eq!(*client.fetch_requests.lock().unwrap(), ["rules+", "wrong+"]);
        }
    }

    #[test]
    fn load_cycles_are_audited_without_blocking_demand_inference() {
        for recursive_values in [false, true] {
            for audit in [false, true] {
                let value = if recursive_values { "other" } else { "42" };
                let a = format!("load(':b.bzl', other='value')\nvalue = {value}\n");
                let b = "load(':a.bzl', other='value')\nvalue = other\n";
                let mut checker = local_checker(
                    &format!("checker-cycle-{recursive_values}-{audit}"),
                    &[
                        ("BUILD", "load(':a.bzl', 'value')\nprint(value)\n"),
                        ("a.bzl", &a),
                        ("b.bzl", b),
                    ],
                    audit,
                );
                let super::CheckResult {
                    loads: graph,
                    diagnostics,
                } = checker.check_files().unwrap();
                assert!(graph.unresolved.is_empty());
                assert_eq!(
                    graph
                        .files
                        .iter()
                        .any(|file| { checker.analysis.snapshot().path(*file).ends_with("b.bzl") }),
                    audit || recursive_values,
                );
                let cycles = diagnostics
                    .iter()
                    .flat_map(|(_, diagnostics)| diagnostics)
                    .filter(|diagnostic| diagnostic.headline_message().contains("circular import"))
                    .count();
                assert_eq!(cycles, usize::from(audit));
            }
        }
    }

    #[test]
    fn mapping_commands_refresh_previously_read_sources() {
        for failed in [false, true] {
            let (mut checker, client, external) =
                fetch_checker(&format!("checker-mapping-refresh-{failed}"), true);
            for repository in ["rules+", "generated+"] {
                std::fs::create_dir_all(external.join(repository)).unwrap();
                checker.loader.finish_fetch([repository.to_owned()], Ok(()));
            }
            std::fs::write(external.join("rules+/defs.bzl"), "value = 42\n").unwrap();
            let generated = external.join("generated+/defs.bzl");
            std::fs::write(&generated, "value = 42\n").unwrap();
            let failure = if failed {
                std::fs::create_dir_all(external.join("fail+")).unwrap();
                checker.loader.finish_fetch(["fail+".to_owned()], Ok(()));
                std::fs::write(
                    external.join("fail+/defs.bzl"),
                    "load('@dep//:defs.bzl', 'value')\nother = value\n",
                )
                .unwrap();
                "load('@@fail+//:defs.bzl', 'other')\nprint(other)\n"
            } else {
                ""
            };
            let caller = checker.bazel_info.workspace.join("caller.bzl");
            std::fs::write(
                &caller,
                format!("load('@@generated+//:defs.bzl', 'value')\n{failure}len(value)\n"),
            )
            .unwrap();
            checker.load_file(&caller, true, &[], true).unwrap();
            let snapshot = checker.analysis.snapshot();
            let caller = checker
                .files
                .iter()
                .copied()
                .find(|file| snapshot.path(*file) == caller)
                .unwrap();
            let diagnostics = snapshot.diagnostics(caller).unwrap();
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.id().as_str() == "invalid-argument-type"),
                "{failed}: {diagnostics:?}"
            );
            drop(snapshot);
            *client.mapping_write.lock().unwrap() =
                Some((generated, "value = 'regenerated source'\n".to_owned()));

            let result = checker.check_files().unwrap();
            let (_, diagnostics) = result
                .diagnostics
                .iter()
                .find(|(file, _)| *file == caller)
                .unwrap();
            assert!(
                !diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.id().as_str() == "invalid-argument-type"),
                "{failed}: {diagnostics:?}"
            );
            let unresolved: Vec<_> = result.loads.unresolved.values().flatten().collect();
            if failed {
                assert!(!unresolved.is_empty(), "expected mapping failures");
                for edge in unresolved {
                    assert_eq!(
                        edge.resolution,
                        starpls_ide::LoadResolution::Failed(
                            "repository mapping batch failed: cannot evaluate requested repository batch"
                                .to_owned()
                        )
                    );
                }
            } else {
                assert!(
                    unresolved.is_empty(),
                    "unexpected load failures: {unresolved:?}"
                );
            }
            let fetches = client.fetch_requests.lock().unwrap();
            assert!(fetches.is_empty(), "unexpected fetches: {fetches:?}");
        }
    }

    fn check_removed_load(case: &str, old_source: Option<&str>) {
        let (mut checker, client, external) =
            fetch_checker(&format!("checker-mapping-removed-load-{case}"), true);
        std::fs::create_dir_all(external.join("rules+")).unwrap();
        std::fs::write(external.join("rules+/defs.bzl"), "value = 42\n").unwrap();
        checker.loader.finish_fetch(["rules+".to_owned()], Ok(()));
        let generated = checker.bazel_info.workspace.join("generated.bzl");
        let old = checker.bazel_info.workspace.join("old.bzl");
        if let Some(old_source) = old_source {
            std::fs::write(&old, old_source).unwrap();
        }
        std::fs::write(&generated, "load(':old.bzl', 'value')\nprint(value)\n").unwrap();
        checker.load_file(&generated, true, &[], true).unwrap();
        let snapshot = checker.analysis.snapshot();
        for file in &checker.files {
            snapshot.diagnostics(*file).unwrap();
        }
        let requested = checker.loader.recorded_loads();
        assert!(
            requested.iter().any(|(_, module)| module == ":old.bzl"),
            "old load was not requested: {requested:?}"
        );
        if case == "detached" {
            assert!(
                requested.iter().any(|(file, module)| {
                    snapshot.path(*file) == old && module == ":detached.bzl"
                }),
                "detached child was not requested: {requested:?}"
            );
        }
        drop(snapshot);
        *client.mapping_write.lock().unwrap() =
            Some((generated, "value = 'regenerated source'\n".to_owned()));

        let result = checker
            .check_files()
            .unwrap_or_else(|error| panic!("{case}: {error:#}"));
        assert!(
            result.loads.unresolved.is_empty(),
            "obsolete load remains unresolved: {:?}",
            result.loads.unresolved
        );
        let snapshot = checker.analysis.snapshot();
        let dependencies: Vec<_> = result
            .loads
            .files
            .iter()
            .map(|file| snapshot.path(*file))
            .collect();
        assert!(
            !dependencies.contains(&old.as_path()),
            "obsolete dependency remains loaded: {dependencies:?}"
        );
        let fetches = client.fetch_requests.lock().unwrap();
        assert!(fetches.is_empty(), "unexpected fetches: {fetches:?}");
    }

    #[test]
    fn mapping_commands_remove_missing_loads() {
        check_removed_load("missing", None);
    }

    #[test]
    fn mapping_commands_remove_resolved_loads() {
        check_removed_load("resolved", Some("value = 42\n"));
    }

    #[test]
    fn mapping_commands_remove_detached_loads() {
        check_removed_load(
            "detached",
            Some("load(':detached.bzl', 'child')\nvalue = child\n"),
        );
    }

    #[test]
    fn mapping_commands_audit_new_unused_loads() {
        for audit in [false, true] {
            let (mut checker, client, external) =
                fetch_checker(&format!("checker-mapping-added-load-{audit}"), true);
            if audit {
                checker.load_scope = super::LoadScope::Transitive;
            }
            std::fs::create_dir_all(external.join("rules+")).unwrap();
            std::fs::write(external.join("rules+/defs.bzl"), "value = 42\n").unwrap();
            checker.loader.finish_fetch(["rules+".to_owned()], Ok(()));
            let generated = checker.bazel_info.workspace.join("generated.bzl");
            std::fs::write(&generated, "value = 42\n").unwrap();
            let caller = checker.bazel_info.workspace.join("caller.bzl");
            std::fs::write(&caller, "load(':generated.bzl', 'value')\nprint(value)\n").unwrap();
            checker.load_file(&caller, true, &[], true).unwrap();
            *client.mapping_write.lock().unwrap() = Some((
                generated,
                "load(':new.bzl', 'unused')\nvalue = 42\n".to_owned(),
            ));

            let result = checker.check_files().unwrap();
            let unresolved: Vec<_> = result
                .loads
                .unresolved
                .values()
                .flatten()
                .map(|edge| edge.module.as_ref())
                .collect();
            assert_eq!(unresolved, if audit { vec![":new.bzl"] } else { vec![] });
            let fetches = client.fetch_requests.lock().unwrap();
            assert!(fetches.is_empty(), "unexpected fetches: {fetches:?}");
        }
    }

    #[test]
    fn fetches_materialize_transitive_loads_once() {
        let (mut checker, client, external) = fetch_checker("checker-fetch-transitive", true);
        client.fetch_files.lock().unwrap().extend([
            (
                "rules+".to_owned(),
                (
                    external.join("rules+/defs.bzl"),
                    "load('@dep//:child.bzl', child='value')\nvalue = child\n".to_owned(),
                ),
            ),
            (
                "wrong+".to_owned(),
                (external.join("wrong+/child.bzl"), "value = 42\n".to_owned()),
            ),
        ]);
        let report_path = checker.bazel_info.workspace.join("coverage.json");
        checker.report_diagnostics(Some(&report_path)).unwrap();
        assert_eq!(*client.fetch_requests.lock().unwrap(), ["rules+", "wrong+"]);
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["complete"], true);
        assert_eq!(report["checked_files"].as_array().unwrap().len(), 2);
        assert_eq!(report["loaded_dependencies"].as_array().unwrap().len(), 2);
        assert!(report["unresolved_loads"].as_array().unwrap().is_empty());
    }

    #[test]
    fn existing_repositories_are_refreshed_before_inference() {
        for (name, old, new, expected_errors) in [
            ("missing", None, "str", 1),
            ("stale-valid", Some("int"), "str", 1),
            ("stale-invalid", Some("str"), "int", 0),
        ] {
            let (mut checker, client, external) =
                fetch_checker(&format!("checker-refresh-{name}"), true);
            let dependency = external.join("rules+/defs.bzl");
            std::fs::create_dir_all(dependency.parent().unwrap()).unwrap();
            let contents = |annotation| {
                format!("value = 42\ndef consume(value: {annotation}) -> None:\n    print(value)\n")
            };
            if let Some(old) = old {
                std::fs::write(&dependency, contents(old)).unwrap();
            }
            client
                .fetch_files
                .lock()
                .unwrap()
                .insert("rules+".to_owned(), (dependency, contents(new)));
            let caller = checker.bazel_info.workspace.join("caller.bzl");
            std::fs::write(
                &caller,
                "load('@rules//:defs.bzl', 'consume')\nconsume(42)\n",
            )
            .unwrap();
            checker.load_file(&caller, true, &[], true).unwrap();
            for _ in 0..2 {
                let result = checker.check_files().unwrap();
                assert!(result.loads.unresolved.is_empty());
                let diagnostics: Vec<_> = result
                    .diagnostics
                    .iter()
                    .flat_map(|(_, diagnostics)| diagnostics)
                    .collect();
                assert_eq!(
                    diagnostics.len(),
                    expected_errors,
                    "{name}: {diagnostics:?}"
                );
                for diagnostic in diagnostics {
                    assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
                }
                assert_eq!(*client.fetch_requests.lock().unwrap(), ["rules+"]);
            }
        }
    }

    #[test]
    fn failed_validation_rejects_existing_sources() {
        for bzlmod in [false, true] {
            let (mut checker, client, external) =
                fetch_checker(&format!("checker-stale-failure-{bzlmod}"), bzlmod);
            std::fs::create_dir_all(external.join("rules+")).unwrap();
            std::fs::write(external.join("rules+/defs.bzl"), "value = 42\n").unwrap();
            client
                .fetch_failures
                .lock()
                .unwrap()
                .insert("rules+".to_owned(), "validation refused".to_owned());
            for _ in 0..2 {
                let result = checker.check_files().unwrap();
                let unresolved: Vec<_> = result.loads.unresolved.values().flatten().collect();
                assert_eq!(unresolved.len(), 2);
                for edge in unresolved {
                    let starpls_ide::LoadResolution::Failed(message) = &edge.resolution else {
                        panic!("expected fetch failure: {edge:?}")
                    };
                    assert!(message.contains("validation refused"), "{message}");
                }
                assert_eq!(*client.fetch_requests.lock().unwrap(), ["rules+"]);
            }
        }
    }

    #[test]
    fn selected_external_inputs_are_refreshed_before_discovery() {
        for (directory, missing, failed) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap()).join(
                format!("checker-selected-repository-{directory}-{missing}-{failed}"),
            );
            let external = root.join("external");
            let repository = external.join("rules+");
            std::fs::create_dir_all(&external).unwrap();
            let source = repository.join("defs.bzl");
            if !missing {
                std::fs::create_dir_all(&repository).unwrap();
                std::fs::write(&source, "value: str = 'stale'\n").unwrap();
            }
            let client = Arc::new(TestBazelClient::default());
            if failed {
                client
                    .fetch_failures
                    .lock()
                    .unwrap()
                    .insert("rules+".to_owned(), "validation refused".to_owned());
            } else {
                client.fetch_files.lock().unwrap().insert(
                    "rules+".to_owned(),
                    (source.clone(), "value: str = 42\n".to_owned()),
                );
            }
            let (sender, receiver) = crossbeam_channel::unbounded();
            let loader =
                DefaultFileLoader::new(client.clone(), root.clone(), None, external, sender, true);
            let info = starpls_bazel::client::BazelInfo {
                workspace: root,
                ..Default::default()
            };
            let options = CheckCommand::default();
            let (analysis, loader) = options
                .prepare_analysis(loader, &info, Default::default())
                .unwrap();
            let selected = if directory { &repository } else { &source };
            let mut checker = Checker::new(
                analysis,
                info,
                vec![selected.to_str().unwrap().to_owned()],
                &[],
                loader,
                receiver,
                &options,
            )
            .unwrap();
            if failed {
                let [error] = checker.input_errors.as_slice() else {
                    panic!("expected one failed input")
                };
                assert!(error.message.contains("validation refused"));
                assert!(checker.files.is_empty());
                assert!(checker.report_diagnostics(None).is_err());
                assert_eq!(*client.fetch_requests.lock().unwrap(), ["rules+"]);
                continue;
            }
            let result = checker.check_files().unwrap();
            assert!(checker.input_errors.is_empty());
            assert!(result.loads.unresolved.is_empty());
            let [(_, diagnostics)] = result.diagnostics.as_slice() else {
                panic!("expected one source: {:?}", result.diagnostics)
            };
            let [diagnostic] = diagnostics.as_slice() else {
                panic!("expected updated source: {diagnostics:?}")
            };
            assert_eq!(diagnostic.id().as_str(), "invalid-assignment");
            assert_eq!(*client.fetch_requests.lock().unwrap(), ["rules+"]);
        }
    }

    #[test]
    fn explicit_external_interfaces_are_refreshed_before_installation() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-explicit-interface-freshness");
        let external = root.join("external");
        let interface = external.join("stubs+/defs.bzli");
        std::fs::create_dir_all(interface.parent().unwrap()).unwrap();
        std::fs::write(&interface, "value: str\n").unwrap();
        std::fs::write(root.join("defs.bzl"), "value = 'implementation'\n").unwrap();
        let caller = root.join("BUILD");
        std::fs::write(&caller, "load(':defs.bzl', 'value')\nlen(value)\n").unwrap();
        let client = Arc::new(TestBazelClient::default());
        client
            .fetch_files
            .lock()
            .unwrap()
            .insert("stubs+".to_owned(), (interface, "value: int\n".to_owned()));
        let (sender, receiver) = crossbeam_channel::unbounded();
        let loader =
            DefaultFileLoader::new(client.clone(), root.clone(), None, external, sender, true);
        let info = starpls_bazel::client::BazelInfo {
            workspace: root,
            ..Default::default()
        };
        let matches = <CheckCommand as clap::Args>::augment_args(clap::Command::new("check"))
            .try_get_matches_from([
                "check",
                "--type_interface",
                "defs.bzl=external/stubs+/defs.bzli",
            ])
            .unwrap();
        let options = <CheckCommand as clap::FromArgMatches>::from_arg_matches(&matches).unwrap();
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        let mut checker = Checker::new(
            analysis,
            info,
            vec![caller.to_str().unwrap().to_owned()],
            &[],
            loader,
            receiver,
            &options,
        )
        .unwrap();
        let result = checker.check_files().unwrap();
        assert!(result.loads.unresolved.is_empty());
        let diagnostics: Vec<_> = result
            .diagnostics
            .iter()
            .flat_map(|(_, diagnostics)| diagnostics)
            .collect();
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("expected fresh interface type: {diagnostics:?}")
        };
        assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
        assert_eq!(*client.fetch_requests.lock().unwrap(), ["stubs+"]);
    }

    #[test]
    fn failed_or_incomplete_fetches_terminate_with_load_errors() {
        for mode in ["failed", "partial", "empty", "existing"] {
            let (mut checker, client, external) =
                fetch_checker(&format!("checker-fetch-{mode}"), true);
            if mode == "existing" {
                std::fs::create_dir_all(external.join("rules+")).unwrap();
            }
            if mode == "partial" {
                client.fetch_files.lock().unwrap().insert(
                    "rules+".to_owned(),
                    (external.join("rules+/defs.bzl"), "value = 42\n".to_owned()),
                );
            }
            if matches!(mode, "failed" | "partial") {
                client.fetch_failures.lock().unwrap().insert(
                    "rules+".to_owned(),
                    "download refused by test server".to_owned(),
                );
            }
            let report_path = checker.bazel_info.workspace.join("coverage.json");
            for _ in 0..2 {
                assert!(checker.report_diagnostics(Some(&report_path)).is_err());
                let requests = client.fetch_requests.lock().unwrap();
                assert_eq!(requests.len(), 1, "{mode}: {requests:?}");
                let report: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
                assert_eq!(report["complete"], false);
                let loads = report["unresolved_loads"].as_array().unwrap();
                assert_eq!(loads.len(), 2, "{mode}: {loads:?}");
                for load in loads {
                    let message = load["message"].as_str().unwrap();
                    if matches!(mode, "failed" | "partial") {
                        assert!(
                            message.contains("failed to fetch repository @@rules+"),
                            "{mode}: {message}"
                        );
                        assert!(
                            message.contains("download refused by test server"),
                            "{mode}: {message}"
                        );
                    } else {
                        assert!(message.contains("Not found"), "{mode}: {message}");
                    }
                }
            }
        }
    }

    #[test]
    fn fetch_batches_preserve_individual_results_and_do_not_repeat() {
        for failed in [false, true] {
            let (mut checker, client, external) =
                fetch_checker(&format!("checker-fetch-batch-{failed}"), true);
            checker.loader.resolve_repository_mappings(&[String::new()]);
            let extra = checker.bazel_info.workspace.join("extra.bzl");
            std::fs::write(
                &extra,
                "load('@@good+//:defs.bzl', 'good')\nload('@@bad+//:defs.bzl', 'bad')\nprint(good, bad)\n",
            )
            .unwrap();
            checker.load_file(&extra, true, &[], true).unwrap();
            client.fetch_files.lock().unwrap().extend([
                (
                    "rules+".to_owned(),
                    (external.join("rules+/defs.bzl"), "value = 42\n".to_owned()),
                ),
                (
                    "good+".to_owned(),
                    (external.join("good+/defs.bzl"), "good = 42\n".to_owned()),
                ),
                (
                    "bad+".to_owned(),
                    (external.join("bad+/defs.bzl"), "bad = 42\n".to_owned()),
                ),
            ]);
            if failed {
                client.fetch_failures.lock().unwrap().insert(
                    "bad+".to_owned(),
                    "repository authentication failed".to_owned(),
                );
            }
            let report_path = checker.bazel_info.workspace.join("coverage.json");
            for _ in 0..2 {
                let result = checker.report_diagnostics(Some(&report_path));
                assert_eq!(result.is_err(), failed, "{result:?}");
                let requests = client.fetch_requests.lock().unwrap();
                assert_eq!(requests.len(), if failed { 6 } else { 3 });
                let batches = client.fetch_batches.lock().unwrap();
                assert_eq!(batches.len(), if failed { 4 } else { 1 });
                assert_eq!(batches[0].len(), 3);
                if failed {
                    assert!(batches[1..].iter().all(|batch| batch.len() == 1));
                }
                let report: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
                assert_eq!(report["complete"], !failed);
                let unresolved = report["unresolved_loads"].as_array().unwrap();
                assert_eq!(unresolved.len(), usize::from(failed));
                if failed {
                    assert_eq!(unresolved[0]["module"], "@@bad+//:defs.bzl");
                    assert!(unresolved[0]["message"]
                        .as_str()
                        .unwrap()
                        .contains("repository authentication failed"));
                }
            }
        }
    }

    #[test]
    fn failed_legacy_queries_reject_materialized_sources() {
        let (mut checker, client, external) = fetch_checker("checker-fetch-legacy", false);
        client.fetch_files.lock().unwrap().insert(
            "rules+".to_owned(),
            (external.join("rules+/defs.bzl"), "value = 42\n".to_owned()),
        );
        client.fetch_failures.lock().unwrap().insert(
            "rules+".to_owned(),
            "unrelated package failed to load".to_owned(),
        );
        let report_path = checker.bazel_info.workspace.join("coverage.json");
        for _ in 0..2 {
            assert!(checker.report_diagnostics(Some(&report_path)).is_err());
            assert_eq!(*client.fetch_requests.lock().unwrap(), ["rules+"]);
            let report: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
            assert_eq!(report["complete"], false);
            assert_eq!(report["loaded_dependencies"].as_array().unwrap().len(), 0);
        }
    }

    #[test]
    #[cfg(unix)]
    fn fetched_symlinks_preserve_repository_context() {
        let (mut checker, client, external) = fetch_checker("checker-fetch-symlink", true);
        let physical = checker.bazel_info.workspace.join("physical");
        std::fs::create_dir_all(&physical).unwrap();
        std::fs::write(
            physical.join("defs.bzl"),
            "load('//:helper.bzl', 'value')\n",
        )
        .unwrap();
        std::fs::write(physical.join("helper.bzl"), "value = 42\n").unwrap();
        *client.retarget.lock().unwrap() = Some((external.join("rules+"), physical.clone()));
        checker.load_scope = super::LoadScope::Transitive;
        let graph = checker.check_files().unwrap().loads;
        let snapshot = checker.analysis.snapshot();
        let report = checker
            .coverage_report(&snapshot, &graph, &checker.files, Default::default())
            .unwrap();
        assert!(report.complete);
        assert_eq!(*client.fetch_requests.lock().unwrap(), ["rules+"]);
        assert_eq!(report.loaded_dependencies.len(), 2);
        for file in report.loaded_dependencies {
            assert_eq!(file.repository.as_deref(), Some("rules+"));
            assert!(file.path.starts_with(external.join("rules+")));
        }
    }

    #[test]
    fn file_inventories_preserve_spaces_and_accept_crlf() {
        let mut paths = vec!["BUILD".to_owned()];
        super::read_file_list(
            b"dir with spaces/rules.bzl\r\n\nlast.bzl\n".as_slice(),
            &mut paths,
        )
        .unwrap();
        assert_eq!(paths, ["BUILD", "dir with spaces/rules.bzl", "last.bzl"]);
    }

    #[test]
    fn exclusions_agree_across_recursive_explicit_and_inventory_inputs() {
        let sources = [
            (
                "BUILD",
                "load('//project/sky/repos:defs.bzl', 'value')\nlen(value)\n",
            ),
            ("project/sky/BUILD.bazel", ""),
            ("project/sky/repos/defs.bzl", "value = 42\n"),
            ("project/sky/repos/deep/bad.bzl", "excluded_name\n"),
            ("project/sky/repos_extra/bad.bzl", "selected_name\n"),
            ("other/repos/bad.bzl", "def broken(:\n"),
            ("project/fragment.BUILD.bazel", "excluded_name\n"),
            ("other/fragment.BUILD.bazel", "selected_name\n"),
            ("cache/deep/bad.bzl", "excluded_name\n"),
            ("other/cache/deep/bad.bzl", "excluded_name\n"),
            ("only.bzl", "excluded_name\n"),
            ("other/only.bzl", "selected_name\n"),
        ];
        for mode in ["recursive", "explicit", "inventory"] {
            let inventory = sources
                .iter()
                .map(|(path, _)| *path)
                .collect::<Vec<_>>()
                .join("\n");
            let mut inputs = sources.to_vec();
            inputs.push(("inputs.txt", &inventory));
            let mut options = CheckCommand {
                ignore_patterns: [
                    "project/sky/unused/../repos/",
                    "./project/fragment.BUILD.bazel",
                    "cache",
                    "./only.bzl",
                ]
                .map(str::to_owned)
                .to_vec(),
                ..Default::default()
            };
            match mode {
                "recursive" => options.paths.push(".".to_owned()),
                "explicit" => {
                    options.paths = sources.iter().map(|(path, _)| (*path).to_owned()).collect();
                    // Input normalization must not turn the lexical parent into an exclusion.
                    options
                        .paths
                        .push("project/sky/repos/../BUILD.bazel".to_owned());
                }
                "inventory" => options.files_from = Some("inputs.txt".into()),
                _ => unreachable!(),
            }
            let mut checker =
                local_checker_with_options(&format!("checker-exclusions-{mode}"), &inputs, options);
            let root = checker.bazel_info.workspace.clone();
            let report_path = root.join("coverage.json");
            assert!(checker.report_diagnostics(Some(&report_path)).is_err());
            let report: serde_json::Value =
                serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
            let selected: std::collections::BTreeSet<_> = report["selected_files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|file| file["path"].as_str().unwrap().to_owned())
                .collect();
            let expected = [
                "BUILD",
                "project/sky/BUILD.bazel",
                "project/sky/repos_extra/bad.bzl",
                "other/repos/bad.bzl",
                "other/fragment.BUILD.bazel",
                "other/only.bzl",
            ]
            .map(|path| root.join(path).to_str().unwrap().to_owned())
            .into_iter()
            .collect();
            assert_eq!(selected, expected, "{mode}: {report}");
            assert_eq!(
                report["checked_files"].as_array().unwrap().len(),
                selected.len()
            );
            assert_eq!(report["complete"], true, "{report}");
            assert_eq!(report["loaded_dependencies"].as_array().unwrap().len(), 1);
            assert_eq!(
                report["loaded_dependencies"][0]["path"],
                root.join("project/sky/repos/defs.bzl").to_str().unwrap()
            );
            let excluded = if mode == "recursive" {
                "project/sky/repos"
            } else {
                "project/sky/repos/deep/bad.bzl"
            };
            assert_eq!(
                report["excluded_inputs"][root.join(excluded).to_str().unwrap()],
                "ignored"
            );
            let checked = checker.check_files().unwrap();
            let snapshot = checker.analysis.snapshot();
            for (file, diagnostics) in &checked.diagnostics {
                if snapshot.path(*file) == root.join("project/sky/BUILD.bazel") {
                    assert!(diagnostics.is_empty(), "{diagnostics:?}");
                } else {
                    assert!(
                        !diagnostics.is_empty(),
                        "missing diagnostics for {}",
                        snapshot.path(*file).display()
                    );
                }
                if snapshot.path(*file) == root.join("BUILD") {
                    assert!(
                        diagnostics
                            .iter()
                            .any(|d| d.id().as_str() == "invalid-argument-type"),
                        "{diagnostics:?}"
                    );
                }
            }
            drop(snapshot);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn exclusions_preserve_stub_contracts() {
        let matches = <CheckCommand as clap::Args>::augment_args(clap::Command::new("check"))
            .try_get_matches_from([
                "check",
                "BUILD",
                "--type_interface",
                "impl/hidden.bzl=types/hidden.bzli",
                "--type_interface",
                "other/hidden.bzl=types/other.bzli",
                "--type_interface",
                "impl/unused.bzl=types/unused.bzli",
                "--ignore_pattern",
                "impl/",
                "--ignore_pattern",
                "types/",
                "--audit-loads",
            ])
            .unwrap();
        let options = <CheckCommand as clap::FromArgMatches>::from_arg_matches(&matches).unwrap();
        let mut checker = local_checker_with_options(
            "checker-excluded-stub-contracts",
            &[
                ("BUILD", "load('//impl:hidden.bzl', 'value')\nlen(value)\n"),
                ("impl/hidden.bzl", "value = 'wrong'\n"),
                (
                    "impl/unused.bzl",
                    "load(':missing.bzl', 'unused')\nvalue = 'wrong'\n",
                ),
                ("types/unused.bzli", "value: int\n"),
                (
                    "types/hidden.bzli",
                    "load('//:types/helper.bzli', 'Value')\nvalue: Value\nunrelated = missing\n",
                ),
                (
                    "types/helper.bzli",
                    "class Value(Protocol):\n    def marker(self) -> int: ...\n",
                ),
                ("other/hidden.bzl", "value = 'wrong'\n"),
                ("types/other.bzli", "value: int\n"),
            ],
            options,
        );
        let root = checker.bazel_info.workspace.clone();
        assert_eq!(checker.files.len(), 1);
        checker.load_scope = super::LoadScope::Requested;
        let checked = checker.check_files().unwrap();
        let [(file, diagnostics)] = checked.diagnostics.as_slice() else {
            panic!("expected only selected caller diagnostics");
        };
        let snapshot = checker.analysis.snapshot();
        assert_eq!(snapshot.path(*file), root.join("BUILD"));
        let requested = checker.loader.recorded_loads();
        assert!(
            requested.iter().any(|(file, module)| {
                snapshot.path(*file) == root.join("types/hidden.bzli")
                    && module == "//:types/helper.bzli"
            }),
            "interface helper was not requested: {requested:?}"
        );
        assert!(
            checked.loads.unresolved.is_empty(),
            "{:?}",
            checked.loads.unresolved
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.id().as_str() == "invalid-argument-type"),
            "{diagnostics:?}"
        );
        let dependencies: Vec<_> = checked
            .loads
            .files
            .iter()
            .map(|file| snapshot.path(*file))
            .collect();
        assert!(
            dependencies.contains(&root.join("types/helper.bzli").as_path()),
            "excluded interface helper was not loaded: {dependencies:?}"
        );
        drop(snapshot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exclusion_paths_must_stay_workspace_relative() {
        let workspace = std::env::current_dir().unwrap();
        for pattern in [
            "../outside",
            "nested/../../outside",
            workspace.to_str().unwrap(),
        ] {
            assert!(
                super::IgnoredPaths::new(&workspace, &[pattern.to_owned()]).is_err(),
                "{pattern}"
            );
        }
    }

    #[test]
    fn excluded_dependencies_still_report_requested_load_failures() {
        let mut checker = local_checker_with_options(
            "checker-excluded-load-failure",
            &[
                (
                    "BUILD",
                    "load('//ignored:defs.bzl', 'value')\nprint(value)\n",
                ),
                (
                    "ignored/defs.bzl",
                    "load(':missing.bzl', 'imported')\nvalue = imported\n",
                ),
            ],
            CheckCommand {
                paths: vec!["BUILD".to_owned(), "ignored/defs.bzl".to_owned()],
                ignore_patterns: vec!["./ignored".to_owned()],
                ..Default::default()
            },
        );
        let root = checker.bazel_info.workspace.clone();
        let report_path = root.join("coverage.json");
        assert!(checker.report_diagnostics(Some(&report_path)).is_err());
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["complete"], false);
        assert_eq!(report["unresolved_loads"][0]["module"], ":missing.bzl");
        assert_eq!(
            report["excluded_inputs"][root.join("ignored/defs.bzl").to_str().unwrap()],
            "ignored"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn coverage_reports_transitive_failures_and_completed_roots() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-coverage");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("BUILD"), "load(':dep.bzl')\n").unwrap();
        std::fs::write(root.join("dep.bzl"), "load(':missing.bzl')\n").unwrap();
        std::fs::write(root.join("deploy.star"), "fail('excluded')\n").unwrap();
        std::fs::write(root.join("template.bzl.in"), "@@template@@\n").unwrap();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            root.clone(),
            None,
            None,
            sender,
            false,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: root.clone(),
            ..Default::default()
        };
        let options = CheckCommand {
            bazel_only: true,
            audit_loads: true,
            ..Default::default()
        };
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        let paths = [
            "BUILD",
            "BUILD",
            "deploy.star",
            "template.bzl.in",
            "absent.bzl",
        ]
        .map(|path| root.join(path).to_str().unwrap().to_owned())
        .to_vec();
        let mut checker =
            Checker::new(analysis, info, paths, &["star"], loader, receiver, &options).unwrap();
        let report_path = root.join("coverage.json");
        assert!(checker.report_diagnostics(Some(&report_path)).is_err());
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["complete"], false);
        assert_eq!(report["selected_files"].as_array().unwrap().len(), 1);
        assert_eq!(report["checked_files"].as_array().unwrap().len(), 1);
        assert_eq!(report["loaded_dependencies"].as_array().unwrap().len(), 1);
        assert_eq!(report["input_errors"].as_array().unwrap().len(), 1);
        let loads = report["unresolved_loads"].as_array().unwrap();
        let [load] = loads.as_slice() else {
            panic!("expected transitive failure: {loads:?}");
        };
        assert_eq!(load["module"], ":missing.bzl");
        assert!(load["source"]["path"]
            .as_str()
            .unwrap()
            .ends_with("/dep.bzl"));
        assert_eq!(
            report["excluded_inputs"][root.join("deploy.star").to_str().unwrap()],
            "not_bazel"
        );
        assert_eq!(
            report["excluded_inputs"][root.join("template.bzl.in").to_str().unwrap()],
            "unsupported_file"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recursive_selection_records_nested_repository_boundaries() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-selection");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("BUILD"), "").unwrap();
        std::fs::write(root.join("nested/MODULE.bazel"), "").unwrap();
        std::fs::write(root.join("nested/defs.bzl"), "").unwrap();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            root.clone(),
            None,
            None,
            sender,
            false,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: root.clone(),
            ..Default::default()
        };
        let options = CheckCommand::default();
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        let checker = Checker::new(
            analysis,
            info,
            vec![root.to_str().unwrap().to_owned()],
            &[],
            loader,
            receiver,
            &options,
        )
        .unwrap();
        assert_eq!(checker.files.len(), 1);
        assert!(matches!(
            checker.exclusions.get(&root.join("nested")),
            Some(super::Exclusion::NestedRepository)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn explicit_source_symlinks_are_checked_and_unsupported_paths_fail() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-explicit-paths");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("overlay.txt"), "value = 1\n").unwrap();
        std::fs::write(root.join("unsupported.py"), "value = 1\n").unwrap();
        std::os::unix::fs::symlink(root.join("overlay.txt"), root.join("BUILD.bazel")).unwrap();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            root.clone(),
            None,
            None,
            sender,
            false,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: root.clone(),
            ..Default::default()
        };
        let options = CheckCommand::default();
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        let paths = ["BUILD.bazel", "unsupported.py"]
            .map(|name| root.join(name).to_str().unwrap().to_owned())
            .to_vec();
        let mut checker =
            Checker::new(analysis, info, paths, &[], loader, receiver, &options).unwrap();
        assert_eq!(checker.files.len(), 1);
        assert_eq!(
            checker.files.iter().next().unwrap().api_context(),
            Some(starpls_bazel::APIContext::Build)
        );
        assert_eq!(checker.input_errors.len(), 1);
        checker.load_scope = super::LoadScope::Transitive;
        let graph = checker.check_files().unwrap().loads;
        let snapshot = checker.analysis.snapshot();
        let report = checker
            .coverage_report(&snapshot, &graph, &checker.files, Default::default())
            .unwrap();
        assert!(!report.complete);
        assert_eq!(
            report.checked_files.first().unwrap().path,
            root.join("BUILD.bazel")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_repository_context_remains_incomplete_without_queued_work() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-pending");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("BUILD"), "load('@unknown//:defs.bzl')\n").unwrap();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            root.clone(),
            None,
            None,
            sender,
            true,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: root.clone(),
            ..Default::default()
        };
        let options = CheckCommand::default();
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        let mut checker = Checker::new(
            analysis,
            info,
            vec![root.join("BUILD").to_str().unwrap().to_owned()],
            &[],
            loader,
            receiver,
            &options,
        )
        .unwrap();
        let graph = checker.check_files().unwrap().loads;
        assert!(checker.loader.pending_repository_mappings().is_empty());
        let report = checker
            .coverage_report(
                &checker.analysis.snapshot(),
                &graph,
                &checker.files,
                Default::default(),
            )
            .unwrap();
        assert!(!report.complete);
        let [load] = report.unresolved_loads.as_slice() else {
            panic!("expected unresolved load");
        };
        assert_eq!(load.module.as_ref(), "@unknown//:defs.bzl");
        assert_eq!(load.message, "load resolution is still pending");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_stub_packages_prepare_their_source_dependency_graph() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-stub-graph");
        let workspace = root.join("workspace");
        let external = root.join("external");
        for path in [
            &workspace,
            &external.join("stubs+"),
            &external.join("rules+"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(
            workspace.join("starpls.toml"),
            "[[stub-packages]]\nmanifest='@stubs//:package.toml'\nallow-unversioned=true\n",
        )
        .unwrap();
        std::fs::write(external.join("stubs+/package.toml"),
            "format-version=1\n[source]\nrepository='@dep'\nmodule='rules'\nversions=['1']\n[files]\n'defs.bzl'='defs.bzli'\n").unwrap();
        std::fs::write(
            external.join("stubs+/defs.bzli"),
            "def value() -> int: ...\n",
        )
        .unwrap();
        std::fs::write(
            external.join("rules+/defs.bzl"),
            "load('@dep//:value.bzl', 'helper')\ndef value(): return helper\n",
        )
        .unwrap();
        let client = Arc::new(TestBazelClient::default());
        client.fetch_files.lock().unwrap().insert(
            "wrong+".to_owned(),
            (
                external.join("wrong+/value.bzl"),
                "helper = 42\n".to_owned(),
            ),
        );
        let (sender, receiver) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            client.clone(),
            workspace.clone(),
            None,
            external,
            sender,
            true,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: workspace.clone(),
            ..Default::default()
        };
        let (analysis, loader) = CheckCommand::default()
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        assert_eq!(
            client.mapping_requests.lock().unwrap().as_slice(),
            &[vec![String::new()], vec!["stubs+".to_owned()]]
        );
        let mut checker = Checker::new(
            analysis,
            info,
            Vec::new(),
            &[],
            loader,
            receiver,
            &CheckCommand::default(),
        )
        .unwrap();
        checker.report_diagnostics(None).unwrap();
        assert_eq!(*client.fetch_requests.lock().unwrap(), ["stubs+", "rules+"]);
        std::fs::remove_dir_all(root).unwrap();
    }
}

impl CheckCommand {
    fn input_paths(&self) -> anyhow::Result<Vec<String>> {
        let mut paths = self.paths.clone();
        if let Some(path) = &self.files_from {
            if path == Path::new("-") {
                read_file_list(std::io::stdin().lock(), &mut paths)?;
            } else {
                let file = std::fs::File::open(path)
                    .with_context(|| format!("cannot open file inventory {}", path.display()))?;
                read_file_list(std::io::BufReader::new(file), &mut paths)?;
            }
        }
        Ok(paths)
    }

    pub(crate) fn run(self) -> anyhow::Result<()> {
        let paths = self.input_paths()?;
        if self.progress {
            eprintln!("Initializing Bazel context");
        }
        let bazel_client = Arc::new(BazelCLI::default());
        let bazel_cx = BazelContext::new(&*bazel_client)
            .map_err(|err| anyhow!("failed to initialize Bazel context: {}", err))?;
        let (fetch_repo_sender, fetch_repo_receiver) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            bazel_client,
            bazel_cx.info.workspace.clone(),
            bazel_cx.info.workspace_name.clone(),
            bazel_cx.info.output_base.join("external"),
            fetch_repo_sender,
            bazel_cx.bzlmod_enabled,
        );
        loader.finish_mapping(String::new(), Ok(bazel_cx.main_repo_mapping));
        let (analysis, loader) = self.prepare_analysis(loader, &bazel_cx.info, bazel_cx.rules)?;

        // Strip off the leading "." from each of the specified extensions.
        // This works better when filtering against files with .extension().
        let extensions = self
            .extensions
            .iter()
            .map(|ext| match ext.strip_prefix('.') {
                Some(ext) => ext,
                None => ext,
            })
            .chain(["star", "sky"])
            .collect::<Vec<_>>();

        let mut checker = Checker::new(
            analysis,
            bazel_cx.info,
            paths,
            &extensions,
            loader,
            fetch_repo_receiver,
            &self,
        )?;
        checker.report_diagnostics(self.report.as_deref())
    }

    fn prepare_analysis(
        &self,
        loader: DefaultFileLoader,
        info: &BazelInfo,
        rules: starpls_bazel::build::BuildLanguage,
    ) -> anyhow::Result<(Analysis, Arc<DefaultFileLoader>)> {
        let mut overlays = HashSet::new();
        for SourceOverlay {
            logical,
            physical: _,
        } in &self.source_overlay
        {
            let logical = starpls_common::absolute_path(logical)?;
            anyhow::ensure!(
                overlays.insert(logical.clone()),
                "duplicate source overlay for {}",
                logical.display()
            );
        }
        let loader = loader.for_check(overlays);
        // Package manifests can resolve source repositories relative to an
        // external annotation repository. Finish those synchronous queries
        // before deferring mappings discovered through the source graph.
        let prepared = self.type_interfaces.prepare(&loader, &info.workspace)?;
        loader.fetch_source_repositories(prepared.paths(), |message| {
            if self.progress {
                eprintln!("{message}");
            }
        })?;
        for path in prepared.paths() {
            loader.ensure_repository_for_path(path)?;
        }
        let loader = Arc::new(loader.with_deferred_mappings().with_load_recording());
        let mut analysis = Analysis::new(
            loader.clone(),
            starpls_ide::InferenceOptions {
                infer_ctx_attributes: self.inference_options.infer_ctx_attributes,
                use_code_flow_analysis: self.inference_options.use_code_flow_analysis,
                skip_load_cycle_checks: !self.audit_loads,
                ..Default::default()
            },
        )?;
        analysis.set_builtin_defs(load_bazel_builtins(), rules)?;
        for SourceOverlay { logical, physical } in &self.source_overlay {
            let logical = starpls_common::absolute_path(logical)?;
            let Some((Dialect::Bazel, context)) =
                document::source_kind(&info.workspace, &logical, &[])
            else {
                anyhow::bail!(
                    "source overlay requires a Bazel path: {}",
                    logical.display()
                );
            };
            let contents = std::fs::read_to_string(physical)
                .with_context(|| format!("cannot read source overlay {}", physical.display()))?;
            let file_info = context.map(|api_context| FileInfo::Bazel {
                api_context,
                is_external: false,
            });
            if let Some(document) = analysis.document(&logical) {
                anyhow::ensure!(
                    document.contents == contents,
                    "conflicting source overlays for {} and {}",
                    document.path,
                    logical.display()
                );
                analysis.file(&logical, Dialect::Bazel, file_info)?;
            } else {
                analysis.open_document(&logical, Dialect::Bazel, file_info, contents, 0)?;
            }
        }
        prepared.install(&mut analysis, &info.workspace)?;
        Ok((analysis, loader))
    }
}

fn read_file_list(reader: impl BufRead, paths: &mut Vec<String>) -> anyhow::Result<()> {
    for line in reader.lines() {
        let line = line?;
        if !line.is_empty() {
            paths.push(line);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Exclusion {
    UnsupportedFile,
    NotBazel,
    Ignored,
    NestedRepository,
}

#[derive(Serialize)]
struct InputError {
    path: PathBuf,
    message: String,
}

#[derive(Serialize)]
struct ReportFile {
    path: PathBuf,
    repository: Option<String>,
}

#[derive(Serialize)]
struct UnresolvedLoad {
    source: ReportFile,
    module: Box<str>,
    start: u32,
    end: u32,
    message: String,
}

#[derive(Default, Serialize)]
struct DiagnosticCounts {
    errors: usize,
    warnings: usize,
    infos: usize,
}

#[derive(Serialize)]
struct CoverageReport<'a> {
    version: u32,
    load_scope: LoadScope,
    workspace: &'a Path,
    bazel_release: &'a str,
    selected_files: Vec<ReportFile>,
    checked_files: Vec<ReportFile>,
    loaded_dependencies: Vec<ReportFile>,
    excluded_inputs: &'a BTreeMap<PathBuf, Exclusion>,
    input_errors: &'a [InputError],
    unresolved_loads: Vec<UnresolvedLoad>,
    diagnostics: DiagnosticCounts,
    complete: bool,
}

struct Checker {
    analysis: Analysis,
    bazel_info: BazelInfo,
    files: indexmap::IndexSet<File>,
    exclusions: BTreeMap<PathBuf, Exclusion>,
    input_errors: Vec<InputError>,
    ignored_paths: IgnoredPaths,
    loader: Arc<DefaultFileLoader>,
    fetch_repo_receiver: crossbeam_channel::Receiver<Task>,
    progress: bool,
    load_scope: LoadScope,
}

/// CLI selection only: dependencies can still load excluded source paths.
#[derive(Default)]
struct IgnoredPaths {
    names: Vec<String>,
    paths: Vec<PathBuf>,
}

impl IgnoredPaths {
    fn new(workspace: &Path, patterns: &[String]) -> anyhow::Result<Self> {
        let workspace = starpls_common::absolute_path(workspace)?;
        let mut ignored = Self::default();
        for pattern in patterns {
            let path = Path::new(pattern);
            if path
                .file_name()
                .is_some_and(|name| name == pattern.as_str())
            {
                ignored.names.push(pattern.clone());
                continue;
            }
            anyhow::ensure!(
                !pattern.is_empty() && path.is_relative(),
                "ignore pattern {pattern:?} must be a basename or a workspace-relative path"
            );
            let path = starpls_common::absolute_path(&workspace.join(path))?;
            anyhow::ensure!(
                path.starts_with(&workspace),
                "ignore pattern {pattern:?} resolves outside workspace {}",
                workspace.display()
            );
            ignored.paths.push(path);
        }
        Ok(ignored)
    }

    /// Paths come from normalized CLI roots or the analysis file interner.
    fn contains(&self, path: &Path) -> bool {
        let Self { names, paths } = self;
        path.components()
            .any(|component| is_ignored_name(component.as_os_str(), names))
            || paths.iter().any(|ignored| path.starts_with(ignored))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LoadScope {
    Requested,
    Transitive,
}

struct CheckResult {
    loads: LoadGraph,
    diagnostics: Vec<(File, Vec<Diagnostic>)>,
}

#[derive(Default)]
struct LoadGraph {
    files: indexmap::IndexSet<File>,
    unresolved: indexmap::IndexMap<File, Vec<LoadDependency>>,
}

impl Checker {
    fn new(
        analysis: Analysis,
        bazel_info: BazelInfo,
        paths: Vec<String>,
        extensions: &[&str],
        loader: Arc<DefaultFileLoader>,
        fetch_repo_receiver: crossbeam_channel::Receiver<Task>,
        options: &CheckCommand,
    ) -> anyhow::Result<Self> {
        let ignored_paths = IgnoredPaths::new(&bazel_info.workspace, &options.ignore_patterns)?;
        let mut checker = Self {
            analysis,
            bazel_info,
            files: Default::default(),
            exclusions: Default::default(),
            input_errors: Vec::new(),
            ignored_paths,
            loader,
            fetch_repo_receiver,
            progress: options.progress,
            load_scope: if options.audit_loads {
                LoadScope::Transitive
            } else {
                LoadScope::Requested
            },
        };

        let interfaces = checker.analysis.type_interface_files();
        if paths.is_empty() && interfaces.is_empty() && options.source_overlay.is_empty() {
            checker.input_errors.push(InputError {
                path: checker.bazel_info.workspace.clone(),
                message: "no input paths or configured interfaces were selected".to_owned(),
            });
        }
        let snapshot = checker.analysis.snapshot();
        for file in interfaces {
            let path = snapshot.path(file);
            if checker.ignored_paths.contains(path) {
                checker
                    .exclusions
                    .insert(path.to_path_buf(), Exclusion::Ignored);
            } else {
                checker.files.insert(file);
            }
        }
        drop(snapshot);
        let paths = paths
            .iter()
            .map(|path| starpls_common::absolute_path(Path::new(path)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        // Validate all selected external repositories in one batch before
        // walking their directories or admitting cached source files.
        let fetches = checker.loader.fetch_source_repositories(
            paths
                .iter()
                .filter(|path| !checker.ignored_paths.contains(path))
                .map(PathBuf::as_path),
            |message| {
                if checker.progress {
                    eprintln!("{message}");
                }
            },
        )?;
        if !fetches.is_empty() {
            checker.analysis.invalidate_loads();
        }
        for SourceOverlay {
            logical,
            physical: _,
        } in &options.source_overlay
        {
            let logical = starpls_common::absolute_path(logical)?;
            if checker.ignored_paths.contains(&logical) {
                checker.exclusions.insert(logical, Exclusion::Ignored);
            } else {
                checker.load_file(&logical, true, extensions, options.bazel_only)?;
            }
        }
        for path in paths {
            if checker.ignored_paths.contains(&path) {
                checker.exclusions.insert(path, Exclusion::Ignored);
                continue;
            }
            if let Err(error) = checker.loader.ensure_repository_for_path(&path) {
                checker.input_errors.push(InputError {
                    path,
                    message: format!("{error:#}"),
                });
                continue;
            }
            if checker.analysis.document(&path).is_some() {
                checker.load_file(&path, true, extensions, options.bazel_only)?;
                continue;
            }
            let mut walk = WalkDir::new(&path).into_iter();
            while let Some(entry) = walk.next() {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        checker.input_errors.push(InputError {
                            path: error.path().unwrap_or(&path).to_path_buf(),
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
                if !document::visit_source_entry(&entry, &[])
                    || checker.ignored_paths.contains(entry.path())
                {
                    checker
                        .exclusions
                        .insert(entry.path().to_path_buf(), Exclusion::Ignored);
                    if entry.file_type().is_dir() {
                        walk.skip_current_dir();
                    }
                    continue;
                }
                if entry.depth() > 0
                    && entry.file_type().is_dir()
                    && document::is_repository_root(entry.path())
                {
                    checker
                        .exclusions
                        .insert(entry.path().to_path_buf(), Exclusion::NestedRepository);
                    walk.skip_current_dir();
                    continue;
                }
                if entry.file_type().is_file() || (entry.depth() == 0 && entry.path().is_file()) {
                    if let Err(error) = checker.load_file(
                        entry.path(),
                        entry.depth() == 0,
                        extensions,
                        options.bazel_only,
                    ) {
                        checker.input_errors.push(InputError {
                            path: entry.path().to_path_buf(),
                            message: format!("{error:#}"),
                        });
                    }
                } else if entry.depth() == 0 && !entry.file_type().is_dir() {
                    checker.input_errors.push(InputError {
                        path: entry.path().to_path_buf(),
                        message: "input is not a regular source file or directory".to_owned(),
                    });
                }
            }
        }

        Ok(checker)
    }

    fn load_file(
        &mut self,
        path: &Path,
        is_explicit: bool,
        extensions: &[&str],
        bazel_only: bool,
    ) -> anyhow::Result<()> {
        let path = starpls_common::absolute_path(path)?;

        let Some((dialect, api_context)) =
            document::source_kind(&self.bazel_info.workspace, &path, extensions)
        else {
            if is_explicit {
                if !bazel_only {
                    anyhow::bail!("unsupported Starlark source file {}", path.display());
                }
                self.exclusions.insert(path, Exclusion::UnsupportedFile);
            }
            return Ok(());
        };

        if bazel_only && dialect != Dialect::Bazel {
            self.exclusions.insert(path, Exclusion::NotBazel);
            return Ok(());
        }
        self.loader.ensure_repository_for_path(&path)?;

        let info = api_context.map(|api_context| FileInfo::Bazel {
            api_context,
            is_external: path.starts_with(&self.bazel_info.output_base),
        });

        let file = self.analysis.file(&path, dialect, info)?;
        self.files.insert(file);

        Ok(())
    }

    fn report_diagnostics_for_file(
        snapshot: &AnalysisSnapshot,
        diagnostics: &[Diagnostic],
        counts: &mut DiagnosticCounts,
    ) -> anyhow::Result<()> {
        for diagnostic in diagnostics {
            match diagnostic.severity() {
                Severity::Info => counts.infos += 1,
                Severity::Warning => counts.warnings += 1,
                Severity::Error => counts.errors += 1,
                Severity::Fatal => counts.errors += 1,
            }
        }
        let config = DisplayDiagnosticConfig::new("starpls").color(true);
        anstream::print!("{}", snapshot.render_diagnostics(diagnostics, &config)?);
        Ok(())
    }

    fn load_graph(&mut self, checked: impl IntoIterator<Item = File>) -> anyhow::Result<LoadGraph> {
        let mut graph = LoadGraph {
            files: self.files.clone(),
            unresolved: Default::default(),
        };
        graph.files.extend(checked);
        let interfaces = self.analysis.type_interface_pairs();
        let snapshot = self.analysis.snapshot();
        if self.load_scope == LoadScope::Transitive {
            for (source, _) in &interfaces {
                let path = snapshot.path(*source);
                if self.ignored_paths.contains(path) {
                    self.exclusions
                        .insert(path.to_path_buf(), Exclusion::Ignored);
                } else {
                    graph.files.insert(*source);
                }
            }
        }
        // Requests survive cached queries. Current roots and load statements
        // determine which of those requests still belong to this check.
        let mut requests: HashMap<File, HashSet<String>> = Default::default();
        for (file, module) in self.loader.recorded_loads() {
            requests.entry(file).or_default().insert(module);
        }
        let mut frontier: Vec<_> = graph.files.iter().copied().collect();
        let progress = self.progress && self.load_scope == LoadScope::Transitive;
        if progress {
            eprintln!("Discovering loads from {} source files", frontier.len());
        }
        let mut visited = 0;
        while let Some(file) = frontier.pop() {
            visited += 1;
            if progress && (visited == 1 || visited % 100 == 0) {
                eprintln!(
                    "Discovering loads: {visited} files visited, {} discovered; {}",
                    graph.files.len(),
                    snapshot.path(file).display()
                );
            }
            // Trusted interfaces and implementations can consult each other's
            // declarations without a load statement, including excluded files.
            for (source, interface) in &interfaces {
                let counterpart = if file == *source {
                    *interface
                } else if file == *interface {
                    *source
                } else {
                    continue;
                };
                if requests.contains_key(&counterpart) && graph.files.insert(counterpart) {
                    frontier.push(counterpart);
                }
            }
            let dependencies = match self.load_scope {
                LoadScope::Transitive => snapshot.load_dependencies(file)?,
                LoadScope::Requested => {
                    let Some(modules) = requests.get(&file) else {
                        continue;
                    };
                    let mut dependencies = Vec::new();
                    for (module, range) in snapshot.load_statement_locations(file)? {
                        if !modules.contains(module.as_ref()) {
                            continue;
                        }
                        let resolution = snapshot.resolve_load(file, &module)?;
                        dependencies.push(LoadDependency {
                            module,
                            range,
                            resolution,
                        });
                    }
                    dependencies
                }
            };
            let mut unresolved = Vec::new();
            for edge in dependencies {
                match &edge.resolution {
                    LoadResolution::Resolved(loaded) => {
                        if graph.files.insert(*loaded) {
                            frontier.push(*loaded);
                        }
                    }
                    LoadResolution::Pending => unresolved.push(edge),
                    LoadResolution::Failed(_) => unresolved.push(edge),
                }
            }
            if !unresolved.is_empty() {
                graph.unresolved.insert(file, unresolved);
            }
        }
        Ok(graph)
    }

    /// Finish native work after all snapshots have drained, then refresh files
    /// before retrying pending queries. Mapping queries can regenerate files too.
    fn resolve_pending_loads(&mut self) -> bool {
        let mut repositories = self.loader.pending_repository_mappings();
        let fetches: Vec<_> = self
            .fetch_repo_receiver
            .try_iter()
            .map(|task| {
                let Task::FetchExternalRepoRequest(FetchExternalRepoRequest { repo, revision: _ }) =
                    task
                else {
                    unreachable!("CLI loader only sends repository fetch requests");
                };
                repo
            })
            .filter(|repo| self.loader.begin_fetch(repo.clone()))
            .collect();
        if repositories.is_empty() && fetches.is_empty() {
            return false;
        }
        while !repositories.is_empty() {
            if self.progress {
                eprintln!(
                    "Resolving repository mappings: {} repositories",
                    repositories.len()
                );
            }
            self.loader.resolve_repository_mappings(&repositories);
            repositories = self.loader.pending_repository_mappings();
        }
        if !fetches.is_empty() {
            let results = self.loader.fetch_repositories(&fetches, |message| {
                if self.progress {
                    eprintln!("{message}");
                }
            });
            for document::RepositoryFetchResult { name: repo, result } in results {
                if let Err(error) = result {
                    eprintln!("Failed to fetch repository @@{repo}: {error:#}");
                }
            }
        }
        self.analysis.invalidate_loads();
        true
    }

    fn check_files(&mut self) -> anyhow::Result<CheckResult> {
        loop {
            let mut diagnostics = Vec::new();
            let snapshot = self.analysis.snapshot();
            for (index, file) in self.files.iter().copied().enumerate() {
                if self.progress && (index == 0 || (index + 1) % 100 == 0) {
                    eprintln!(
                        "Checking {}/{}: {}",
                        index + 1,
                        self.files.len(),
                        snapshot.path(file).display()
                    );
                }
                diagnostics.push((file, snapshot.diagnostics(file)?));
            }
            drop(snapshot);
            let graph = self.load_graph(diagnostics.iter().map(|(file, _)| *file))?;
            if !self.resolve_pending_loads() {
                return Ok(CheckResult {
                    loads: graph,
                    diagnostics,
                });
            }
        }
    }

    fn report_diagnostics(&mut self, report_path: Option<&Path>) -> anyhow::Result<()> {
        let CheckResult {
            loads: graph,
            diagnostics,
        } = self.check_files()?;
        let snapshot = self.analysis.snapshot();
        let mut counts = DiagnosticCounts::default();
        let mut checked = indexmap::IndexSet::new();
        for InputError { path, message } in &self.input_errors {
            eprintln!("Cannot select {}: {message}", path.display());
        }
        for (file, diagnostics) in diagnostics {
            Self::report_diagnostics_for_file(&snapshot, &diagnostics, &mut counts)?;
            checked.insert(file);
        }

        let report = self.coverage_report(&snapshot, &graph, &checked, counts)?;
        for load in &report.unresolved_loads {
            eprintln!(
                "Unresolved load in {}: {:?}: {}",
                load.source.path.display(),
                load.module,
                load.message
            );
        }
        if let Some(path) = report_path {
            use std::io::Write;
            let file = std::fs::File::create(path)
                .with_context(|| format!("cannot write coverage report {}", path.display()))?;
            let mut writer = std::io::BufWriter::new(file);
            serde_json::to_writer_pretty(&mut writer, &report)?;
            writeln!(writer)?;
            writer.flush()?;
        }
        anstream::println!(
            "Checked {} files; discovered {} dependencies; excluded {} inputs",
            report.checked_files.len(),
            report.loaded_dependencies.len(),
            report.excluded_inputs.len()
        );
        if !report.complete {
            anyhow::bail!(
                "coverage incomplete: {} input failures and {} unresolved loads",
                report.input_errors.len(),
                report.unresolved_loads.len()
            );
        }
        if report.diagnostics.errors > 0 {
            anyhow::bail!(
                "failed with {} errors and {} warnings",
                report.diagnostics.errors,
                report.diagnostics.warnings
            );
        }
        if report.diagnostics.warnings > 0 {
            anstream::println!(
                "{}",
                Renderer::styled().render(Level::Warning.title(&format!(
                    "passed with {} warnings",
                    report.diagnostics.warnings
                )))
            );
        }
        Ok(())
    }

    fn report_file(&self, snapshot: &AnalysisSnapshot, file: File) -> anyhow::Result<ReportFile> {
        let path = snapshot.path(file).to_path_buf();
        let repository = match file.dialect {
            Dialect::Standard => None,
            Dialect::Bazel => self
                .loader
                .repository_for_path(&path)?
                .map(|repository| repository.name),
        };
        Ok(ReportFile { path, repository })
    }

    fn coverage_report<'a>(
        &'a self,
        snapshot: &AnalysisSnapshot,
        graph: &LoadGraph,
        checked: &indexmap::IndexSet<File>,
        diagnostics: DiagnosticCounts,
    ) -> anyhow::Result<CoverageReport<'a>> {
        let mut unresolved_loads = Vec::new();
        for (file, edges) in &graph.unresolved {
            for LoadDependency {
                module,
                range,
                resolution,
            } in edges
            {
                let message = match resolution {
                    LoadResolution::Resolved(_) => {
                        unreachable!("resolved loads have no coverage failure")
                    }
                    LoadResolution::Pending => "load resolution is still pending".to_owned(),
                    LoadResolution::Failed(error) => error.clone(),
                };
                unresolved_loads.push(UnresolvedLoad {
                    source: self.report_file(snapshot, *file)?,
                    module: module.clone(),
                    start: range.start().to_u32(),
                    end: range.end().to_u32(),
                    message,
                });
            }
        }
        Ok(CoverageReport {
            version: 2,
            load_scope: self.load_scope,
            workspace: &self.bazel_info.workspace,
            bazel_release: &self.bazel_info.release,
            selected_files: self
                .files
                .iter()
                .map(|file| self.report_file(snapshot, *file))
                .collect::<anyhow::Result<_>>()?,
            checked_files: checked
                .iter()
                .map(|file| self.report_file(snapshot, *file))
                .collect::<anyhow::Result<_>>()?,
            loaded_dependencies: graph
                .files
                .difference(checked)
                .filter(|file| !self.files.contains(*file))
                .map(|file| self.report_file(snapshot, *file))
                .collect::<anyhow::Result<_>>()?,
            excluded_inputs: &self.exclusions,
            input_errors: &self.input_errors,
            diagnostics,
            complete: self.input_errors.is_empty()
                && unresolved_loads.is_empty()
                && self.files.is_subset(checked),
            unresolved_loads,
        })
    }
}
