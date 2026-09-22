use starpls_common::File;
use starpls_syntax::TextRange;
use ty_python_semantic::SemanticModel;

use crate::Database;
use crate::SemanticTokens;

pub(crate) fn semantic_tokens(
    db: &Database,
    file: File,
    range: Option<TextRange>,
) -> SemanticTokens {
    let model = SemanticModel::new(db, db.starlark_program_file(file));
    let range = range.map(|range| {
        ruff_text_size::TextRange::new(
            u32::from(range.start()).into(),
            u32::from(range.end()).into(),
        )
    });
    ty_ide::semantic_tokens_for_model(&model, range)
}

#[cfg(test)]
mod tests {
    use starpls_bazel::APIContext;
    use starpls_common::Dialect;
    use starpls_common::FileInfo;

    use crate::Analysis;
    use crate::SemanticTokenType;

    #[test]
    fn highlights_bazel_factories_and_skips_unadmitted_statements() {
        let (mut analysis, loader) = Analysis::new_for_test();
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                starpls_bazel::Builtins::default(),
            )
            .unwrap();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let source = "load(42, invalid.call())\nclass PythonOnly: pass\ndef impl(ctx):\n    return struct(value = ctx.attr.name)\nthing = rule(implementation = impl)\n";
        let file = fixture.add_file_with_options(
            &mut analysis.db,
            "defs.bzl",
            source,
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Bzl,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        let tokens = analysis.snapshot().semantic_tokens(file, None).unwrap();
        assert!(
            tokens
                .iter()
                .all(|token| { !["invalid", "PythonOnly"].contains(&&source[token.range]) }),
            "{tokens:?}"
        );
        for name in ["struct", "rule"] {
            assert!(
                tokens.iter().any(|token| {
                    &source[token.range] == name && token.token_type == SemanticTokenType::Variable
                }),
                "{name}: {tokens:?}"
            );
        }
    }

    #[test]
    fn highlights_loaded_functions_and_tracks_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(&mut analysis.db, "defs.bzl", "def greet(name: str): pass\n");
        let file = fixture.add_file(&mut analysis.db, "main.bzl", "");
        loader.add_files_from_fixture(&fixture);
        for (binding, suffix) in [
            ("\"greet\"", "greet(name)\n"),
            ("local=\"greet\"", "local(name\n"),
            ("local=\"greet\"", "local(name)\n"),
        ] {
            let source =
                format!("load(\"defs.bzl\", {binding})\ndef run(name: str):\n    {suffix}");
            analysis.update_file(file, source.clone());
            let tokens = analysis.snapshot().semantic_tokens(file, None).unwrap();
            let call = source.rfind(suffix).unwrap();
            let parameter = source.find("name:").unwrap();
            assert!(
                tokens.iter().any(|token| {
                    usize::from(token.range.start()) == call
                        && token.token_type == SemanticTokenType::Function
                }),
                "{tokens:?}"
            );
            assert!(
                tokens.iter().any(|token| {
                    usize::from(token.range.start()) == parameter
                        && token.token_type == SemanticTokenType::Parameter
                }),
                "{tokens:?}"
            );
            let range = starpls_syntax::TextRange::new(
                u32::try_from(call).unwrap().into(),
                u32::try_from(source.len()).unwrap().into(),
            );
            let expected: Vec<_> = tokens
                .iter()
                .filter(|token| usize::from(token.range.start()) >= call)
                .cloned()
                .collect();
            let ranged = analysis
                .snapshot()
                .semantic_tokens(file, Some(range))
                .unwrap();
            assert!(!ranged.is_empty());
            assert_eq!(&*ranged, expected.as_slice());
            assert!(tokens
                .iter()
                .all(|token| usize::from(token.range.start()) > source.find('\n').unwrap()));
        }
    }
}
