use std::collections::{BTreeMap, BTreeSet};

use dek_core::{
    DesiredResource, EffectiveOption, GroupResource, HostConfig, OptionPath, ResourceId, UserKind,
    UserResource,
};
use serde::de::DeserializeOwned;

use super::{ModuleOutput, invalid, origins, parse, ready, unique_origins};

pub(super) struct ManagedAccounts {
    pub(super) users: BTreeSet<String>,
    pub(super) groups: BTreeSet<String>,
}

type Fields<'a> = BTreeMap<&'a str, (&'a OptionPath, &'a EffectiveOption)>;

pub(super) fn expand(
    config: &HostConfig,
    output: &mut ModuleOutput,
) -> Result<ManagedAccounts, String> {
    let groups = collect(config, "users.groups.", &["", "gid"]);
    let users = collect(
        config,
        "users.users.",
        &[
            "",
            "isNormalUser",
            "isSystemUser",
            "uid",
            "group",
            "extraGroups",
            "home",
            "shell",
            "description",
        ],
    );
    let managed = ManagedAccounts {
        users: users.keys().map(|name| (*name).into()).collect(),
        groups: groups.keys().map(|name| (*name).into()).collect(),
    };

    for (name, fields) in groups {
        validate_presence(&fields)?;
        let resource = GroupResource {
            name: name.into(),
            gid: field(&fields, "gid")?,
        };
        output.resources.push(ready(
            DesiredResource::Group(resource),
            field_origins(&fields),
            Vec::new(),
        ));
    }

    for (name, fields) in users {
        validate_presence(&fields)?;
        let sources = field_origins(&fields);
        let normal = field::<bool>(&fields, "isNormalUser")? == Some(true);
        let system = field::<bool>(&fields, "isSystemUser")? == Some(true);
        let kind = match (normal, system) {
            (true, true) => {
                return Err(invalid(
                    &format!("users.users.{name}"),
                    &sources,
                    "isNormalUser and isSystemUser cannot both be true",
                ));
            }
            (true, false) => Some(UserKind::Normal),
            (false, true) => Some(UserKind::System),
            (false, false) => None,
        };
        let resource = UserResource {
            name: name.into(),
            kind,
            uid: field(&fields, "uid")?,
            group: field(&fields, "group")?,
            extra_groups: field(&fields, "extraGroups")?,
            home: field(&fields, "home")?,
            shell: field(&fields, "shell")?,
            description: field(&fields, "description")?,
        };
        let depends_on: BTreeSet<_> = resource
            .group
            .iter()
            .chain(resource.extra_groups.iter().flatten())
            .filter(|group| managed.groups.contains(*group))
            .cloned()
            .map(ResourceId::Group)
            .collect();
        output.resources.push(ready(
            DesiredResource::User(resource),
            sources,
            depends_on.into_iter().collect(),
        ));
    }
    Ok(managed)
}

fn collect<'a>(
    config: &'a HostConfig,
    prefix: &str,
    supported: &[&str],
) -> BTreeMap<&'a str, Fields<'a>> {
    let mut accounts = BTreeMap::<_, Fields<'_>>::new();
    for (path, option) in &config.options {
        if let Some(suffix) = path.as_str().strip_prefix(prefix) {
            let (name, field) = suffix.split_once('.').unwrap_or((suffix, ""));
            if supported.contains(&field) {
                accounts
                    .entry(name)
                    .or_default()
                    .insert(field, (path, option));
            }
        }
    }
    accounts
}

fn validate_presence(fields: &Fields<'_>) -> Result<(), String> {
    if let Some(&(path, option)) = fields.get("")
        && !option
            .value
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    {
        return Err(invalid(
            path.as_str(),
            &origins(option),
            "expected {} for account presence; use dotted paths for attributes",
        ));
    }
    Ok(())
}

fn field<T: DeserializeOwned>(fields: &Fields<'_>, field: &str) -> Result<Option<T>, String> {
    fields
        .get(field)
        .map(|(path, option)| parse(&option.value, path.as_str(), &origins(option)))
        .transpose()
}

fn field_origins(fields: &Fields<'_>) -> Vec<dek_core::Origin> {
    unique_origins(
        fields
            .values()
            .flat_map(|(_, option)| option.effective_definitions.iter().map(|def| &def.origin)),
    )
}
