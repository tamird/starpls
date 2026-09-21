//! Exercise the shipped Skylib contracts through BUILD loads and IDE navigation.

use starpls_bazel::APIContext;
use starpls_common::Dialect;
use starpls_common::FileInfo;
use starpls_hir::Fixture;

use crate::Analysis;
use crate::FilePosition;
use crate::LocationLink;

const COPY_FILE: &str = include_str!("../../../../stubs/bazel_skylib/rules/copy_file.bzli");
const WRITE_FILE: &str = include_str!("../../../../stubs/bazel_skylib/rules/write_file.bzli");

#[test]
fn skylib_package_checks_build_calls_and_navigates_to_parameters() {
    let (mut analysis, loader) = Analysis::new_for_test();
    let mut fixture = Fixture::new(&mut analysis.db);
    let copy_source = fixture.add_file(&mut analysis.db, "@bazel_skylib//rules:copy_file.bzl", "");
    let write_source =
        fixture.add_file(&mut analysis.db, "@bazel_skylib//rules:write_file.bzl", "");
    let copy_stub = fixture.add_file(&mut analysis.db, "copy_file.bzli", COPY_FILE);
    let write_stub = fixture.add_file(&mut analysis.db, "write_file.bzli", WRITE_FILE);
    let source = r#"load("@bazel_skylib//rules:copy_file.bzl", "copy_file")
load("@bazel_skylib//rules:write_file.bzl", "write_file")

copy_file(name = "copy", src = "input.txt", out = "copy.txt", allow_symlink = None)
copy_file(name = "label", src = Label("//:input.txt"), out = "label.txt", is_executable = True, allow_symlink = False, visibility = ["//visibility:public"])
write_file(name = "empty", out = "empty.txt")
write_file(name = "list", out = "list.txt", content = ["first", "second"], newline = "unix")
write_file(name = "tuple", out = "tuple.txt", content = ("first", "second"), is_executable = True)
"#;
    let caller = fixture.add_file_with_options(
        &mut analysis.db,
        "BUILD.bazel",
        source,
        Dialect::Bazel,
        Some(FileInfo::Bazel {
            api_context: APIContext::Build,
            is_external: false,
        }),
    );
    loader.add_files_from_fixture(&fixture);
    analysis
        .set_builtin_defs(
            starpls_bazel::decode_builtins(include_bytes!(
                "../../../starpls/src/builtin/builtin.pb"
            ))
            .unwrap(),
            starpls_bazel::Builtins::default(),
        )
        .unwrap();
    analysis
        .set_type_interfaces([(copy_source, copy_stub), (write_source, write_stub)])
        .unwrap();
    {
        let snapshot = analysis.snapshot();
        for file in [copy_stub, write_stub, caller] {
            let diagnostics = snapshot.diagnostics(file).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }
        for (parameter, expected, contents) in [
            ("allow_symlink", copy_stub, COPY_FILE),
            ("content", write_stub, WRITE_FILE),
        ] {
            let locations = snapshot
                .goto_definition(
                    FilePosition {
                        file_id: caller,
                        pos: (source.rfind(parameter).unwrap() as u32).into(),
                    },
                    false,
                )
                .unwrap()
                .unwrap();
            let [LocationLink::Local {
                target_file_id,
                origin_selection_range: _,
                target_range: _,
                target_selection_range,
            }] = locations.as_slice()
            else {
                panic!("{locations:?}")
            };
            assert_eq!(*target_file_id, expected.source);
            assert_eq!(
                &contents[usize::from(target_selection_range.start())
                    ..usize::from(target_selection_range.end())],
                parameter,
            );
        }
    }
    for invalid_call in [
        "copy_file(name = 'bad', src = 42, out = 'out')",
        "copy_file(name = 'bad', src = 'in', out = 'out', is_executable = 'yes')",
        "copy_file(name = 'bad', src = 'in', out = 'out', allow_symlink = 'yes')",
        "write_file(name = 'bad', out = 'out', content = [1])",
        "write_file(name = 'bad', out = 'out', is_executable = 'yes')",
    ] {
        analysis.update_file(caller, format!("{source}\n{invalid_call}\n"));
        let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("{invalid_call}: {diagnostics:?}")
        };
        assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
    }
}
