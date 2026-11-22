use std::fs;

use dek_core::{
    DesiredResource, GroupResource, Origin, Resource, ResourceOrigin, ResourceSet, TargetIdentity,
    UserResource,
};
use dek_target::{
    backend::{BackendError, BackendRegistry},
    planner::{PlanError, plan},
};

use super::{
    AccountBackend,
    database::Database,
    nss::{Identity, Key, NameService},
    primary::replace_gid,
};

const PASSWD: &str = "alice:credential-fixture:1700:2000:Original:/home/alice:/bin/bash\n";
const GROUP: &str = "local:x:2000:\nother:x:2001:alice\n";
const USERS: &[(&str, u32)] = &[("alice", 1700), ("external", 3500)];
const GROUPS: &[(&str, u32)] = &[("local", 2000), ("other", 2001), ("external", 3501)];

struct FixtureNss;

impl NameService for FixtureNss {
    fn user(&self, key: Key<'_>) -> Result<Option<Identity>, BackendError> {
        Ok(find(USERS, key))
    }

    fn group(&self, key: Key<'_>) -> Result<Option<Identity>, BackendError> {
        Ok(find(GROUPS, key))
    }
}

fn find(input: &[(&str, u32)], key: Key<'_>) -> Option<Identity> {
    let &(name, id) = input.iter().find(|&&(name, id)| match key {
        Key::Name(wanted) => name == wanted,
        Key::Id(wanted) => id == wanted,
    })?;
    Some(Identity {
        name: name.into(),
        id,
    })
}

fn fixture() -> (tempfile::TempDir, AccountBackend) {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("passwd"), PASSWD).unwrap();
    fs::write(directory.path().join("group"), GROUP).unwrap();
    let backend = AccountBackend {
        database: Database {
            etc: directory.path().into(),
            nss: Box::new(FixtureNss),
        },
    };
    (directory, backend)
}

fn registry(backend: &AccountBackend) -> BackendRegistry<'_> {
    let mut registry = BackendRegistry::new(
        TargetIdentity {
            id: "nss-fixture".into(),
            os: "linux".into(),
            distribution: Some("arch".into()),
            architecture: "x86_64".into(),
        },
        1,
        |path| Ok(path.into()),
    );
    registry.register(backend).unwrap();
    registry
}

fn resource(desired: DesiredResource) -> Resource {
    Resource {
        desired,
        origins: vec![ResourceOrigin {
            module: "fixture".into(),
            origin: Origin::Json {
                file: "accounts.json".into(),
                pointer: "/options/0".parse().unwrap(),
            },
        }],
        depends_on: vec![],
        notifications: vec![],
    }
}

fn user(name: &str, uid: Option<u32>) -> UserResource {
    UserResource {
        name: name.into(),
        kind: None,
        uid,
        group: None,
        extra_groups: None,
        home: None,
        shell: None,
        description: None,
    }
}

fn group(name: &str, gid: Option<u32>) -> DesiredResource {
    DesiredResource::Group(GroupResource {
        name: name.into(),
        gid,
    })
}

#[test]
fn external_names_visible_id_collisions_and_implicit_migrations_fail_without_writes() {
    let (directory, backend) = fixture();
    let registry = registry(&backend);
    for (desired, diagnostic) in [
        (DesiredResource::User(user("external", None)), "外部 NSS"),
        (group("external", None), "外部 NSS"),
        (DesiredResource::User(user("new", Some(1700))), "占用"),
        (DesiredResource::User(user("new", Some(3500))), "NSS 条目"),
        (group("new", Some(2000)), "占用"),
        (group("new", Some(3501)), "NSS 条目"),
        (DesiredResource::User(user("alice", Some(1701))), "单独迁移"),
        (group("local", Some(2002)), "单独迁移"),
    ] {
        let Err(PlanError::Resource {
            locations, message, ..
        }) = plan(
            &registry,
            "repo",
            &ResourceSet {
                resources: vec![resource(desired)],
            },
            &[],
        )
        else {
            panic!("a conflicting account declaration must fail before mutation");
        };
        assert!(locations.contains("accounts.json"));
        assert!(message.contains(diagnostic), "{message}");
        assert!(!message.contains("credential-fixture"));
    }
    assert_eq!(
        fs::read_to_string(directory.path().join("passwd")).unwrap(),
        PASSWD
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("group")).unwrap(),
        GROUP
    );
}

#[test]
fn missing_references_and_same_plan_duplicate_ids_are_rejected() {
    let (_directory, backend) = fixture();
    let registry = registry(&backend);
    let mut alice = user("alice", None);
    alice.extra_groups = Some(vec!["unknown".into()]);
    let missing = ResourceSet {
        resources: vec![resource(DesiredResource::User(alice))],
    };
    let Err(error) = plan(&registry, "repo", &missing, &[]) else {
        panic!("unmanaged missing groups must be detected before execution");
    };
    assert!(error.to_string().contains("unknown"));

    let duplicate = ResourceSet {
        resources: vec![
            resource(group("new_a", Some(4000))),
            resource(group("new_b", Some(4000))),
        ],
    };
    let Err(error) = plan(&registry, "repo", &duplicate, &[]) else {
        panic!("two declarations must not reserve the same GID");
    };
    assert!(error.to_string().contains("4000"));
}

#[test]
fn primary_gid_edit_preserves_other_raw_entry_fields_and_rejects_stale_identity() {
    let source = b"# account fixture\nalice:x:1700:2000:Some raw \xff:/old/home:/bin/sh\nbob:x:1760:2000:::/usr/bin/nologin";
    let changed = replace_gid(source, "alice", 1700, 2000, 3000).unwrap();
    let expected = b"# account fixture\nalice:x:1700:3000:Some raw \xff:/old/home:/bin/sh\nbob:x:1760:2000:::/usr/bin/nologin";
    assert_eq!(changed, expected);
    assert!(replace_gid(source, "alice", 1701, 2000, 3000).is_err());
    assert!(replace_gid(source, "alice", 1700, 2001, 3000).is_err());
    assert!(replace_gid(source, "deleted", 1700, 2000, 3000).is_err());
}

#[test]
fn fixed_gid_is_reserved_before_automatic_allocation() {
    let (_directory, backend) = fixture();
    let registry = registry(&backend);
    let resources = ResourceSet {
        resources: vec![
            resource(group("auto", None)),
            resource(group("fixed", Some(4400))),
        ],
    };
    let review = plan(&registry, "repo", &resources, &[]).unwrap();
    let [fixed, automatic] = &review.actions[..] else {
        panic!("both new groups need creation");
    };
    assert!(
        matches!(&fixed.kind, dek_core::ActionKind::EnsureGroup(group) if group.name == "fixed" && group.gid == Some(4400))
    );
    assert!(
        matches!(&automatic.kind, dek_core::ActionKind::EnsureGroup(group) if group.name == "auto" && group.gid.is_none())
    );
    assert_eq!(automatic.depends_on, [fixed.id.as_str()]);
}
