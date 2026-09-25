use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use clap::Args;
use starpls_common::FileInfo;
use starpls_ide::Analysis;

use super::stub_package::Registration;

#[derive(Args, Clone, Default)]
pub(crate) struct TypeInterfaceOptions {
    /// Apply INTERFACE to SOURCE exports or BUILD variable bindings; repeat for more files.
    #[clap(long = "type_interface", value_name = "SOURCE=INTERFACE")]
    mappings: Vec<TypeInterfaceMapping>,
}

#[derive(Clone, Debug)]
struct TypeInterfaceMapping {
    source: PathBuf,
    interface: PathBuf,
}

impl FromStr for TypeInterfaceMapping {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((source, interface)) = value.split_once('=') else {
            return Err("expected SOURCE=INTERFACE".to_owned());
        };
        if source.is_empty() || interface.is_empty() {
            return Err("SOURCE and INTERFACE must both be nonempty paths".to_owned());
        }
        Ok(Self {
            source: source.into(),
            interface: interface.into(),
        })
    }
}

impl TypeInterfaceOptions {
    pub(crate) fn is_configured(&self) -> bool {
        !self.mappings.is_empty()
    }
    pub(crate) fn prepare(
        &self,
        loader: &crate::document::DefaultFileLoader,
        workspace: &Path,
    ) -> anyhow::Result<PreparedInterfaces> {
        let mut registrations = super::stub_package::load(loader, workspace)?;
        let resolve = |path: &Path| -> anyhow::Result<PathBuf> {
            let path = workspace.join(path);
            match loader.repository_for_path(&path)? {
                Some(repository) => loader.register_path(&path, &repository),
                None => starpls_common::absolute_path(&path),
            }
        };
        for TypeInterfaceMapping { source, interface } in &self.mappings {
            let origin = format!(
                "--type_interface {}={}",
                source.display(),
                interface.display()
            );
            let source = resolve(source).with_context(|| {
                format!("cannot resolve type interface source {}", source.display())
            })?;
            let interface = resolve(interface).with_context(|| {
                format!("cannot resolve type interface {}", interface.display())
            })?;
            registrations.push(Registration {
                source,
                interface,
                origin,
            });
        }
        let mut origins = HashMap::new();
        for Registration {
            source,
            interface: _,
            origin,
        } in &registrations
        {
            match origins.entry(source.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(origin);
                }
                Entry::Occupied(entry) => {
                    anyhow::bail!(
                        "duplicate type interface for {}: {} and {}",
                        entry.key().display(),
                        entry.get(),
                        origin
                    );
                }
            }
        }
        Ok(PreparedInterfaces { registrations })
    }
}

#[derive(Debug)]
pub(crate) struct PreparedInterfaces {
    registrations: Vec<Registration>,
}

impl PreparedInterfaces {
    pub(crate) fn install(self, analysis: &mut Analysis, workspace: &Path) -> anyhow::Result<()> {
        let Self { registrations } = self;
        let mut mappings = Vec::with_capacity(registrations.len());
        for Registration {
            source,
            interface,
            origin: _,
        } in registrations
        {
            let open = |path: &Path| {
                let (dialect, context) =
                    crate::document::dialect_and_api_context_for_workspace_path(workspace, path)
                        .with_context(|| {
                            format!("cannot classify type interface path {}", path.display())
                        })?;
                analysis.file(
                    path,
                    dialect,
                    context.map(|api_context| FileInfo::Bazel {
                        api_context,
                        is_external: !path.starts_with(workspace),
                    }),
                )
            };
            let source = open(&source)?;
            let interface = open(&interface)?;
            mappings.push((source, interface));
        }
        analysis.set_type_interfaces(mappings)
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[test]
    fn check_and_server_share_repeatable_mapping_arguments() {
        for command in ["check", "server"] {
            let parsed = crate::Cli::try_parse_from([
                "starpls",
                command,
                "--type_interface",
                "source.bzl=one.bzli",
                "--type_interface",
                "other.bzl=two.bzli",
            ])
            .unwrap();
            let options = match parsed.command.unwrap() {
                crate::Commands::Check(command) => command.type_interfaces,
                crate::Commands::Server(command) => command.type_interfaces,
                crate::Commands::Version => panic!("expected checking command"),
            };
            assert_eq!(options.mappings.len(), 2);
            for bad in ["missing-separator", "=empty.bzli", "empty.bzl="] {
                assert!(
                    crate::Cli::try_parse_from(["starpls", command, "--type_interface", bad])
                        .is_err()
                );
            }
        }
    }
    #[test]
    fn build_mapping_registration_preserves_host_context() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("build-annotations");
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("BUILD.bazel");
        let stub = root.join("BUILD.bzli");
        std::fs::write(&source, "_VALUE = 1\n").unwrap();
        std::fs::write(&stub, "_VALUE: str\n").unwrap();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = std::sync::Arc::new(crate::document::DefaultFileLoader::new(
            std::sync::Arc::new(starpls_bazel::client::BazelCLI::default()),
            root.clone(),
            None,
            root.join("external"),
            sender,
            false,
        ));
        let mut analysis = starpls_ide::Analysis::new(loader, Default::default()).unwrap();
        analysis
            .set_builtin_defs(crate::server::load_bazel_builtins(), Default::default())
            .unwrap();
        super::PreparedInterfaces {
            registrations: vec![super::Registration {
                source,
                interface: stub,
                origin: "test mapping".into(),
            }],
        }
        .install(&mut analysis, &root)
        .unwrap();
        let sources = analysis.type_interface_sources();
        let [source] = sources.as_slice() else {
            panic!("{sources:?}");
        };
        assert_eq!(source.api_context(), Some(starpls_bazel::APIContext::Build));
        let diagnostics = analysis.snapshot().diagnostics(*source).unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.id().as_str() == "invalid-assignment"),
            "{diagnostics:?}"
        );
        analysis.update_file(*source, "_VALUE: str = 'ok'\n".into());
        let diagnostics = analysis.snapshot().diagnostics(*source).unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.id().as_str() == "invalid-syntax"),
            "{diagnostics:?}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checked_in_with_cfg_package_matches_its_source_version() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("with-cfg-stubs");
        let workspace = root.join("workspace");
        let external = root.join("external");
        let source = external.join("with_cfg.bzl+");
        let stubs = external.join("with_cfg_stubs+");
        for directory in [&workspace, &source, &stubs] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(source.join("with_cfg.bzl"), "def with_cfg(kind): pass\n").unwrap();
        std::fs::create_dir_all(source.join("with_cfg/private")).unwrap();
        std::fs::write(source.join("with_cfg/private/with_cfg.bzl"), "").unwrap();
        std::fs::write(
            stubs.join("stubs.toml"),
            include_str!("../../../../stubs/with_cfg/stubs.toml"),
        )
        .unwrap();
        std::fs::write(
            stubs.join("with_cfg.bzli"),
            include_str!("../../../../stubs/with_cfg/with_cfg.bzli"),
        )
        .unwrap();
        std::fs::write(
            stubs.join("private_helpers.bzli"),
            include_str!("../../../../stubs/with_cfg/private_helpers.bzli"),
        )
        .unwrap();
        std::fs::write(
            workspace.join("starpls.toml"),
            "[[stub-packages]]\nmanifest = '@with_cfg_stubs//:stubs.toml'\n",
        )
        .unwrap();
        for version in ["0.14.6", "0.14.7"] {
            let mut client = crate::document::source_tests::TestBazelClient::default();
            client.repository_mappings.insert(
                "".into(),
                std::sync::Arc::new([("with_cfg_stubs".into(), "with_cfg_stubs+".into())].into()),
            );
            client.repository_mappings.insert(
                "with_cfg_stubs+".into(),
                std::sync::Arc::new([("with_cfg.bzl".into(), "with_cfg.bzl+".into())].into()),
            );
            client.selected_modules.insert(
                "with_cfg.bzl+".into(),
                starpls_bazel::client::SelectedModule {
                    name: "with_cfg.bzl".into(),
                    version: Some(version.into()),
                },
            );
            let (sender, _) = crossbeam_channel::unbounded();
            let loader = crate::document::DefaultFileLoader::new(
                std::sync::Arc::new(client),
                workspace.clone(),
                None,
                external.clone(),
                sender,
                true,
            );
            let prepared = super::TypeInterfaceOptions::default().prepare(&loader, &workspace);
            if version == "0.14.6" {
                let prepared = prepared.unwrap();
                let [first, second] = prepared.registrations.as_slice() else {
                    panic!("{prepared:?}");
                };
                let actual =
                    std::collections::BTreeMap::from([first, second].map(|registration| {
                        let super::Registration {
                            source,
                            interface,
                            origin: _,
                        } = registration;
                        (source.clone(), interface.clone())
                    }));
                let expected = std::collections::BTreeMap::from([
                    (source.join("with_cfg.bzl"), stubs.join("with_cfg.bzli")),
                    (
                        source.join("with_cfg/private/with_cfg.bzl"),
                        stubs.join("private_helpers.bzli"),
                    ),
                ]);
                assert_eq!(actual, expected);
            } else {
                let message = format!("{:#}", prepared.unwrap_err());
                assert!(
                    message.contains("selected version 0.14.7")
                        && message.contains("accepts 0.14.6"),
                    "{message}"
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn packages_compose_and_reject_conflicts_atomically() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("stub-packages");
        let workspace = root.join("workspace");
        let stubs = workspace.join("stubs");
        std::fs::create_dir_all(&stubs).unwrap();
        std::fs::create_dir_all(workspace.join("nested")).unwrap();
        for name in ["one", "two"] {
            std::fs::write(
                workspace.join(format!("{name}.bzl")),
                "def compute(): pass\n",
            )
            .unwrap();
            std::fs::write(
                stubs.join(format!("{name}.bzli")),
                "def compute(value: int): ...\n",
            )
            .unwrap();
        }
        std::fs::write(root.join("outside.bzl"), "").unwrap();
        std::fs::write(root.join("outside.bzli"), "").unwrap();
        let manifest = |source: &str, interface: &str| {
            format!(
            "format-version = 1\n[source]\nrepository = '@'\nmodule = 'local'\nversions = ['1']\n[files]\n'{source}' = '{interface}'\n"
        )
        };
        let package = |name: &str| {
            format!("[[stub-packages]]\nmanifest = 'stubs/{name}.toml'\nallow-unversioned = true\n")
        };
        let first = manifest("one.bzl", "one.bzli");
        let second = manifest("two.bzl", "two.bzli");
        std::fs::write(stubs.join("first.toml"), &first).unwrap();
        std::fs::write(stubs.join("second.toml"), &second).unwrap();
        let configuration = package("first") + &package("second");
        std::fs::write(workspace.join("starpls.toml"), &configuration).unwrap();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = std::sync::Arc::new(crate::document::DefaultFileLoader::new(
            std::sync::Arc::new(starpls_bazel::client::BazelCLI::default()),
            workspace.clone(),
            None,
            root.join("external"),
            sender,
            false,
        ));
        let mut analysis = starpls_ide::Analysis::new(loader.clone(), Default::default()).unwrap();
        super::TypeInterfaceOptions::default()
            .prepare(&loader, &workspace)
            .and_then(|prepared| prepared.install(&mut analysis, &workspace))
            .unwrap();
        let configured = analysis.type_interface_files();
        assert_eq!(configured.len(), 2);
        for (config, content, expected) in [
            (
                "unknown = true\n".to_owned(),
                first.clone(),
                "unknown field",
            ),
            (
                package("first") + "unknown = true\n",
                first.clone(),
                "unknown field",
            ),
            (
                package("first").replace("true", "false"),
                first.clone(),
                "no selected module version",
            ),
            (
                package("first"),
                first.replace("format-version = 1", "format-version = 2"),
                "format-version 2",
            ),
            (
                package("first"),
                first.replace("[source]", "unknown = true\n[source]"),
                "unknown field",
            ),
            (
                package("first"),
                first.replace("[files]", "unknown = true\n[files]"),
                "unknown field",
            ),
            (package("first"), first.replace("['1']", "[]"), "nonempty"),
            (
                package("first"),
                manifest("../outside.bzl", "one.bzli"),
                "outside repository",
            ),
            (
                package("first"),
                manifest("one.bzl", "../../outside.bzli"),
                "outside repository",
            ),
            (
                package("first"),
                manifest("one.bzl", "missing.bzli"),
                "cannot resolve",
            ),
            (
                package("first") + &package("first"),
                first.clone(),
                "duplicate type interface",
            ),
        ] {
            std::fs::write(workspace.join("starpls.toml"), config).unwrap();
            std::fs::write(stubs.join("first.toml"), content).unwrap();
            let error = super::TypeInterfaceOptions::default()
                .prepare(&loader, &workspace)
                .and_then(|prepared| prepared.install(&mut analysis, &workspace))
                .unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            assert_eq!(analysis.type_interface_files(), configured);
        }
        std::fs::write(stubs.join("first.toml"), &first).unwrap();
        std::fs::write(
            stubs.join("second.toml"),
            manifest("nested/../one.bzl", "two.bzli"),
        )
        .unwrap();
        std::fs::write(workspace.join("starpls.toml"), &configuration).unwrap();
        let error = super::TypeInterfaceOptions::default()
            .prepare(&loader, &workspace)
            .and_then(|prepared| prepared.install(&mut analysis, &workspace))
            .unwrap_err()
            .to_string();
        for expected in ["one.bzl", "first.toml", "second.toml"] {
            assert!(error.contains(expected), "{error}");
        }
        std::fs::write(workspace.join("starpls.toml"), package("first")).unwrap();
        let options = super::TypeInterfaceOptions {
            mappings: vec![super::TypeInterfaceMapping {
                source: "one.bzl".into(),
                interface: "stubs/two.bzli".into(),
            }],
        };
        let error = options
            .prepare(&loader, &workspace)
            .and_then(|prepared| prepared.install(&mut analysis, &workspace))
            .unwrap_err()
            .to_string();
        for expected in ["one.bzl", "first.toml", "--type_interface"] {
            assert!(error.contains(expected), "{error}");
        }
        assert_eq!(analysis.type_interface_files(), configured);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn installed_stub_packages_share_edits_without_sharing_contracts() {
        use starpls_bazel::APIContext;
        use starpls_common::Dialect;
        use starpls_common::FileInfo;

        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("installed-stubs");
        let workspace = root.join("workspace");
        let external = root.join("external");
        let backing = root.join("backing");
        for directory in [&workspace, &external, &backing] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(
            backing.join("defs.bzl"),
            "def identity(value): return value\n",
        )
        .unwrap();
        std::fs::write(
            backing.join("defs.bzli"),
            "def identity(value: int) -> int: ...\n",
        )
        .unwrap();
        std::fs::write(backing.join("stubs.toml"), "format-version = 1\n[source]\nrepository = '@@one+'\nmodule = 'local'\nversions = ['1']\n[files]\n'defs.bzl' = 'defs.bzli'\n").unwrap();
        std::os::unix::fs::symlink(&backing, external.join("stubs+")).unwrap();
        for name in ["one+", "two+"] {
            let directory = external.join(name);
            std::fs::create_dir_all(&directory).unwrap();
            std::os::unix::fs::symlink(backing.join("defs.bzl"), directory.join("defs.bzl"))
                .unwrap();
        }
        std::fs::write(
            workspace.join("starpls.toml"),
            "[[stub-packages]]\nmanifest = '@@stubs+//:stubs.toml'\nallow-unversioned = true\n",
        )
        .unwrap();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = std::sync::Arc::new(crate::document::DefaultFileLoader::new(
            std::sync::Arc::new(crate::document::source_tests::TestBazelClient::default()),
            workspace.clone(),
            None,
            external.clone(),
            sender,
            false,
        ));
        let mut analysis = starpls_ide::Analysis::new(loader.clone(), Default::default()).unwrap();
        super::TypeInterfaceOptions::default()
            .prepare(&loader, &workspace)
            .unwrap()
            .install(&mut analysis, &workspace)
            .unwrap();
        let info = Some(FileInfo::Bazel {
            api_context: APIContext::Bzl,
            is_external: false,
        });
        let caller = analysis.open_document(&workspace.join("caller.bzl"), Dialect::Bazel, info,
            "load('@@one+//:defs.bzl', one = 'identity')\nload('@@two+//:defs.bzl', two = 'identity')\none('bad')\ntwo('ok')\n".into(), 1).unwrap();
        let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let interfaces = analysis.type_interface_files();
        let [interface] = interfaces.as_slice() else {
            panic!("{interfaces:?}")
        };
        assert_eq!(
            analysis.snapshot().path(*interface),
            external.join("stubs+/defs.bzli")
        );
        analysis
            .open_document(
                &backing.join("defs.bzli"),
                Dialect::Bazel,
                info,
                "def identity(value: str) -> str: ...\n".into(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(caller).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let mut manager = crate::diagnostics::DiagnosticsManager::default();
        assert_eq!(
            manager.request(&snapshot, *interface).unwrap().0,
            *interface
        );
        drop(snapshot);
        analysis.close_document(&backing.join("defs.bzli")).unwrap();
        assert_eq!(analysis.snapshot().diagnostics(caller).unwrap().len(), 1);
        // A contract explicitly configured for the physical source has its own scope.
        let physical = analysis
            .file(&backing.join("defs.bzl"), Dialect::Bazel, info)
            .unwrap();
        analysis
            .set_type_interfaces([(physical, *interface)])
            .unwrap();
        let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mapping_installation_validates_all_files_before_replacing_configuration() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("type-interface-config");
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.bzl");
        let interface = root.join("source.bzli");
        std::fs::write(&source, "def compute(): pass\n").unwrap();
        std::fs::write(&interface, "def compute(value: int): ...\n").unwrap();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = std::sync::Arc::new(crate::document::DefaultFileLoader::new(
            std::sync::Arc::new(starpls_bazel::client::BazelCLI::default()),
            root.clone(),
            None,
            root.join("external"),
            sender,
            false,
        ));
        let mut analysis = starpls_ide::Analysis::new(loader.clone(), Default::default()).unwrap();
        let mapping = super::TypeInterfaceMapping {
            source: "source.bzl".into(),
            interface: "source.bzli".into(),
        };
        super::TypeInterfaceOptions {
            mappings: vec![mapping.clone()],
        }
        .prepare(&loader, &root)
        .and_then(|prepared| prepared.install(&mut analysis, &root))
        .unwrap();
        let configured = analysis.type_interface_files();
        assert_eq!(configured.len(), 1);
        for (mappings, expected) in [
            (
                vec![mapping.clone(), mapping.clone()],
                "duplicate type interface",
            ),
            (
                vec![super::TypeInterfaceMapping {
                    source: source.clone(),
                    interface: source.clone(),
                }],
                "must be a .bzli",
            ),
            (
                vec![super::TypeInterfaceMapping {
                    source: interface.clone(),
                    interface: interface.clone(),
                }],
                "source must be a .bzl",
            ),
            (
                vec![super::TypeInterfaceMapping {
                    source: source.clone(),
                    interface: root.join("missing.bzli"),
                }],
                "cannot open",
            ),
        ] {
            let error = super::TypeInterfaceOptions { mappings }
                .prepare(&loader, &root)
                .and_then(|prepared| prepared.install(&mut analysis, &root))
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(analysis.type_interface_files(), configured);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
