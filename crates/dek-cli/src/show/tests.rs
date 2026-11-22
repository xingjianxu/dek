use std::fs;

use dek_controller::eval::{TargetAnswer, TargetSnapshotSource};
use dek_core::{
    Observation,
    protocol::{TargetQuery, TargetQueryKind},
};
use serde_json::{Value, json};

use super::*;

struct Snapshot {
    source: TargetSnapshotSource,
    calls: usize,
}

impl TargetProvider for Snapshot {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        assert_eq!(query.kind, TargetQueryKind::Fact);
        assert_eq!(query.key, "architecture");
        self.calls += 1;
        Ok(TargetAnswer {
            value: json!("aarch64"),
            observation: Observation::Digest("arch-fixture".into()),
        })
    }

    fn snapshot_source(&self) -> Option<&TargetSnapshotSource> {
        Some(&self.source)
    }
}

#[test]
fn labels_only_the_snapshot_used_by_target_queries() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("dek.json"),
        r#"{"schemaVersion":1,"hosts":{"server":{"entry":"server.rb","target":{"ssh":"lab-server"}}}}"#,
    ).unwrap();
    let entry = directory.path().join("server.rb");
    let repo = Repository::load(directory.path()).unwrap();
    let mut provider = Snapshot {
        source: TargetSnapshotSource {
            label: "lab snapshot 2026-11-23".into(),
        },
        calls: 0,
    };
    let mut output = Vec::new();

    fs::write(&entry, "configure 'programs.git.enable', true").unwrap();
    show(
        &mut output,
        &repo,
        HostSelector::Named("server"),
        None,
        true,
        &mut provider,
    )
    .unwrap();
    let without_facts: Value = serde_json::from_slice(&output).unwrap();
    assert!(without_facts.get("targetSnapshot").is_none());
    assert_eq!(provider.calls, 0);

    fs::write(
        &entry,
        "configure 'programs.git.config', { 'target' => { 'architecture' => target.fact('architecture') } }",
    )
    .unwrap();
    let option = "programs.git.config".parse().unwrap();
    output.clear();
    show(
        &mut output,
        &repo,
        HostSelector::Named("server"),
        Some(&option),
        true,
        &mut provider,
    )
    .unwrap();
    let with_facts: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        with_facts["options"]["programs.git.config"]["value"]["target"]["architecture"],
        "aarch64"
    );
    assert_eq!(
        with_facts["targetSnapshot"]["label"],
        "lab snapshot 2026-11-23"
    );
    assert_eq!(provider.calls, 1);

    output.clear();
    show(
        &mut output,
        &repo,
        HostSelector::Named("server"),
        Some(&option),
        false,
        &mut provider,
    )
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(
        text.contains("aarch64") && text.contains("lab snapshot 2026-11-23"),
        "{text}"
    );
    assert_eq!(provider.calls, 2);
}
