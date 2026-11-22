#![cfg(unix)]

use std::{fs, os::unix::fs::MetadataExt, path::Path};

use dek_controller::{
    cache::{CacheStore, UnitKey},
    eval::{
        CachedCollector, DeclarationGraph, LanguageCollector, RubyProviders, RubySession,
        TargetAnswer, TargetProvider, collect_graph,
    },
    modules::{
        ExpandedResources, ExpansionCache, ExpansionError, ModuleRegistry, RepositoryModules,
        SchemaRegistry, assemble_resources, merge_cache_key, register_app_modules,
        register_app_schemas, register_key_modules, register_key_schemas,
    },
    repo::{HostSelector, Repository},
};
use dek_core::{
    DesiredResource, FileState, HostConfig, Observation, Resource, ResourceId,
    protocol::TargetQuery,
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

struct Fixture {
    directory: TempDir,
    store: CacheStore,
}

struct NeverQuery;

impl TargetProvider for NeverQuery {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        panic!("unexpected target query {query:?}")
    }
}

struct Run {
    graph: DeclarationGraph,
    config: HostConfig,
    output: ExpandedResources,
}

impl Fixture {
    fn new(host: Value, profiles: Value, modules: Value) -> Self {
        let directory = tempdir().unwrap();
        let root = directory.path();
        let store = CacheStore::new(&root.join("cache"), root).unwrap();
        let fixture = Self { directory, store };
        fixture.write(
            "dek.json",
            json!({"schemaVersion":1,"hosts":{"host":host},"profiles":profiles,"modules":modules})
                .to_string(),
        );
        fixture
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn write(&self, relative: &str, contents: impl AsRef<[u8]>) {
        let path = self.root().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn collect(&self) -> Run {
        self.try_collect().unwrap()
    }

    fn try_collect(&self) -> Result<Run, ExpansionError> {
        self.try_collect_with(&mut NeverQuery)
    }

    fn try_collect_with(&self, target: &mut dyn TargetProvider) -> Result<Run, ExpansionError> {
        let repo = Repository::load(self.root()).unwrap();
        let mut schemas = SchemaRegistry::default();
        register_app_schemas(&mut schemas).unwrap();
        register_key_schemas(&mut schemas).unwrap();
        let custom = RepositoryModules::load(&repo, &mut schemas).unwrap();
        let mut ruby = RubySession::new();
        let graph = collect_graph(
            &repo,
            HostSelector::Named("host"),
            &mut CachedCollector::new(&self.store, LanguageCollector::new(&mut ruby, target)),
        )
        .unwrap();
        let config = schemas.merge_cached(&self.store, &graph).unwrap();

        let mut registry = ModuleRegistry::default();
        register_app_modules(&mut registry).unwrap();
        register_key_modules(&mut registry).unwrap();
        custom
            .register(
                &mut registry,
                &mut ruby,
                RubyProviders { target, http: None },
            )
            .unwrap();
        let drafts = registry.expand_cached(&ExpansionCache::new(&self.store, &graph), &config)?;
        let output = assemble_resources(repo.root(), &drafts)?;
        Ok(Run {
            graph,
            config,
            output,
        })
    }

    fn stamp(&self, key: &UnitKey) -> u64 {
        fs::metadata(self.store.record_path(key)).unwrap().ino()
    }

    fn module_stamp(&self, run: &Run, module: &str) -> u64 {
        self.stamp(&ExpansionCache::new(&self.store, &run.graph).key(module))
    }

    fn custom_module(&self, name: &str, script: &str) {
        self.write(&format!("{name}.json"), json!({"options":[{
            "path":format!("{name}.enable"),"type":"boolean","default":true,"description":"Test module"
        }]}).to_string());
        self.write(&format!("{name}.rb"), script);
    }

    fn calls(&self, name: &str) -> usize {
        fs::read(self.root().join(format!("{name}.calls")))
            .unwrap()
            .len()
    }
}

fn static_entry(imports: Value, options: Value) -> String {
    json!({"schemaVersion":1,"imports":imports,"options":options}).to_string()
}

fn git(branch: &str) -> Value {
    json!({"path":"programs.git.config","value":{"init":{"defaultBranch":branch}}})
}

fn find<'a>(result: &'a Run, path: &str) -> &'a Resource {
    result
        .output
        .resources
        .resources
        .iter()
        .find(|resource| resource.id() == ResourceId::Path(path.into()))
        .unwrap()
}

fn contents<'a>(result: &'a Run, path: &str) -> &'a [u8] {
    let DesiredResource::File(file) = &find(result, path).desired else {
        panic!("expected file")
    };
    let FileState::Present(content) = &file.state else {
        panic!("expected present file")
    };
    &result.output.contents[&content.digest]
}

#[test]
fn dynamic_profile_change_replaces_the_merge_dependency_graph() {
    let fixture = Fixture::new(
        json!({"entry":"host.rb","target":{"local":true}}),
        json!({"a":"a.json","b":"b.json"}),
        json!({}),
    );
    fixture.write("host.rb", "profile File.read(File.join(__dir__, 'selection')).strip\nconfigure 'programs.git.enable', true\n");
    fixture.write("selection", "a");
    fixture.write("a.json", static_entry(json!([]), json!([git("from-a")])));
    fixture.write("b.json", static_entry(json!([]), json!([git("from-b")])));
    let first = fixture.collect();
    let key = merge_cache_key(&first.graph);
    let prior = fixture.stamp(&key);
    assert!(
        std::str::from_utf8(contents(&first, "/etc/gitconfig"))
            .unwrap()
            .contains("from-a")
    );

    fixture.write("selection", "b");
    let second = fixture.collect();
    assert_eq!(second.graph.profiles[0].name, "b");
    assert!(
        std::str::from_utf8(contents(&second, "/etc/gitconfig"))
            .unwrap()
            .contains("from-b")
    );
    assert_ne!(fixture.stamp(&key), prior);
    let after_switch = fixture.stamp(&key);

    fixture.write("a.json", "intentionally invalid and unreachable");
    fixture.collect();
    assert_eq!(fixture.stamp(&key), after_switch);
}

#[test]
fn only_modules_with_changed_visible_options_reevaluate() {
    let fixture = Fixture::new(
        json!({"entry":"host.json","target":{"local":true}}),
        json!({"base":"base.json"}),
        json!({
            "lab.whole":{"schema":"lab.whole.json","expand":"lab.whole.rb","inputs":"sdk"},
            "lab.precise":{"schema":"lab.precise.json","expand":"lab.precise.rb","inputs":"sdk","readsOptions":["lab.precise"]}
        }),
    );
    fixture.write(
        "host.json",
        static_entry(
            json!(["base"]),
            json!([
                {"path":"programs.git.enable","value":true},
                {"path":"services.caddy.enable","value":true},
                {"path":"services.caddy.configFile","value":"Caddyfile"}
            ]),
        ),
    );
    fixture.write("Caddyfile", "initial and unchanged");
    fixture.write("base.json", static_entry(json!([]), json!([git("first")])));
    for name in ["lab.whole", "lab.precise"] {
        fixture.custom_module(name, &format!("File.open(File.join(__dir__, '{name}.calls'), 'a') {{ |file| file.write('x') }}\n{{resources: []}}\n"));
    }
    let first = fixture.collect();
    let original_git = fixture.module_stamp(&first, "programs.git");
    let original_caddy = fixture.module_stamp(&first, "services.caddy");
    assert_eq!(
        (fixture.calls("lab.whole"), fixture.calls("lab.precise")),
        (1, 1)
    );
    fixture.collect();
    assert_eq!(
        (fixture.calls("lab.whole"), fixture.calls("lab.precise")),
        (1, 1)
    );

    fixture.write("base.json", static_entry(json!([]), json!([git("second")])));
    let second = fixture.collect();
    assert_ne!(fixture.module_stamp(&second, "programs.git"), original_git);
    assert_eq!(
        fixture.module_stamp(&second, "services.caddy"),
        original_caddy
    );
    assert!(
        std::str::from_utf8(contents(&second, "/etc/gitconfig"))
            .unwrap()
            .contains("second")
    );
    assert_eq!(
        contents(&second, "/etc/caddy/Caddyfile"),
        b"initial and unchanged"
    );
    assert_eq!(
        (fixture.calls("lab.whole"), fixture.calls("lab.precise")),
        (2, 1)
    );
}

#[test]
fn refreshed_locations_reuse_merge_and_draft_and_static_files_are_read_at_assembly() {
    let fixture = Fixture::new(
        json!({"entry":"host.json","target":{"local":true}}),
        json!({"base":{"entry":"profiles/base.rb","inputs":"sdk"}}),
        json!({}),
    );
    fixture.write("host.json", static_entry(json!(["base"]), json!([])));
    let declarations = "service :caddy, enable: true, configFile: source_file('Caddyfile')\n";
    fixture.write("profiles/base.rb", declarations);
    fixture.write("profiles/Caddyfile", "old file");
    let first = fixture.collect();
    let merge_key = merge_cache_key(&first.graph);
    let merge = fixture.stamp(&merge_key);
    let caddy = fixture.module_stamp(&first, "services.caddy");
    assert_eq!(
        find(&first, "/etc/caddy/Caddyfile").origins[0]
            .origin
            .to_string(),
        "profiles/base.rb:1"
    );

    fixture.write("profiles/base.rb", format!("\n{declarations}"));
    let moved = fixture.collect();
    assert_eq!(fixture.stamp(&merge_key), merge);
    assert_eq!(fixture.module_stamp(&moved, "services.caddy"), caddy);
    let option = &moved.config.options[&"services.caddy.configFile".parse().unwrap()];
    assert_eq!(
        option.effective_definitions[0].origin.to_string(),
        "profiles/base.rb:2"
    );
    assert_eq!(
        find(&moved, "/etc/caddy/Caddyfile").origins[0]
            .origin
            .to_string(),
        "profiles/base.rb:2"
    );
    assert_eq!(contents(&moved, "/etc/caddy/Caddyfile"), b"old file");
    let base_key = moved.graph.profiles[0]
        .cache_key("host", &moved.graph.target)
        .unwrap();
    let collection = fixture.stamp(&base_key);

    fixture.write("profiles/Caddyfile", "new static bytes");
    let changed = fixture.collect();
    assert_eq!(fixture.stamp(&base_key), collection);
    assert_eq!(fixture.stamp(&merge_key), merge);
    assert_eq!(fixture.module_stamp(&changed, "services.caddy"), caddy);
    assert_eq!(
        contents(&changed, "/etc/caddy/Caddyfile"),
        b"new static bytes"
    );
    assert_ne!(
        find(&moved, "/etc/caddy/Caddyfile").desired,
        find(&changed, "/etc/caddy/Caddyfile").desired
    );
}

#[test]
fn only_opted_in_ruby_expansions_can_hit_and_their_inputs_are_reobserved() {
    let fixture = Fixture::new(
        json!({"entry":"host.json","target":{"local":true}}),
        json!({}),
        json!({
            "lab.sdk":{"schema":"lab.sdk.json","expand":"lab.sdk.rb","inputs":"sdk"},
            "lab.native":{"schema":"lab.native.json","expand":"lab.native.rb"}
        }),
    );
    fixture.write("host.json", static_entry(json!([]), json!([])));
    fixture.write("signal.txt", "first");
    fixture.custom_module("lab.sdk", "File.open(File.join(__dir__, 'lab.sdk.calls'), 'a') { |file| file.write('x') }\nfile_resource '/tmp/managed-fixture', content: read_text('signal.txt')\n");
    fixture.custom_module("lab.native", "File.open(File.join(__dir__, 'lab.native.calls'), 'a') { |file| file.write('x') }\n{ resources: [] }\n");
    let first = fixture.collect();
    assert_eq!(contents(&first, "/tmp/managed-fixture"), b"first");
    fixture.collect();
    assert_eq!(
        (fixture.calls("lab.sdk"), fixture.calls("lab.native")),
        (1, 2)
    );

    fixture.write("signal.txt", "second");
    let changed = fixture.collect();
    assert_eq!(contents(&changed, "/tmp/managed-fixture"), b"second");
    assert_eq!(
        (fixture.calls("lab.sdk"), fixture.calls("lab.native")),
        (2, 3)
    );
}

#[test]
fn changing_an_overridden_value_preserves_the_builtin_expansion() {
    let fixture = Fixture::new(
        json!({"entry":"host.json","target":{"local":true}}),
        json!({"base":"base.json"}),
        json!({}),
    );
    fixture.write(
        "host.json",
        static_entry(
            json!(["base"]),
            json!([{"path":"programs.git.enable","value":true}, git("host-value")]),
        ),
    );
    fixture.write("base.json", static_entry(json!([]), json!([git("first")])));
    let first = fixture.collect();
    let stamp = fixture.module_stamp(&first, "programs.git");
    fixture.write("base.json", static_entry(json!([]), json!([git("second")])));
    let second = fixture.collect();
    assert_eq!(
        contents(&first, "/etc/gitconfig"),
        contents(&second, "/etc/gitconfig")
    );
    assert_eq!(fixture.module_stamp(&second, "programs.git"), stamp);
    let config = &second.config.options[&"programs.git.config".parse().unwrap()];
    assert_eq!(
        config.overridden_definitions[0].value,
        json!({"init":{"defaultBranch":"second"}})
    );
}

#[test]
fn an_older_graph_cannot_replace_fresh_merge_or_expansion_records() {
    let fixture = Fixture::new(
        json!({"entry":"host.json","target":{"local":true}}),
        json!({}),
        json!({}),
    );
    let write = |branch| {
        fixture.write(
            "host.json",
            static_entry(
                json!([]),
                json!([{"path":"programs.git.enable","value":true}, git(branch)]),
            ),
        )
    };
    write("old");
    let old = fixture.collect();
    write("current");
    let current = fixture.collect();
    let key = merge_cache_key(&current.graph);
    let merge = fixture.stamp(&key);
    let git = fixture.module_stamp(&current, "programs.git");
    let mut schemas = SchemaRegistry::default();
    register_app_schemas(&mut schemas).unwrap();
    register_key_schemas(&mut schemas).unwrap();
    let mut modules = ModuleRegistry::default();
    register_app_modules(&mut modules).unwrap();

    assert!(schemas.merge_cached(&fixture.store, &old.graph).is_err());
    assert!(
        modules
            .expand_cached(
                &ExpansionCache::new(&fixture.store, &old.graph),
                &old.config
            )
            .is_err()
    );
    assert_eq!(fixture.stamp(&key), merge);
    assert_eq!(fixture.module_stamp(&current, "programs.git"), git);
}

#[test]
fn key_and_ciphertext_bytes_are_materialized_again_on_a_module_hit() {
    let fixture = Fixture::new(
        json!({"entry":"host.json","target":{"local":true}}),
        json!({}),
        json!({}),
    );
    fixture.write(
        "host.json",
        static_entry(
            json!([]),
            json!([
                {"path":"users.users.alice.openssh.authorizedKeys.keyFiles","value":["alice.pub"]},
                {"path":"age.identityPaths","value":["/etc/dek/identity"]},
                {"path":"age.secrets.deploy.file","value":"deploy.age"},
                {"path":"age.secrets.deploy.path","value":"/srv/deploy"}
            ]),
        ),
    );
    fixture.write("alice.pub", "first public bytes");
    fixture.write("deploy.age", "first ciphertext");
    let first = fixture.collect();
    let module = fixture.module_stamp(&first, "dek.keys");
    let key = ResourceId::AuthorizedKeys("alice".into());

    fixture.write("alice.pub", "second public bytes");
    fixture.write("deploy.age", "second ciphertext");
    let second = fixture.collect();
    assert_eq!(fixture.module_stamp(&second, "dek.keys"), module);
    let key_resource = second
        .output
        .resources
        .resources
        .iter()
        .find(|resource| resource.id() == key)
        .unwrap();
    let DesiredResource::AuthorizedKeys(keys) = &key_resource.desired else {
        panic!("expected an SSH key resource")
    };
    assert_eq!(
        second.output.contents[&keys.key_files[0]],
        b"second public bytes"
    );
    let DesiredResource::AgeSecret(secret) = &find(&second, "/srv/deploy").desired else {
        panic!("expected an age file")
    };
    assert_eq!(
        second.output.contents[&secret.ciphertext],
        b"second ciphertext"
    );
}

#[test]
fn edited_local_inputs_invalidate_a_collected_graph_without_another_publisher() {
    let fixture = Fixture::new(
        json!({"entry":"host.rb","inputs":"sdk","target":{"local":true}}),
        json!({}),
        json!({}),
    );
    fixture.write(
        "host.rb",
        "configure 'programs.git.enable', true\nconfigure 'programs.git.config', { init: { defaultBranch: read_text('branch') + read_dir('members').join } }\n",
    );
    fixture.write("branch", "old");
    fixture.write("members/first", "");
    let old = fixture.collect();
    let merge = fixture.stamp(&merge_cache_key(&old.graph));
    let git = fixture.module_stamp(&old, "programs.git");
    let mut schemas = SchemaRegistry::default();
    register_app_schemas(&mut schemas).unwrap();
    register_key_schemas(&mut schemas).unwrap();
    let mut modules = ModuleRegistry::default();
    register_app_modules(&mut modules).unwrap();

    let mut assert_stale = || {
        assert!(schemas.merge_cached(&fixture.store, &old.graph).is_err());
        assert!(
            modules
                .expand_cached(
                    &ExpansionCache::new(&fixture.store, &old.graph),
                    &old.config
                )
                .is_err()
        );
        assert_eq!(fixture.stamp(&merge_cache_key(&old.graph)), merge);
        assert_eq!(fixture.module_stamp(&old, "programs.git"), git);
    };
    fixture.write("branch", "new");
    assert_stale();
    fixture.write("branch", "old");
    fixture.write("members/second", "");
    assert_stale();
}

#[test]
fn an_invalid_inline_content_digest_preserves_the_last_good_draft() {
    let fixture = Fixture::new(
        json!({"entry":"host.json","target":{"local":true}}),
        json!({}),
        json!({
            "lab.sdk":{"schema":"lab.sdk.json","expand":"lab.sdk.rb","inputs":"sdk"}
        }),
    );
    fixture.write("host.json", static_entry(json!([]), json!([])));
    fixture.custom_module("lab.sdk", "{ resources: [] }\n");
    let valid = fixture.collect();
    let path = fixture
        .store
        .record_path(&ExpansionCache::new(&fixture.store, &valid.graph).key("lab.sdk"));
    let previous = fs::read(&path).unwrap();
    fixture.write(
        "lab.sdk.rb",
        "{resources: [], contents: {'sha256:wrong' => [1, 2, 3]}}\n",
    );
    let error = fixture
        .try_collect()
        .err()
        .expect("invalid content object")
        .to_string();
    assert!(
        error.contains("lab.sdk") && error.contains("sha256:wrong"),
        "{error}"
    );
    assert!(
        fs::read(&path).unwrap() == previous,
        "the last good draft was replaced"
    );
}

#[derive(Default)]
struct TargetDuringRecheck<'a> {
    on_recheck: Option<Box<dyn FnOnce() + 'a>>,
}

impl TargetProvider for TargetDuringRecheck<'_> {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        assert_eq!(query.key, "/etc/value");
        if query.parent_id == "cache-recheck"
            && let Some(change) = self.on_recheck.take()
        {
            change();
        }
        Ok(TargetAnswer {
            value: json!("target input"),
            observation: Observation::Digest("stable-provider-observation".into()),
        })
    }
}

#[test]
fn provider_rechecks_cannot_return_or_publish_drafts_from_an_older_graph() {
    let fixture = Fixture::new(
        json!({"entry":"host.json","target":{"local":true}}),
        json!({}),
        json!({
            "lab.sdk":{"schema":"lab.sdk.json","expand":"lab.sdk.rb","inputs":"sdk"}
        }),
    );
    let initial = static_entry(json!([]), json!([]));
    let changed = static_entry(
        json!([]),
        json!([{"path":"programs.git.enable","value":true}]),
    );
    fixture.write("host.json", &initial);
    let script =
        "file_resource '/tmp/rechecked-fixture', content: target.read_text('/etc/value')\n";
    fixture.custom_module("lab.sdk", script);
    let valid = fixture
        .try_collect_with(&mut TargetDuringRecheck::default())
        .unwrap();
    let key = ExpansionCache::new(&fixture.store, &valid.graph).key("lab.sdk");
    let stamp = fixture.stamp(&key);

    for cold_draft in [false, true] {
        fixture.write("host.json", &initial);
        if cold_draft {
            fixture.write("lab.sdk.rb", format!("# changed script bytes\n{script}"));
        }
        let mut provider = TargetDuringRecheck {
            on_recheck: Some(Box::new(|| fixture.write("host.json", &changed))),
        };
        let error = fixture
            .try_collect_with(&mut provider)
            .err()
            .expect("changed graph must fail")
            .to_string();
        assert!(error.contains("changed"), "{error}");
        assert!(provider.on_recheck.is_none());
        assert_eq!(fixture.stamp(&key), stamp);
    }
}
