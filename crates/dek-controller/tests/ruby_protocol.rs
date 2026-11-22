use std::{fs, path::Path};

use dek_controller::eval::{RubySession, TargetAnswer, TargetProvider};
use dek_core::{
    InputKind, Observation,
    protocol::{EvalOperation, TargetQuery, TargetQueryKind},
};
use serde_json::{Value, json};

#[derive(Default)]
struct Target(Vec<TargetQuery>);

impl TargetProvider for Target {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        self.0.push(query.clone());
        match query.kind {
            TargetQueryKind::Fact => Ok(TargetAnswer {
                value: json!("aarch64"),
                observation: Observation::Digest("sha256:architecture".into()),
            }),
            TargetQueryKind::ReadText => Ok(TargetAnswer {
                value: Value::Null,
                observation: Observation::Missing,
            }),
            TargetQueryKind::Capability => Err("capability is unavailable".into()),
        }
    }
}

fn write(directory: &Path, name: &str, script: &str) -> std::path::PathBuf {
    let path = directory.join(name);
    fs::write(&path, script).unwrap();
    path
}

#[test]
fn reuses_one_process_and_separates_request_contexts() {
    let directory = tempfile::tempdir().unwrap();
    let collect = write(
        directory.path(),
        "collect.rb",
        r#"
warn "stderr fixture"
STDOUT.puts "stdout fixture"
raise "unexpected stdin" unless STDIN.eof?
previous = @count
@count = 1
environment = ENV["DEK_TEST_EVALUATOR_CONTEXT"]
directory = Dir.pwd
ENV["DEK_TEST_EVALUATOR_CONTEXT"] = context.fetch("name")
Dir.chdir(File.dirname(context.fetch("entry")))
[Process.pid, previous, operation, context.fetch("name"), environment, directory]
"#,
    );
    let expand = write(
        directory.path(),
        "expand.rb",
        r#"[Process.pid, @count, operation, context.fetch("name"), ENV["DEK_TEST_EVALUATOR_CONTEXT"], Dir.pwd]"#,
    );

    let mut ruby = RubySession::new();
    let mut target = Target::default();
    let first = ruby
        .evaluate(
            EvalOperation::Collect,
            &collect,
            json!({"name": "a", "entry": collect}),
            &mut target,
        )
        .unwrap();
    let second = ruby
        .evaluate(
            EvalOperation::Collect,
            &collect,
            json!({"name": "b", "entry": collect}),
            &mut target,
        )
        .unwrap();
    let third = ruby
        .evaluate(
            EvalOperation::Expand,
            &expand,
            json!({"name": "c"}),
            &mut target,
        )
        .unwrap();

    let directory = json!(std::env::current_dir().unwrap());
    let environment = json!(std::env::var("DEK_TEST_EVALUATOR_CONTEXT").ok());
    assert_eq!(first.result[0], second.result[0]);
    assert_eq!(second.result[0], third.result[0]);
    assert_ne!(first.id, second.id);
    assert_ne!(second.id, third.id);
    assert_eq!(
        &first.result.as_array().unwrap()[1..],
        &[
            Value::Null,
            json!("collect"),
            json!("a"),
            environment.clone(),
            directory.clone()
        ]
    );
    assert_eq!(
        &second.result.as_array().unwrap()[1..],
        &[
            Value::Null,
            json!("collect"),
            json!("b"),
            environment.clone(),
            directory.clone()
        ]
    );
    assert_eq!(
        &third.result.as_array().unwrap()[1..],
        &[
            Value::Null,
            json!("expand"),
            json!("c"),
            environment.clone(),
            directory.clone()
        ]
    );
    assert!(target.0.is_empty());
}

#[test]
fn dispatches_reverse_requests_with_origins_and_observations() {
    let directory = tempfile::tempdir().unwrap();
    let entry = write(
        directory.path(),
        "input.rb",
        "[target.fact('architecture'), target.read_text('/etc/dek/override')]\n",
    );
    let mut target = Target::default();
    let output = RubySession::new()
        .evaluate(EvalOperation::Collect, &entry, Value::Null, &mut target)
        .unwrap();

    assert_eq!(output.result, json!(["aarch64", null]));
    assert_eq!(target.0.len(), 2);
    assert_eq!(target.0[0].parent_id, output.id);
    assert_eq!(target.0[1].parent_id, output.id);
    assert_ne!(target.0[0].id, target.0[1].id);
    assert_eq!(output.inputs[0].kind, InputKind::TargetFact);
    assert_eq!(output.inputs[0].id, "architecture");
    assert_eq!(output.inputs[1].kind, InputKind::TargetFile);
    assert_eq!(output.inputs[1].observation, Observation::Missing);
    assert!(
        output.inputs[1]
            .origin
            .as_ref()
            .unwrap()
            .to_string()
            .ends_with("input.rb:1")
    );
}

#[test]
fn reports_ruby_syntax_runtime_target_and_exit_errors() {
    let directory = tempfile::tempdir().unwrap();
    let mut target = Target::default();

    for (name, script, expected) in [
        ("syntax.rb", "value = (\n", "syntax"),
        (
            "runtime.rb",
            "\nraise 'fixture failure'\n",
            "fixture failure",
        ),
        (
            "target.rb",
            "target.capability('systemd')\n",
            "capability is unavailable",
        ),
        ("exit.rb", "exit! 19\n", "19"),
    ] {
        let entry = write(directory.path(), name, script);
        let message = RubySession::new()
            .evaluate(EvalOperation::Collect, &entry, Value::Null, &mut target)
            .unwrap_err()
            .to_string();
        assert!(message.contains("collect"), "{message}");
        assert!(message.contains(name), "{message}");
        assert!(message.contains(expected), "{message}");
        if name == "syntax.rb" {
            assert!(message.contains("syntax.rb:1"), "{message}");
        }
        if name == "runtime.rb" {
            assert!(message.contains("runtime.rb:2"), "{message}");
        }
        if name == "target.rb" {
            assert!(message.contains("target.rb:1"), "{message}");
        }
    }
}

#[test]
fn reports_errors_in_required_ruby_files() {
    let directory = tempfile::tempdir().unwrap();
    let entry = write(directory.path(), "entry.rb", "require_relative 'helper'\n");
    let helper = write(directory.path(), "helper.rb", "\nraise 'fixture failure'\n");
    let message = RubySession::new()
        .evaluate(
            EvalOperation::Collect,
            &entry,
            Value::Null,
            &mut Target::default(),
        )
        .unwrap_err()
        .to_string();
    assert!(
        message.contains(&format!("{}:2", helper.display())),
        "{message}"
    );
}
