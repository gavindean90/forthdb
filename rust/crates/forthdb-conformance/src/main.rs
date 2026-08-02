use forthdb_conformance::load_fixture;
use serde_json::json;
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn default_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../conformance/v1/kernel_cases.json")
}

fn main() -> ExitCode {
    let fixture_path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_fixture_path);

    match load_fixture(&fixture_path) {
        Ok(fixture) => {
            let report = json!({
                "implementation": "rust",
                "scope": "conformance-fixture-parser",
                "schema_version": fixture.schema_version,
                "cases": fixture.case_count(),
                "steps": fixture.step_count(),
                "status": "parsed-and-validated"
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .expect("the fixed parser report must serialize")
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
