use alfred_core::OwnerRequest;
use alfred_planner::{plan_owner_request, rejection_report};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::{EXIT_ESCALATED, EXIT_OK, EXIT_USAGE};

enum PlanFailure {
    Rejected(String),
    Usage(String),
}

pub fn run(request_path: &Path, out_dir: &Path) -> ExitCode {
    match plan_from_file(request_path) {
        Ok(spec_json) => match write_dagspec(&spec_json, out_dir) {
            Ok(output_path) => {
                println!("{}", output_path.display());
                ExitCode::from(EXIT_OK)
            }
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(EXIT_USAGE)
            }
        },
        Err(failure) => {
            match &failure {
                PlanFailure::Rejected(report) => eprintln!("{report}"),
                PlanFailure::Usage(report) => eprintln!("{report}"),
            }
            ExitCode::from(match failure {
                PlanFailure::Rejected(_) => EXIT_ESCALATED,
                PlanFailure::Usage(_) => EXIT_USAGE,
            })
        }
    }
}

fn plan_from_file(request_path: &Path) -> Result<String, PlanFailure> {
    let request = read_owner_request(request_path)?;
    let spec = plan_owner_request(&request).map_err(|err| {
        let report = serde_json::to_string(&rejection_report(request_path, &err))
            .expect("rejection report serialization cannot fail");
        PlanFailure::Rejected(report)
    })?;
    serde_json::to_string_pretty(&spec)
        .map_err(|err| PlanFailure::Usage(format!("plan rejected: {err}")))
}

fn read_owner_request(request_path: &Path) -> Result<OwnerRequest, PlanFailure> {
    let raw = fs::read_to_string(request_path).map_err(|err| {
        PlanFailure::Usage(
            serde_json::to_string(&usage_report(request_path, &format!("cannot read request file: {err}")))
                .expect("report serialization cannot fail"),
        )
    })?;
    let raw_json: Value = serde_json::from_str(&raw).map_err(|err| {
        PlanFailure::Usage(
            serde_json::to_string(&usage_report(request_path, &format!("request file is not valid JSON: {err}")))
                .expect("report serialization cannot fail"),
        )
    })?;
    serde_json::from_value::<OwnerRequest>(raw_json.clone()).map_err(|err| {
        let mut report = serde_json::Map::new();
        report.insert("error".into(), "owner_request_rejected".into());
        report.insert("source_file".into(), request_path.display().to_string().into());
        report.insert(
            "reason".into(),
            format!("OwnerRequest schema rejected the input (missing or malformed field): {err}").into(),
        );
        report.insert("raw_json".into(), raw_json);
        PlanFailure::Rejected(
            serde_json::to_string(&Value::Object(report)).expect("report serialization cannot fail"),
        )
    })
}

fn usage_report(request_path: &Path, reason: &str) -> Value {
    let mut report = serde_json::Map::new();
    report.insert("error".into(), "owner_request_rejected".into());
    report.insert("source_file".into(), request_path.display().to_string().into());
    report.insert("reason".into(), reason.to_string().into());
    Value::Object(report)
}

fn write_dagspec(spec_json: &str, out_dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(out_dir)
        .map_err(|err| format!("cannot create output directory {}: {err}", out_dir.display()))?;
    let output_path = out_dir.join("dagspec.json");
    fs::write(&output_path, spec_json)
        .map_err(|err| format!("cannot write {}: {err}", output_path.display()))?;
    Ok(output_path)
}
