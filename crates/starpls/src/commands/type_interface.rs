use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use clap::Args;
use starpls_bazel::APIContext;
use starpls_common::Dialect;
use starpls_common::FileInfo;
use starpls_ide::Analysis;

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
    pub(crate) fn install(&self, analysis: &mut Analysis, workspace: &Path) -> anyhow::Result<()> {
        let mut mappings = Vec::with_capacity(self.mappings.len());
        for TypeInterfaceMapping { source, interface } in &self.mappings {
            let source = workspace.join(source).canonicalize().with_context(|| {
                format!("cannot resolve type interface source {}", source.display())
            })?;
            let interface = workspace.join(interface).canonicalize().with_context(|| {
                format!("cannot resolve type interface {}", interface.display())
            })?;
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
    fn mapping_installation_validates_all_files_before_replacing_configuration() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("type-interface-config");
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.bzl");
        let interface = root.join("source.bzli");
        std::fs::write(&source, "def compute(): pass\n").unwrap();
        std::fs::write(&interface, "def compute(value: int): ...\n").unwrap();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = crate::document::DefaultFileLoader::new(
            std::sync::Arc::new(starpls_bazel::client::BazelCLI::default()),
            root.clone(),
            None,
            root.join("external"),
            sender,
            false,
        );
        let mut analysis =
            starpls_ide::Analysis::new(std::sync::Arc::new(loader), Default::default()).unwrap();
        let mapping = super::TypeInterfaceMapping {
            source: "source.bzl".into(),
            interface: "source.bzli".into(),
        };
        super::TypeInterfaceOptions {
            mappings: vec![mapping.clone()],
        }
        .install(&mut analysis, &root)
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
                .install(&mut analysis, &root)
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(analysis.type_interface_files(), configured);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
