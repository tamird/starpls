use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use clap::Args;
use starpls_bazel::APIContext;
use starpls_common::Dialect;
use starpls_common::FileInfo;
use starpls_ide::Analysis;

use super::stub_package::Registration;

#[derive(Args, Default)]
pub(crate) struct TypeInterfaceOptions {
    /// Trust declarations in INTERFACE for exports of SOURCE; repeat for more files.
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
    pub(crate) fn install(
        &self,
        analysis: &mut Analysis,
        loader: &crate::document::DefaultFileLoader,
        workspace: &Path,
    ) -> anyhow::Result<()> {
        let mut registrations = super::stub_package::load(loader, workspace)?;
        let resolve = |path: &Path| -> anyhow::Result<PathBuf> {
            let path = workspace.join(path);
            match loader.repository_for_path(&path)? {
                Some(repository) => loader.register_path(&path, &repository),
                None => Ok(path.canonicalize()?),
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
        let mut mappings = Vec::with_capacity(registrations.len());
        for Registration {
            source,
            interface,
            origin,
        } in registrations
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
            let open = |path: &Path| {
                analysis.file(
                    path,
                    Dialect::Bazel,
                    Some(FileInfo::Bazel {
                        api_context: APIContext::Bzl,
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
            .install(&mut analysis, &loader, &workspace)
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
                .install(&mut analysis, &loader, &workspace)
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
            .install(&mut analysis, &loader, &workspace)
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
            .install(&mut analysis, &loader, &workspace)
            .unwrap_err()
            .to_string();
        for expected in ["one.bzl", "first.toml", "--type_interface"] {
            assert!(error.contains(expected), "{error}");
        }
        assert_eq!(analysis.type_interface_files(), configured);
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
        .install(&mut analysis, &loader, &root)
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
                "cannot resolve type interface",
            ),
        ] {
            let error = super::TypeInterfaceOptions { mappings }
                .install(&mut analysis, &loader, &root)
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(analysis.type_interface_files(), configured);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
