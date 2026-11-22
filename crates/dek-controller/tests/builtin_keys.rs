use std::{fs, path::Path};

use dek_controller::{
    eval::{JsonCollector, collect_graph},
    modules::{
        ExpandedResources, ModuleExpansion, ModuleOutput, ModuleRegistry, SchemaRegistry,
        assemble_resources, register_key_modules, register_key_schemas, register_system_modules,
        register_system_schemas,
    },
    repo::{HostSelector, Repository},
};
use dek_core::{
    DesiredResource, InputKind, OptionAssignment, Origin, Resource, ResourceId, UnitSpec,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PUBLIC: &[u8] = b"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFIxture alice\n";
const INLINE: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIInline alice";
const CIPHERTEXT: &[u8] = b"age-encryption.org/v1\nsample ciphertext bytes\x00\xff";
const SECRET: &str = "/home/alice/.ssh/id_git_deploy";
const IDENTITY: &str = "/etc/dek/identities/server.agekey";

fn write(root: &Path, relative: &str, data: impl AsRef<[u8]>) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

fn json_options(root: &Path, options: Value) -> UnitSpec {
    write(
        root,
        "dek.json",
        r#"{"schemaVersion":1,"hosts":{"server":{"entry":"hosts/server/default.json","target":{"local":true}}}}"#,
    );
    write(
        root,
        "hosts/server/default.json",
        json!({"schemaVersion":1, "imports":[], "options":options}).to_string(),
    );
    let repo = Repository::load(root).unwrap();
    collect_graph(&repo, HostSelector::Named("server"), &mut JsonCollector)
        .unwrap()
        .host
        .spec
}

fn drafts(
    root: &Path,
    profiles: &[UnitSpec],
    host: &UnitSpec,
) -> Result<Vec<ModuleExpansion>, String> {
    let mut schemas = SchemaRegistry::default();
    register_system_schemas(&mut schemas).unwrap();
    register_key_schemas(&mut schemas).unwrap();
    let mut modules = ModuleRegistry::default();
    register_system_modules(&mut modules).unwrap();
    register_key_modules(&mut modules).unwrap();
    let config = schemas
        .merge(profiles, host)
        .map_err(|err| err.to_string())?;
    modules.expand(root, &config).map_err(|err| err.to_string())
}

fn find(result: &ExpandedResources, id: ResourceId) -> &Resource {
    result
        .resources
        .resources
        .iter()
        .find(|resource| resource.id() == id)
        .unwrap_or_else(|| panic!("missing resource {id:?}"))
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn json_origin(path: &str, pointer: &str) -> Origin {
    Origin::Json {
        file: path.into(),
        pointer: pointer.parse().unwrap(),
    }
}

#[test]
fn maps_repository_key_and_ciphertext_files_without_reading_target_identities() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let host = json_options(
        root,
        json!([
            {"path":"users.groups.research","value":{}},
            {"path":"users.users.alice.isNormalUser","value":true},
            {"path":"users.users.alice.home","value":"/home/alice"},
            {"path":"systemd.tmpfiles.settings.dek","value":{"/home/alice/.ssh":{"d":{
                "user":"alice", "group":"research", "mode":"0700"
            }}}},
            {"path":"users.users.alice.openssh.authorizedKeys.keys","value":[INLINE]},
            {"path":"users.users.alice.openssh.authorizedKeys.keyFiles","value":["../../keys/alice.pub"]},
            {"path":"age.identityPaths","value":[IDENTITY,r"C:\Target\identities\spare.agekey"]},
            {"path":"age.secrets.gitDeploy.file","value":"../../secrets/gitDeploy.age"},
            {"path":"age.secrets.gitDeploy.path","value":SECRET},
            {"path":"age.secrets.gitDeploy.owner","value":"alice"},
            {"path":"age.secrets.gitDeploy.group","value":"research"},
            {"path":"age.secrets.gitDeploy.mode","value":"0600"}
        ]),
    );

    let mut drafts = drafts(root, &[], &host).unwrap();
    for expansion in &mut drafts {
        let encoded = rmp_serde::to_vec(&expansion.output).unwrap();
        expansion.output = rmp_serde::from_slice::<ModuleOutput>(&encoded).unwrap();
        assert!(expansion.output.contents.is_empty());
        assert!(expansion.output.inputs.is_empty());
    }
    assert!(assemble_resources(root, &drafts).is_err());
    write(root, "keys/alice.pub", PUBLIC);
    write(root, "secrets/gitDeploy.age", CIPHERTEXT);
    let result = assemble_resources(root, &drafts).unwrap();

    let authorized = find(&result, ResourceId::AuthorizedKeys("alice".into()));
    let DesiredResource::AuthorizedKeys(keys) = &authorized.desired else {
        unreachable!()
    };
    assert_eq!(keys.user, "alice");
    assert_eq!(keys.keys, [INLINE]);
    assert_eq!(keys.key_files.len(), 1);
    assert_eq!(keys.key_files[0].0, digest(PUBLIC));
    assert!(
        authorized
            .depends_on
            .contains(&ResourceId::User("alice".into()))
    );
    assert!(
        authorized
            .depends_on
            .contains(&ResourceId::Path("/home/alice/.ssh".into()))
    );

    let secret = find(&result, ResourceId::Path(SECRET.into()));
    let DesiredResource::AgeSecret(age) = &secret.desired else {
        unreachable!()
    };
    assert_eq!(age.ciphertext.0, digest(CIPHERTEXT));
    assert_eq!(age.path, SECRET);
    assert_eq!(
        age.identity_paths,
        [IDENTITY, r"C:\Target\identities\spare.agekey"]
    );
    assert_eq!(age.attributes.owner.as_deref(), Some("alice"));
    assert_eq!(age.attributes.group.as_deref(), Some("research"));
    assert_eq!(age.attributes.mode.as_deref(), Some("0600"));
    for dependency in [
        ResourceId::User("alice".into()),
        ResourceId::Group("research".into()),
        ResourceId::Path("/home/alice/.ssh".into()),
    ] {
        assert!(secret.depends_on.contains(&dependency));
    }
    assert_eq!(result.contents.len(), 2);
    assert_eq!(result.contents[&age.ciphertext], CIPHERTEXT);
    assert_eq!(result.contents[&keys.key_files[0]], PUBLIC);
    assert_eq!(result.file_inputs.len(), 2);
    assert!(
        result
            .file_inputs
            .iter()
            .all(|input| input.kind == InputKind::File)
    );
    assert!(
        result
            .file_inputs
            .iter()
            .any(|input| input.origin
                == Some(json_origin("hosts/server/default.json", "/options/5")))
    );
    assert!(
        result
            .file_inputs
            .iter()
            .any(|input| input.origin
                == Some(json_origin("hosts/server/default.json", "/options/7")))
    );

    let revised = b"age-encryption.org/v1\nnew ciphertext";
    write(root, "secrets/gitDeploy.age", revised);
    let next = assemble_resources(root, &drafts).unwrap();
    assert_ne!(next.resources, result.resources);
    assert!(next.contents.values().any(|bytes| bytes == revised));
    assert_eq!(next.file_inputs.len(), 2);
}

#[test]
fn empty_authorized_keys_manages_an_empty_set_and_omission_keeps_it_unmanaged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for field in ["keys", "keyFiles"] {
        let host = json_options(
            root,
            json!([
                {"path":format!("users.users.alice.openssh.authorizedKeys.{field}"),"value":[]}
            ]),
        );
        let result = assemble_resources(root, &drafts(root, &[], &host).unwrap()).unwrap();
        assert_eq!(result.resources.resources.len(), 1);
        let resource = find(&result, ResourceId::AuthorizedKeys("alice".into()));
        assert!(
            matches!(&resource.desired, DesiredResource::AuthorizedKeys(keys)
            if keys.keys.is_empty() && keys.key_files.is_empty())
        );
        assert!(resource.depends_on.is_empty());
    }

    let host = json_options(root, json!([{"path":"users.users.alice","value":{}}]));
    let result = assemble_resources(root, &drafts(root, &[], &host).unwrap()).unwrap();
    assert_eq!(result.resources.resources.len(), 1);
    assert!(matches!(
        result.resources.resources[0].desired,
        DesiredResource::User(_)
    ));
}

#[test]
fn keeps_relative_key_files_from_distinct_declaring_directories() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let make_profile = |directory: &str| UnitSpec {
        options: vec![OptionAssignment {
            path: "users.users.alice.openssh.authorizedKeys.keyFiles"
                .parse()
                .unwrap(),
            value: json!(["alice.pub"]),
            origin: json_origin(&format!("{directory}/profile.json"), "/options/0"),
        }],
        ..UnitSpec::default()
    };
    write(root, "profiles/base/alice.pub", b"base public key");
    write(root, "profiles/extra/alice.pub", b"extra public key");
    let profiles = [
        make_profile("profiles/base"),
        make_profile("profiles/extra"),
    ];
    let expanded = assemble_resources(
        root,
        &drafts(root, &profiles, &UnitSpec::default()).unwrap(),
    )
    .unwrap();
    let resource = find(&expanded, ResourceId::AuthorizedKeys("alice".into()));
    let DesiredResource::AuthorizedKeys(keys) = &resource.desired else {
        unreachable!()
    };
    assert_eq!(
        keys.key_files
            .iter()
            .map(|digest| digest.0.as_str())
            .collect::<Vec<_>>(),
        [digest(b"base public key"), digest(b"extra public key")]
    );
    assert_eq!(expanded.file_inputs.len(), 2);
    assert_eq!(resource.origins.len(), 2);
}

#[test]
fn incomplete_age_declaration_reports_its_configuration_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let host = json_options(
        root,
        json!([
            {"path":"age.secrets.gitDeploy.path","value":SECRET}
        ]),
    );
    let error = drafts(root, &[], &host).unwrap_err();
    for part in [
        "age.secrets.gitDeploy.file",
        "hosts/server/default.json",
        "/options/0",
    ] {
        assert!(error.contains(part), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn repository_file_sources_cannot_resolve_through_links_outside_the_repo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let host = json_options(
        &root,
        json!([
            {"path":"age.secrets.gitDeploy.file","value":"../../secrets/link.age"},
            {"path":"age.secrets.gitDeploy.path","value":SECRET}
        ]),
    );
    let outside = dir.path().join("outside");
    fs::write(&outside, b"outside repo").unwrap();
    fs::create_dir(root.join("secrets")).unwrap();
    std::os::unix::fs::symlink(outside, root.join("secrets/link.age")).unwrap();
    let error = assemble_resources(&root, &drafts(&root, &[], &host).unwrap())
        .unwrap_err()
        .to_string();
    for part in [
        "outside the repository",
        "link.age",
        "hosts/server/default.json",
        "/options/0",
    ] {
        assert!(error.contains(part), "{error}");
    }
}
