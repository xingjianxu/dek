#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use dek_controller::{
    cache::CacheStore,
    eval::{
        CachedCollector, DeclarationGraph, GraphError, HttpInputProvider, LanguageCollector,
        RubySession, TargetAnswer, TargetProvider, collect_graph, collection_semantic_digest,
    },
    repo::{HostSelector, Repository},
};
use dek_core::{InputKind, Observation, protocol::TargetQuery};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::{TempDir, tempdir};

const ENV_ROOT: &str = "DEK_TEST_CACHED_ENV_ROOT";
const ENV_VALUE: &str = "DEK_TEST_CACHED_ENV_VALUE";

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new(hosts: Value, profiles: Value) -> Self {
        let fixture = Self {
            root: tempdir().unwrap(),
        };
        fixture.write(
            "dek.json",
            json!({"schemaVersion":1,"hosts":hosts,"profiles":profiles}).to_string(),
        );
        let ruby = Command::new("ruby")
            .args(["-rrbconfig", "-e", "print RbConfig.ruby"])
            .output()
            .unwrap();
        assert!(ruby.status.success());
        let ruby = String::from_utf8(ruby.stdout)
            .unwrap()
            .replace('\'', "'\\''");
        fixture.write(
            "ruby-launcher",
            format!("#!/bin/sh\nprintf x >> \"$0.starts\"\nexec '{ruby}' \"$@\"\n"),
        );
        fs::set_permissions(
            fixture.root.path().join("ruby-launcher"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fixture
    }

    fn write(&self, path: &str, content: impl AsRef<[u8]>) {
        let path = self.root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn starts(&self) -> usize {
        fs::read(self.root.path().join("ruby-launcher.starts")).map_or(0, |data| data.len())
    }

    fn collect(
        &self,
        host: &str,
        target: &mut dyn TargetProvider,
    ) -> Result<DeclarationGraph, GraphError> {
        collect(self.root.path(), host, target, None)
    }

    fn cache(&self) -> CacheStore {
        cache(self.root.path())
    }
}

fn cache(root: &Path) -> CacheStore {
    CacheStore::new(&root.join("cache"), root).unwrap()
}

fn collect(
    root: &Path,
    host: &str,
    target: &mut dyn TargetProvider,
    http: Option<&mut dyn HttpInputProvider>,
) -> Result<DeclarationGraph, GraphError> {
    let repo = Repository::load(root).unwrap();
    let store = cache(root);
    let mut ruby = RubySession::with_executable(root.join("ruby-launcher"));
    let mut collector = LanguageCollector::new(&mut ruby, target);
    if let Some(http) = http {
        collector = collector.with_http_provider(http);
    }
    collect_graph(
        &repo,
        HostSelector::Named(host),
        &mut CachedCollector::new(&store, collector),
    )
}

fn host(entry: &str, sdk: bool, target: &str) -> Value {
    let mut host = json!({"entry":entry,"target":{"ssh":target}});
    if sdk {
        host["inputs"] = json!("sdk");
    }
    host
}

fn sdk_profile(entry: &str) -> Value {
    json!({"entry":entry,"inputs":"sdk"})
}

#[derive(Default)]
struct Target {
    text: Option<&'static str>,
    reads: usize,
    alternating: bool,
}

fn observation(text: Option<&str>) -> Observation {
    text.map_or(Observation::Missing, |value| {
        Observation::Digest(format!("sha256:{:x}", Sha256::digest(value.as_bytes())))
    })
}

impl TargetProvider for Target {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        assert_eq!(query.key, "/etc/host-value");
        self.reads += 1;
        let text = if self.alternating && self.reads.is_multiple_of(2) {
            Some("changed mid-collection")
        } else {
            self.text
        };
        Ok(TargetAnswer {
            observation: observation(text),
            value: json!(text),
        })
    }
}

#[test]
fn warm_sdk_starts_no_ruby_and_native_io_remains_dynamic() {
    let fixture = Fixture::new(
        json!({"sdk":host("sdk.rb",true,"sdk-target"),"native":host("native.rb",false,"native-target")}),
        json!({}),
    );
    fixture.write(
        "sdk.rb",
        "configure 'example.value', read_text('sdk.txt')\n",
    );
    fixture.write("sdk.txt", "observed");
    fixture.write(
        "native.rb",
        "configure 'example.value', File.read(File.join(__dir__, 'native.txt'))\n",
    );
    fixture.write("native.txt", "first");

    let cold = fixture.collect("sdk", &mut Target::default()).unwrap();
    let warm = fixture.collect("sdk", &mut Target::default()).unwrap();
    assert_eq!(cold.host.spec, warm.host.spec);
    assert_eq!(fixture.starts(), 1);
    let launcher = fixture.root.path().join("ruby-launcher");
    let mut changed = fs::read(&launcher).unwrap();
    changed.extend_from_slice(b"# interpreter wrapper updated\n");
    fs::write(launcher, changed).unwrap();
    fixture.collect("sdk", &mut Target::default()).unwrap();
    assert_eq!(fixture.starts(), 2);

    let before = fixture.collect("native", &mut Target::default()).unwrap();
    assert_eq!(before.host.spec.options[0].value, "first");
    assert!(before.host.cache_key("native", &before.target).is_none());
    fixture.write("native.txt", "second");
    let after = fixture.collect("native", &mut Target::default()).unwrap();
    assert_eq!(after.host.spec.options[0].value, "second");
    assert_eq!(fixture.starts(), 4);
}

#[test]
fn reobserves_loaded_code_and_directory_but_leaves_source_file_deferred() {
    let fixture = Fixture::new(json!({"server":host("entry.rb",true,"server")}), json!({}));
    fixture.write("entry.rb", "require_relative 'helper'\nconfigure 'example.code', CACHE_HELPER\nconfigure 'example.members', read_dir('members')\nservice :caddy, configFile: source_file('Caddyfile')\n");
    fixture.write("helper.rb", "CACHE_HELPER = 'old'\n");
    fixture.write("members/βeta", "");
    fixture.write("members/alpha", "");
    fixture.write("Caddyfile", "first");
    let cold = fixture.collect("server", &mut Target::default()).unwrap();
    assert_eq!(cold.host.spec.options[1].value, json!(["alpha", "βeta"]));

    fixture.write("Caddyfile", "source file only");
    fixture.collect("server", &mut Target::default()).unwrap();
    assert_eq!(fixture.starts(), 1);

    fs::rename(
        fixture.root.path().join("members/alpha"),
        fixture.root.path().join("members/new\nmember"),
    )
    .unwrap();
    let changed = fixture.collect("server", &mut Target::default()).unwrap();
    assert_eq!(
        changed.host.spec.options[1].value,
        json!(["new\nmember", "βeta"])
    );
    fixture.collect("server", &mut Target::default()).unwrap();
    assert_eq!(fixture.starts(), 2);

    fixture.write("helper.rb", "CACHE_HELPER = 'new'\n");
    let changed = fixture.collect("server", &mut Target::default()).unwrap();
    assert_eq!(changed.host.spec.options[0].value, "new");
    assert_eq!(fixture.starts(), 3);
}

#[test]
fn existing_require_candidate_is_replaced_when_an_earlier_source_appears() {
    let fixture = Fixture::new(json!({"host":host("entry.rb",true,"host")}), json!({}));
    fs::create_dir(fixture.root.path().join("earlier")).unwrap();
    fixture.write(
        "later/cache_shadow_helper.rb",
        "CACHE_SHADOW_VALUE = 'later'\n",
    );
    fixture.write("entry.rb", "      $LOAD_PATH.unshift(File.join(__dir__, 'earlier'), File.join(__dir__, 'later'))\n      require 'cache_shadow_helper'\n      configure 'example.shadow', CACHE_SHADOW_VALUE\n");
    let first = fixture.collect("host", &mut Target::default()).unwrap();
    assert_eq!(first.host.spec.options[0].value, "later");
    assert!(first.host.spec.inputs.iter().any(|input| {
        input.kind == InputKind::File
            && input.id == "earlier/cache_shadow_helper.rb"
            && input.observation == Observation::Missing
    }));
    fixture.collect("host", &mut Target::default()).unwrap();
    assert_eq!(fixture.starts(), 1);
    fixture.write(
        "earlier/cache_shadow_helper.rb",
        "CACHE_SHADOW_VALUE = 'earlier'\n",
    );
    let changed = fixture.collect("host", &mut Target::default()).unwrap();
    assert_eq!(changed.host.spec.options[0].value, "earlier");
    assert_eq!(fixture.starts(), 2);
}

#[test]
fn finds_new_target_scope_and_shares_target_independent_profiles() {
    let fixture = Fixture::new(
        json!({"a":host("a.json",false,"a-ssh"),"b":host("b.json",false,"b-ssh")}),
        json!({"toggle":sdk_profile("toggle.rb"),"shared":sdk_profile("shared.rb")}),
    );
    for name in ["a.json", "b.json"] {
        fixture.write(
            name,
            r#"{"schemaVersion":1,"imports":["toggle","shared"],"options":[]}"#,
        );
    }
    fixture.write("shared.rb", "configure 'example.shared', 'shared'\n");
    fixture.write("toggle.rb", "        use_target = read_text('toggle') == 'on'\n        configure 'example.target', use_target ? target.read_text('/etc/host-value') : 'global'\n");
    let mut a = Target::default();
    let mut b = Target {
        text: Some("only b"),
        ..Target::default()
    };

    let first = fixture.collect("a", &mut a).unwrap();
    let second = fixture.collect("b", &mut b).unwrap();
    assert_eq!(first.profiles[0].spec, second.profiles[0].spec);
    assert_eq!(fixture.starts(), 1);
    assert_eq!(a.reads + b.reads, 0);

    fixture.write("toggle", "on");
    let missing = fixture.collect("a", &mut a).unwrap();
    assert!(missing.profiles[0].spec.options[0].value.is_null());
    let other = fixture.collect("b", &mut b).unwrap();
    assert_eq!(other.profiles[0].spec.options[0].value, "only b");
    let a_key = missing.profiles[0].cache_key("a", &missing.target).unwrap();
    let b_key = other.profiles[0].cache_key("b", &other.target).unwrap();
    assert_ne!(a_key, b_key);
    assert!(a_key.target.is_some());
    assert!(
        other.profiles[1]
            .cache_key("b", &other.target)
            .unwrap()
            .target
            .is_none()
    );
    fixture.collect("b", &mut b).unwrap();
    assert_eq!(fixture.starts(), 3);

    a.text = Some("now a");
    let now = fixture.collect("a", &mut a).unwrap();
    assert_eq!(now.profiles[0].spec.options[0].value, "now a");
    assert_eq!(fixture.starts(), 4);
    let reads = a.reads + b.reads;
    fixture.write("toggle", "off");
    fixture.collect("a", &mut a).unwrap();
    fixture.collect("b", &mut b).unwrap();
    assert_eq!(fixture.starts(), 5);
    assert_eq!(a.reads + b.reads, reads);
}

#[test]
fn syntax_failure_and_stale_inputs_preserve_last_successful_record() {
    let fixture = Fixture::new(json!({"host":host("entry.rb",true,"host")}), json!({}));
    let valid = "configure 'example.target', target.read_text('/etc/host-value')\n";
    fixture.write("entry.rb", valid);
    let mut target = Target {
        text: Some("first"),
        ..Target::default()
    };
    let original = fixture.collect("host", &mut target).unwrap();
    let key = original.host.cache_key("host", &original.target).unwrap();
    let path = fixture.cache().record_path(&key);
    let saved = fs::read(&path).unwrap();

    fixture.write("entry.rb", "def (\n");
    let error = fixture
        .collect("host", &mut target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("entry.rb:1"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), saved);

    fixture.write("entry.rb", format!("\n{valid}"));
    target.reads = 0;
    target.alternating = true;
    let error = fixture
        .collect("host", &mut target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("changed"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), saved);

    target.alternating = false;
    let moved = fixture.collect("host", &mut target).unwrap();
    assert_eq!(
        collection_semantic_digest(&original.host.spec),
        collection_semantic_digest(&moved.host.spec)
    );
    assert_ne!(
        original.host.spec.options[0].origin,
        moved.host.spec.options[0].origin
    );
}

struct Http(Option<&'static str>);

impl HttpInputProvider for Http {
    fn read(&mut self, url: &str) -> Result<Option<String>, String> {
        assert_eq!(url, "https://example.test/value");
        Ok(self.0.map(str::to_owned))
    }
}

#[test]
fn http_cache_uses_the_same_provider_for_collection_and_revalidation() {
    let fixture = Fixture::new(json!({"host":host("entry.rb",true,"host")}), json!({}));
    fixture.write(
        "entry.rb",
        "configure 'example.http', http_text('https://example.test/value')\n",
    );
    let evaluate = |text| {
        collect(
            fixture.root.path(),
            "host",
            &mut Target::default(),
            Some(&mut Http(text)),
        )
    };
    assert_eq!(
        evaluate(Some("old")).unwrap().host.spec.options[0].value,
        "old"
    );
    assert_eq!(
        evaluate(Some("old")).unwrap().host.spec.options[0].value,
        "old"
    );
    assert_eq!(fixture.starts(), 1);
    assert_eq!(
        evaluate(Some("new")).unwrap().host.spec.options[0].value,
        "new"
    );
    assert_eq!(fixture.starts(), 2);
    assert!(
        evaluate(None)
            .unwrap_err()
            .to_string()
            .contains("dek refresh")
    );
}

#[test]
fn environment_value_changes_across_runs() {
    let fixture = Fixture::new(json!({"host":host("entry.rb",true,"host")}), json!({}));
    fixture.write(
        "entry.rb",
        format!("configure 'example.env', env('{ENV_VALUE}')\n"),
    );
    let probe = |expected: Option<&str>| {
        let mut runner = Command::new(env::current_exe().unwrap());
        runner
            .args(["--exact", "environment_probe_child"])
            .env(ENV_ROOT, fixture.root.path());
        if let Some(value) = expected {
            runner.env(ENV_VALUE, value);
        } else {
            runner.env_remove(ENV_VALUE);
        }
        let result = runner.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    };
    probe(None);
    probe(None);
    assert_eq!(fixture.starts(), 1);
    probe(Some(""));
    probe(Some(""));
    assert_eq!(fixture.starts(), 2);
    probe(Some("different"));
    assert_eq!(fixture.starts(), 3);
}

#[test]
fn environment_probe_child() {
    if let Some(root) = env::var_os(ENV_ROOT) {
        let graph = collect(&PathBuf::from(root), "host", &mut Target::default(), None).unwrap();
        assert_eq!(
            graph.host.spec.options[0].value,
            json!(env::var(ENV_VALUE).ok())
        );
    }
}

#[test]
fn inconsistent_reads_of_the_same_input_are_never_published() {
    let fixture = Fixture::new(json!({"host":host("entry.rb",true,"host")}), json!({}));
    fixture.write(
        "entry.rb",
        "
        configure 'example.first', target.read_text('/etc/host-value')
        configure 'example.second', target.read_text('/etc/host-value')
    ",
    );
    let mut target = Target {
        text: Some("initial"),
        alternating: true,
        ..Target::default()
    };
    let error = fixture
        .collect("host", &mut target)
        .unwrap_err()
        .to_string();
    assert!(error.contains("changed"), "{error}");
}
