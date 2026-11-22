use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    process::Output,
    time::Instant,
};

use dek_controller::{
    cache::{CacheStore, UnitKey, UnitStage},
    repo::Target,
};
use dek_core::{Action, Priority, ServiceEffect};
use tempfile::{TempDir, tempdir};

use super::*;

const BASE: &str = "profiles/base.rb";
const CADDYFILE: &str = "profiles/Caddyfile";
const GIT_PATH: &str = "/etc/gitconfig";
const HOST_FIXTURE: &str = "DEK_TEST_INCREMENTAL_HOST";
const ROOT_FIXTURE: &str = "DEK_TEST_INCREMENTAL_ROOT";

struct Fixture {
    root: TempDir,
    cache: CacheStore,
    ruby: String,
}

struct Run {
    output: Value,
    starts: usize,
    published: BTreeSet<PathBuf>,
}

impl Run {
    fn actions(&self) -> Vec<Action> {
        serde_json::from_value(self.output["actions"].clone()).unwrap()
    }

    fn assert_warm(&self) {
        assert_eq!(self.starts, 0);
        assert!(self.published.is_empty(), "{:?}", self.published);
    }
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("repo")).unwrap();
        let cache = CacheStore::new(&root.path().join("cache"), &root.path().join("repo")).unwrap();
        let ruby = Command::new("ruby")
            .args(["-rrbconfig", "-e", "print RbConfig.ruby"])
            .output()
            .unwrap();
        assert!(ruby.status.success(), "{:?}", ruby.stderr);
        write(
            root.path(),
            "bin/ruby",
            "#!/bin/sh\nprintf x >> \"$0.starts\"\nexec \"$DEK_TEST_INCREMENTAL_RUBY\" \"$@\"\n",
        );
        fs::set_permissions(
            root.path().join("bin/ruby"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        Self {
            root,
            cache,
            ruby: String::from_utf8(ruby.stdout).unwrap(),
        }
    }

    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    fn write(&self, path: &str, content: impl AsRef<[u8]>) {
        write(&self.repo(), path, content);
    }

    fn target(&self, host: &str, path: &str, content: &str) {
        write(self.root.path(), &format!("target/{host}{path}"), content);
    }

    fn starts(&self) -> usize {
        match fs::metadata(self.root.path().join("bin/ruby.starts")) {
            Ok(metadata) => metadata.len().try_into().unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("could not read Ruby start counter: {error}"),
        }
    }

    fn record(&self, stage: UnitStage, unit: String, host: Option<&str>) -> PathBuf {
        self.cache.record_path(&UnitKey {
            stage,
            unit,
            params: BTreeMap::new(),
            target: host.map(|host| json!([host, Target::Local]).to_string()),
        })
    }

    fn collect_record(&self, priority: Priority, name: &str, entry: &str) -> PathBuf {
        self.record(
            UnitStage::Collect,
            json!([priority, name, entry]).to_string(),
            None,
        )
    }

    fn expand_record(&self, host: &str, module: &str) -> PathBuf {
        self.record(UnitStage::Expand, module.into(), Some(host))
    }

    fn merge_record(&self, host: &str) -> PathBuf {
        self.record(UnitStage::Merge, host.into(), Some(host))
    }

    fn cache_stamp(&self) -> BTreeMap<PathBuf, u64> {
        let key = self.merge_record("server");
        let units = key.parent().unwrap().parent().unwrap();
        if !units.exists() {
            return BTreeMap::new();
        }
        units
            .read_dir()
            .unwrap()
            .map(|shard| shard.unwrap().path())
            .flat_map(|shard| shard.read_dir().unwrap())
            .map(|record| record.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "mpk"))
            .map(|path| {
                let inode = fs::metadata(&path).unwrap().ino();
                (path, inode)
            })
            .collect()
    }

    fn run(&self, mut command: Command, description: &str) -> Output {
        let original = env::var_os("PATH").unwrap_or_default();
        let path = env::join_paths(
            std::iter::once(self.root.path().join("bin")).chain(env::split_paths(&original)),
        )
        .unwrap();
        let output = command
            .env("PATH", path)
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("DEK_TEST_INCREMENTAL_RUBY", &self.ruby)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{description}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn plan(&self, description: &str, host: &str) -> Run {
        let starts = self.starts();
        let before = self.cache_stamp();
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(["--exact", "incremental::run_plan_with_memory_target"])
            .env(HOST_FIXTURE, host)
            .env(ROOT_FIXTURE, self.root.path());
        let start = Instant::now();
        self.run(command, description);
        let elapsed = start.elapsed();

        let output =
            serde_json::from_slice(&fs::read(self.root.path().join("plan.json")).unwrap()).unwrap();
        let starts = self.starts() - starts;
        let published = self
            .cache_stamp()
            .into_iter()
            .filter_map(|(path, inode)| (before.get(&path) != Some(&inode)).then_some(path))
            .collect::<BTreeSet<_>>();
        eprintln!(
            "{description} ({host}): {starts} Ruby starts, {} cache publications, {elapsed:?}",
            published.len()
        );
        Run {
            output,
            starts,
            published,
        }
    }

    fn git_branch(&self, host: &str, expected_starts: usize) -> Value {
        let before = self.starts();
        let mut command = Command::new(env!("CARGO_BIN_EXE_dek"));
        command
            .args([
                "show",
                "programs.git.config",
                "-H",
                host,
                "--json",
                "--repo",
            ])
            .arg(self.repo());
        let output = self.run(command, "show Git options");
        assert_eq!(self.starts() - before, expected_starts, "show -H {host}");
        let output: Value = serde_json::from_slice(&output.stdout).unwrap();
        output["options"]["programs.git.config"]["value"]["init"]["defaultBranch"].clone()
    }
}

#[test]
fn run_plan_with_memory_target() {
    let Ok(host) = env::var(HOST_FIXTURE) else {
        return;
    };
    let root = PathBuf::from(env::var_os(ROOT_FIXTURE).unwrap());
    let repo = root.join("repo");
    let memory = MemoryTarget::default();
    memory.deployed.set(true);
    for path in [CADDY_PATH, GIT_PATH] {
        let contents = fs::read(root.join(format!("target/{host}{path}"))).unwrap();
        memory.set_file_content(
            path,
            FileContent {
                digest: digest(&contents),
                attributes: FileAttributes {
                    mode: (path == GIT_PATH).then(|| "0644".into()),
                    ..FileAttributes::default()
                },
            },
        );
    }

    let mut out = Vec::new();
    dek_cli::command::run(
        [
            "dek",
            "plan",
            "-H",
            &host,
            "--json",
            "--repo",
            repo.to_str().unwrap(),
        ],
        &mut out,
        || Ok(memory.registry()),
    )
    .unwrap();
    assert_eq!(memory.executes.get(), 0);
    write(&root, "plan.json", out);
}

fn set_base(fixture: &Fixture, branch: &str) {
    fixture.write(
        BASE,
        format!(
            "configure 'programs.git', enable: true, config: {{ init: {{ defaultBranch: {branch:?} }} }}\n\
             service :caddy, enable: true, configFile: source_file('./Caddyfile')\n"
        ),
    );
}

fn git_content(branch: &str) -> String {
    format!("[init]\n\tdefaultBranch = \"{branch}\"\n")
}

fn assert_caddy_plan(run: &Run, before: &str, after: &str) {
    let actions = run.actions();
    let [file, reload] = actions.as_slice() else {
        panic!("expected Caddyfile update and service reload, got {actions:?}");
    };
    assert!(matches!(&file.kind, ActionKind::WriteFile { path, content }
        if path == CADDY_PATH && content.digest == digest(after.as_bytes())));
    assert!(matches!(&file.preconditions[..], [Precondition::File {
        path, expected: FileObservation::Present(content)
    }] if path == CADDY_PATH && content.digest == digest(before.as_bytes())));
    assert!(
        matches!(&reload.kind, ActionKind::NotifyService { unit, effect: ServiceEffect::Reload }
        if unit.name == "caddy.service")
    );
    assert_eq!(reload.depends_on, std::slice::from_ref(&file.id));
}

#[test]
fn two_hosts_share_collection_and_plan_only_current_differences() {
    let fixture = Fixture::new();
    let server_entry = "hosts/server/default.rb";
    let laptop_entry = "hosts/laptop/default.json";
    fixture.write(
        "dek.json",
        json!({
            "schemaVersion": 1,
            "hosts": {
                "laptop": {"entry": laptop_entry, "target": {"local": true}},
                "server": {"entry": server_entry, "target": {"local": true}, "inputs": "sdk"},
            },
            "profiles": {"base": {"entry": BASE, "inputs": "sdk"}},
        })
        .to_string(),
    );
    fixture.write(server_entry, "profile 'base'\n");
    write_json(&fixture.repo(), laptop_entry, &["base"], json!([]));
    set_base(&fixture, "main");
    fixture.write(CADDYFILE, CADDY_INITIAL);
    for host in ["server", "laptop"] {
        fixture.target(host, CADDY_PATH, CADDY_INITIAL);
        fixture.target(host, GIT_PATH, &git_content("main"));
    }

    let base = fixture.collect_record(Priority::Profile, "base", BASE);
    let server = fixture.collect_record(Priority::Host, "server", server_entry);
    let laptop = fixture.collect_record(Priority::Host, "laptop", laptop_entry);
    let cold_server = fixture.plan("cold", "server");
    assert_eq!(cold_server.output["host"], "server");
    assert!(cold_server.actions().is_empty());
    assert_eq!(cold_server.starts, 1);
    assert!(cold_server.published.contains(&server));
    assert!(cold_server.published.contains(&base));
    assert!(
        cold_server
            .published
            .contains(&fixture.expand_record("server", "services.caddy"))
    );

    let cross_host = fixture.plan("shared profile already cached", "laptop");
    assert_eq!(cross_host.output["host"], "laptop");
    assert!(cross_host.actions().is_empty());
    assert_eq!(cross_host.starts, 0);
    assert!(cross_host.published.contains(&laptop));
    assert!(!cross_host.published.contains(&base));
    assert!(
        cross_host
            .published
            .contains(&fixture.expand_record("laptop", "services.caddy"))
    );

    let warm_server = fixture.plan("warm", "server");
    warm_server.assert_warm();
    assert_eq!(warm_server.output, cold_server.output);

    write_json(
        &fixture.repo(),
        laptop_entry,
        &["base"],
        json!([
            {"path":"environment.systemPackages","value":["ripgrep"]}
        ]),
    );
    let after_laptop_edit = fixture.plan("after editing the laptop entry", "server");
    after_laptop_edit.assert_warm();
    assert_eq!(after_laptop_edit.output, warm_server.output);
    let changed_laptop = fixture.plan("edited laptop entry", "laptop");
    assert_eq!(changed_laptop.starts, 0);
    assert_ne!(
        changed_laptop.output["resourceDigest"],
        cross_host.output["resourceDigest"],
    );
    assert_eq!(
        changed_laptop.published,
        BTreeSet::from([
            laptop,
            fixture.merge_record("laptop"),
            fixture.expand_record("laptop", "environment.systemPackages"),
        ]),
    );

    fixture.write(CADDYFILE, CADDY_UPDATED);
    let caddy = fixture.plan("after editing only Caddyfile", "server");
    caddy.assert_warm();
    assert_caddy_plan(&caddy, CADDY_INITIAL, CADDY_UPDATED);
    for host in ["server", "laptop"] {
        fixture.target(host, CADDY_PATH, CADDY_UPDATED);
    }

    set_base(&fixture, "next");
    for (host, starts) in [("server", 1), ("laptop", 0)] {
        let git = fixture.plan("after changing shared Git settings", host);
        assert_eq!(git.starts, starts);
        let mut expected = BTreeSet::from([
            fixture.merge_record(host),
            fixture.expand_record(host, "programs.git"),
        ]);
        if host == "server" {
            expected.insert(base.clone());
        }
        assert_eq!(git.published, expected);
        let actions = git.actions();
        assert!(
            matches!(&actions[..], [Action {
            kind: ActionKind::WriteFile { path, content }, ..
        }] if path == GIT_PATH && content.digest == digest(git_content("next").as_bytes())),
            "{actions:?}"
        );
        assert_eq!(fixture.git_branch(host, 0), "next");
        fixture.target(host, GIT_PATH, &git_content("next"));
        let converged = fixture.plan("after matching target state", host);
        converged.assert_warm();
        assert!(converged.actions().is_empty());
    }

    let drift = ":8080 {\n  respond \"manual edit\"\n}\n";
    fixture.target("server", CADDY_PATH, drift);
    let drifted = fixture.plan("after editing the target file", "server");
    drifted.assert_warm();
    assert_caddy_plan(&drifted, drift, CADDY_UPDATED);
    assert_eq!(
        fs::read_to_string(
            fixture
                .root
                .path()
                .join("target/server/etc/caddy/Caddyfile")
        )
        .unwrap(),
        drift
    );
}

#[test]
fn native_io_runs_each_time_while_unchanged_sdk_inputs_reuse_collection() {
    let fixture = Fixture::new();
    fixture.write(
        "dek.json",
        json!({
            "schemaVersion": 1,
            "hosts": {
                "laptop": {"entry": "laptop.rb", "target": {"local": true}},
                "server": {"entry": "server.rb", "target": {"local": true}, "inputs": "sdk"},
            },
        })
        .to_string(),
    );
    fixture.write("laptop.rb", "configure 'programs.git.config', { init: { defaultBranch: File.read(File.join(__dir__, 'native.txt')) } }\n");
    fixture.write(
        "server.rb",
        "configure 'programs.git.config', { init: { defaultBranch: read_text('sdk.txt') } }\n",
    );
    fixture.write("native.txt", "before");
    fixture.write("sdk.txt", "tracked");

    assert_eq!(fixture.git_branch("server", 1), "tracked");
    assert_eq!(fixture.git_branch("laptop", 1), "before");
    assert_eq!(fixture.git_branch("laptop", 1), "before");
    fixture.write("native.txt", "after");
    assert_eq!(fixture.git_branch("laptop", 1), "after");
    assert_eq!(fixture.git_branch("server", 0), "tracked");
}
