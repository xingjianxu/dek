use std::{error::Error, fs, path::Path};

use dek_controller::{
    eval::{JsonCollector, collect_graph},
    modules::{
        ExpandedResources, ModuleExpansion, ModuleRegistry, SchemaRegistry, assemble_resources,
        register_app_modules, register_app_schemas,
    },
    repo::{HostSelector, Repository},
};
use dek_core::{
    DesiredResource, FileState, NotifyWhen, PackageId, PackageSource, Presence, Resource,
    ResourceId, ServiceEffect, UnitId, UnitScope,
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

const HOST: &str = "hosts/demo/default.json";
const PROFILE: &str = "profiles/base.json";
const CADDYFILE: &str = "/etc/caddy/Caddyfile";
const GITCONFIG: &str = "/etc/gitconfig";

fn write_json(root: &Path, path: &str, value: Value) {
    fs::write(root.join(path), serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn write_host(root: &Path, options: Value) {
    write_json(
        root,
        HOST,
        json!({"schemaVersion": 1, "imports": ["base"], "options": options}),
    );
}

fn repository(host: Value, profile: Value) -> TempDir {
    let root = tempdir().unwrap();
    let path = root.path();
    for directory in ["hosts/demo", "profiles"] {
        fs::create_dir_all(path.join(directory)).unwrap();
    }
    write_json(
        path,
        "dek.json",
        json!({
            "schemaVersion": 1,
            "hosts": {"demo": {"entry": HOST, "target": {"local": true}}},
            "profiles": {"base": PROFILE}
        }),
    );
    write_json(
        path,
        PROFILE,
        json!({"schemaVersion": 1, "imports": [], "options": profile}),
    );
    write_host(path, host);
    root
}

fn expand(root: &Path) -> Result<(Vec<ModuleExpansion>, ExpandedResources), Box<dyn Error>> {
    let repo = Repository::load(root)?;
    let graph = collect_graph(&repo, HostSelector::Named("demo"), &mut JsonCollector)?;
    let profiles = graph
        .profiles
        .into_iter()
        .map(|unit| unit.spec)
        .collect::<Vec<_>>();
    let mut schemas = SchemaRegistry::default();
    register_app_schemas(&mut schemas)?;
    let config = schemas.merge(&profiles, &graph.host.spec)?;
    let mut modules = ModuleRegistry::default();
    register_app_modules(&mut modules)?;
    let drafts = modules.expand(repo.root(), &config)?;
    let result = assemble_resources(repo.root(), &drafts)?;
    Ok((drafts, result))
}

fn package(source: PackageSource, name: &str) -> ResourceId {
    ResourceId::Package(PackageId {
        name: name.into(),
        source,
    })
}

fn system_unit(name: &str) -> UnitId {
    UnitId {
        scope: UnitScope::System,
        name: name.into(),
    }
}

fn resource(expansion: &ExpandedResources, id: ResourceId) -> &Resource {
    expansion
        .resources
        .resources
        .iter()
        .find(|resource| resource.id() == id)
        .unwrap()
}

fn file_contents<'a>(expansion: &'a ExpandedResources, file: &Resource) -> &'a [u8] {
    let DesiredResource::File(file) = &file.desired else {
        panic!("expected a file resource");
    };
    let FileState::Present(content) = &file.state else {
        panic!("expected present file contents");
    };
    expansion.contents[&content.digest].as_slice()
}

#[test]
fn json_git_config_generates_package_and_keeps_cased_keys() {
    let root = repository(
        json!([]),
        json!([
            {"path": "programs.git.enable", "value": true},
            {"path": "programs.git.config", "value": {
                "init": {"defaultBranch": "main"},
                "url": {"https://example.test/Acme/": {"insteadOf": ["acme:", "alias:"]}}
            }}
        ]),
    );
    let (drafts, result) = expand(root.path()).unwrap();
    let git = drafts
        .iter()
        .find(|draft| draft.definition.name == "programs.git")
        .unwrap();
    let reads: Vec<_> = git
        .options
        .options
        .keys()
        .map(|path| path.as_str())
        .collect();
    assert_eq!(reads, ["programs.git.config", "programs.git.enable"]);

    let package_id = package(PackageSource::Official, "git");
    let git_package = resource(&result, package_id.clone());
    assert!(
        matches!(&git_package.desired, DesiredResource::Package(pkg) if pkg.state == Presence::Present)
    );
    assert_eq!(git_package.origins[0].origin.file(), PROFILE);

    let config = resource(&result, ResourceId::Path(GITCONFIG.into()));
    let text = std::str::from_utf8(file_contents(&result, config)).unwrap();
    assert!(
        text.contains("[init]\n\tdefaultBranch = \"main\"\n"),
        "{text}"
    );
    assert!(text.contains("[url \"https://example.test/Acme/\"]\n\tinsteadOf = \"acme:\"\n\tinsteadOf = \"alias:\"\n"), "{text}");
    assert_eq!(config.depends_on, [package_id]);
    assert!(config.origins[0].origin.to_string().contains("/options/1"));
    assert!(result.file_inputs.is_empty());
}

#[test]
fn caddy_uses_the_effective_file_origin_and_orders_service_after_configuration() {
    let root = repository(
        json!([
            {"path": "services.caddy.enable", "value": true},
            {"path": "services.caddy.configFile", "value": "./Caddyfile"}
        ]),
        json!([{"path": "services.caddy.configFile", "value": "./Caddyfile"}]),
    );
    fs::write(root.path().join("hosts/demo/Caddyfile"), "host content").unwrap();
    fs::write(root.path().join("profiles/Caddyfile"), "profile content").unwrap();
    let (drafts, result) = expand(root.path()).unwrap();
    assert_eq!(result.resources.resources.len(), 3);
    let package_id = package(PackageSource::Official, "caddy");
    resource(&result, package_id.clone());
    let caddyfile_id = ResourceId::Path(CADDYFILE.into());
    let file = resource(&result, caddyfile_id.clone());
    assert_eq!(file_contents(&result, file), b"host content");
    assert_eq!(file.origins[0].origin.file(), HOST);
    assert_eq!(
        file.depends_on.as_slice(),
        std::slice::from_ref(&package_id)
    );

    let service_id = system_unit("caddy.service");
    let unit = resource(&result, ResourceId::SystemdUnit(service_id.clone()));
    let DesiredResource::SystemdUnit(unit_state) = &unit.desired else {
        panic!("expected a systemd service");
    };
    assert_eq!(
        (unit_state.enabled, unit_state.running),
        (Some(true), Some(true))
    );
    assert_eq!(unit.depends_on, [package_id, caddyfile_id]);
    let [notification] = &file.notifications[..] else {
        panic!("expected one service notification");
    };
    assert_eq!(notification.unit, service_id);
    assert_eq!(notification.effect, ServiceEffect::Reload);
    assert_eq!(notification.when, NotifyWhen::ContentChanged);

    let caddy = drafts
        .iter()
        .find(|draft| draft.definition.name == "services.caddy")
        .unwrap();
    let reads: Vec<_> = caddy
        .options
        .options
        .keys()
        .map(|path| path.as_str())
        .collect();
    assert_eq!(
        reads,
        ["services.caddy.configFile", "services.caddy.enable"]
    );
    assert_eq!(result.file_inputs[0].origin.as_ref().unwrap().file(), HOST);

    fs::write(root.path().join("hosts/demo/Caddyfile"), "changed file").unwrap();
    let changed = assemble_resources(root.path(), &drafts).unwrap();
    let changed_file = resource(&changed, ResourceId::Path(CADDYFILE.into()));
    assert_ne!(file.desired, changed_file.desired);
    assert_eq!(file_contents(&changed, changed_file), b"changed file");
}

#[test]
fn package_sources_are_distinct_and_removing_caddy_only_stops_management() {
    let remaining = json!([
        {"path": "environment.systemPackages", "value": ["ripgrep", "curl"]},
        {"path": "dek.arch.aurPackages", "value": ["ripgrep"]}
    ]);
    let root = repository(
        remaining.clone(),
        json!([
            {"path": "environment.systemPackages", "value": ["ripgrep"]}
        ]),
    );

    let mut initial_options = remaining.as_array().unwrap().clone();
    initial_options.push(json!({"path": "services.caddy.enable", "value": true}));
    initial_options.push(json!({"path": "services.caddy.configFile", "value": "./Caddyfile"}));
    fs::write(root.path().join("hosts/demo/Caddyfile"), "initial file").unwrap();
    write_host(root.path(), json!(initial_options));
    let (_, initial) = expand(root.path()).unwrap();
    resource(&initial, package(PackageSource::Official, "caddy"));
    resource(
        &initial,
        ResourceId::SystemdUnit(system_unit("caddy.service")),
    );
    resource(&initial, ResourceId::Path(CADDYFILE.into()));

    write_host(root.path(), remaining);
    let (_, expanded) = expand(root.path()).unwrap();
    assert_eq!(expanded.resources.resources.len(), 3);
    let official = resource(&expanded, package(PackageSource::Official, "ripgrep"));
    let aur = resource(&expanded, package(PackageSource::Aur, "ripgrep"));
    assert_eq!(
        official
            .origins
            .iter()
            .map(|source| source.origin.file())
            .collect::<Vec<_>>(),
        [PROFILE, HOST]
    );
    assert_eq!(
        aur.origins
            .iter()
            .map(|source| source.origin.file())
            .collect::<Vec<_>>(),
        [HOST]
    );
    assert_eq!(
        resource(&expanded, package(PackageSource::Official, "curl"))
            .origins
            .len(),
        1
    );
    assert!(expanded.resources.resources.iter().all(|resource|
        matches!(&resource.desired, DesiredResource::Package(pkg) if pkg.state == Presence::Present)
    ));
}

#[test]
fn openssh_registers_the_packaged_arch_service_and_empty_config_manages_nothing() {
    let root = repository(
        json!([{"path": "services.openssh.enable", "value": true}]),
        json!([]),
    );
    let (_, result) = expand(root.path()).unwrap();
    let package_id = package(PackageSource::Official, "openssh");
    resource(&result, package_id.clone());
    let service = resource(
        &result,
        ResourceId::SystemdUnit(system_unit("sshd.service")),
    );
    assert_eq!(service.depends_on, [package_id]);
    assert_eq!(service.origins[0].origin.file(), HOST);
    assert!(
        matches!(&service.desired, DesiredResource::SystemdUnit(unit)
        if unit.enabled == Some(true) && unit.running == Some(true))
    );

    write_host(root.path(), json!([]));
    let (_, empty) = expand(root.path()).unwrap();
    assert!(empty.resources.resources.is_empty());
}

#[test]
fn git_rejects_invalid_keys_with_the_effective_configuration_source() {
    let root = repository(
        json!([
            {"path": "programs.git.enable", "value": true},
            {"path": "programs.git.config", "value": {"init": {"bad\nkey": "main"}}}
        ]),
        json!([]),
    );
    let Err(error) = expand(root.path()) else {
        panic!("expected invalid Git variable to fail expansion");
    };
    let error = error.to_string();
    for expected in ["programs.git.config", HOST, "/options/1", "bad\\nkey"] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn package_lists_report_version_constraints_as_invalid_names() {
    let root = repository(
        json!([{"path": "environment.systemPackages", "value": ["ripgrep=14"]}]),
        json!([]),
    );
    let Err(error) = expand(root.path()) else {
        panic!("expected a package name error");
    };
    let error = error.to_string();
    for expected in [
        "environment.systemPackages",
        HOST,
        "/options/0",
        "ripgrep=14",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}
