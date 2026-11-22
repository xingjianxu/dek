mod accounts;
mod units;

use dek_core::{
    DesiredResource, DirectoryResource, EffectiveOption, FileAttributes, Origin, ResourceId,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

use super::{
    DeclaredResource, ExpandContext, ExpansionError, ModuleDefinition, ModuleOutput,
    ModuleRegistry, ResourceDraft, SchemaError, SchemaRegistry,
};

const NAME: &str = "dek.system";
const SOURCE: &str = "crates/dek-controller/src/modules/builtin_system/options.json";
const DIRECTORY_PREFIX: &str = "systemd.tmpfiles.settings";

pub fn register_system_schemas(registry: &mut SchemaRegistry) -> Result<(), SchemaError> {
    registry.register_json(NAME, SOURCE, include_str!("options.json"))
}

pub fn register_system_modules(registry: &mut ModuleRegistry<'_>) -> Result<(), ExpansionError> {
    let reads_options = [
        "users.users",
        "users.groups",
        "systemd.services",
        "systemd.timers",
        "systemd.user",
        "dek.users",
        DIRECTORY_PREFIX,
    ]
    .into_iter()
    .map(|prefix| prefix.parse().expect("built-in option paths"))
    .collect();

    registry.register_builtin(
        ModuleDefinition {
            name: NAME.into(),
            origin: Origin::Json {
                file: SOURCE.into(),
                pointer: "".parse().expect("root pointer"),
            },
            reads_options: Some(reads_options),
            enabled_by: None,
        },
        expand,
    )
}

fn expand(context: &ExpandContext<'_>) -> Result<ModuleOutput, String> {
    let mut output = ModuleOutput::default();
    let managed = accounts::expand(context.options, &mut output)?;
    expand_directories(context, &managed, &mut output)?;
    units::expand(context.options, &managed, &mut output)?;
    Ok(output)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryEntry {
    d: DirectoryRule,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryRule {
    user: Option<String>,
    group: Option<String>,
    mode: Option<String>,
}

fn expand_directories(
    context: &ExpandContext<'_>,
    managed: &accounts::ManagedAccounts,
    output: &mut ModuleOutput,
) -> Result<(), String> {
    for (path, option) in &context.options.options {
        let Some(config) = path.as_str().strip_prefix("systemd.tmpfiles.settings.") else {
            continue;
        };
        if config.contains('.') {
            continue;
        }
        for (target, value) in object(&option.value, path.as_str(), &origins(option))? {
            let sources = member_origins(option, target);
            let member = format!("{path} directory {target:?}");
            if !target.starts_with('/') || target.contains('\0') {
                return Err(invalid(
                    &member,
                    &sources,
                    "expected an absolute target path",
                ));
            }
            if let Some(rule) = value.get("d") {
                object(rule, &member, &sources)?;
            }
            let DirectoryEntry { d } = parse(value, &member, &sources)?;
            let mut depends_on = Vec::new();
            if let Some(owner) = &d.user
                && managed.users.contains(owner)
            {
                depends_on.push(ResourceId::User(owner.clone()));
            }
            if let Some(group) = &d.group
                && managed.groups.contains(group)
            {
                depends_on.push(ResourceId::Group(group.clone()));
            }
            output.resources.push(ready(
                DesiredResource::Directory(DirectoryResource {
                    path: target.clone(),
                    attributes: FileAttributes {
                        owner: d.user,
                        group: d.group,
                        mode: d.mode,
                    },
                }),
                sources,
                depends_on,
            ));
        }
    }
    Ok(())
}

fn ready(
    desired: DesiredResource,
    origins: Vec<Origin>,
    depends_on: Vec<ResourceId>,
) -> DeclaredResource {
    DeclaredResource {
        desired: ResourceDraft::Ready(desired),
        origins,
        depends_on,
        notifications: Vec::new(),
    }
}

fn origins(option: &EffectiveOption) -> Vec<Origin> {
    unique_origins(
        option
            .effective_definitions
            .iter()
            .map(|definition| &definition.origin),
    )
}

fn member_origins(option: &EffectiveOption, member: &str) -> Vec<Origin> {
    let matching = || {
        option
            .effective_definitions
            .iter()
            .filter(|definition| definition.value.get(member).is_some())
    };
    let priority = matching().map(|definition| definition.priority).max();
    unique_origins(
        matching()
            .filter(|definition| Some(definition.priority) == priority)
            .map(|definition| &definition.origin),
    )
}

fn unique_origins<'a>(origins: impl IntoIterator<Item = &'a Origin>) -> Vec<Origin> {
    let mut output = Vec::new();
    for source in origins {
        if !output.contains(source) {
            output.push(source.clone());
        }
    }
    output
}

fn object<'a>(
    value: &'a Value,
    label: &str,
    origins: &[Origin],
) -> Result<&'a serde_json::Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| invalid(label, origins, "expected an object"))
}

fn parse<T: DeserializeOwned>(value: &Value, label: &str, origins: &[Origin]) -> Result<T, String> {
    serde_path_to_error::deserialize(value.clone())
        .map_err(|error| invalid(label, origins, &error.to_string()))
}

fn invalid(label: &str, origins: &[Origin], problem: &str) -> String {
    let sources = origins
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if sources.is_empty() {
        format!("{label}: {problem}")
    } else {
        format!("{label} at {sources}: {problem}")
    }
}
