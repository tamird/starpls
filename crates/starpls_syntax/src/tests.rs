use std::env;
use std::error;
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

use expect_test::expect_file;
use expect_test::ExpectFile;
use runfiles::find_runfiles_dir;

fn check(input: &str, expected: ExpectFile) {
    let mut errors = Vec::new();
    let tree = crate::parse_module(input, &mut |error| errors.push(error));
    let syntax = tree.syntax();
    assert_eq!(syntax.text().to_string(), input);
    let mut buf = format!("{syntax:#?}");
    for error in errors {
        writeln!(buf, "error {:?}: {}", error.range, error.message).unwrap();
    }
    expected.assert_eq(&buf);
}

#[test]
fn test_parse_ok() {
    for test_case in collect_test_cases("test_data/ok").unwrap() {
        check(&test_case.input, expect_file![test_case.expect_file]);
    }
}

#[test]
fn test_parse_error() {
    for test_case in collect_test_cases("test_data/err").unwrap() {
        check(&test_case.input, expect_file![test_case.expect_file]);
    }
}

#[derive(Debug)]
struct TestCase {
    input: String,
    expect_file: PathBuf,
}

fn collect_test_cases(dir: &'static str) -> Result<Vec<TestCase>, Box<dyn error::Error>> {
    let mut test_cases = Vec::new();

    // Check for a test filter.
    let filter = env::var("TEST_FILTER").ok();

    let root = find_runfiles_dir()
        .map(|dir| dir.join("_main/crates/starpls_syntax"))
        .unwrap_or_else(|_| {
            PathBuf::from(
                env::var("CARGO_MANIFEST_DIR")
                    .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string()),
            )
        });

    // Determine the test data directory.
    let test_data_dir = root.join(dir);

    for entry in fs::read_dir(test_data_dir)? {
        let entry = entry?;
        let entry_path = entry.path();

        // Skip non-Starlark files.
        if entry_path.extension().unwrap_or_default() != "star" || {
            let file_type = entry.file_type()?;
            !(file_type.is_file() || file_type.is_symlink())
        } {
            continue;
        }

        // If a filter was specified, check if the test name (the base name of the file without the extension) matches it.
        let stripped = entry_path.with_extension("");
        let test_name = match stripped.file_name().and_then(|name| name.to_str()) {
            Some(test_name) => test_name,
            None => {
                continue;
            }
        };
        if let Some(ref filter) = filter {
            if !test_name.contains(filter) {
                continue;
            }
        }

        // For a Starlark source file `source.star`, the corresponding expect file is `source.rast`.
        let input = fs::read_to_string(&entry_path)?;
        let expect_file = stripped.with_extension("rast");

        test_cases.push(TestCase { input, expect_file })
    }

    Ok(test_cases)
}
