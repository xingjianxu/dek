use std::collections::BTreeMap;

use dek_core::{EffectiveOption, FileAttributes, HostConfig, Origin};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{
    DeclaredResource, ExpandContext, ExpansionError, ModuleDefinition, ModuleOutput,
    ModuleRegistry, RepositoryFile, ResourceDraft, SchemaError, SchemaRegistry,
};

const NAME: &str = "dek.keys";
const SOURCE: &str = "crates/dek-controller/src/modules/builtin_keys/options.json";

pub fn register_key_schemas(registry: &mut SchemaRegistry) -> Result<(), SchemaError> {
    registry.register_json(NAME, SOURCE, include_str!("options.json"))
}

pub fn register_key_modules(registry: &mut ModuleRegistry<'_>) -> Result<(), ExpansionError> {
    let reads_options = ["users.users", "age.identityPaths", "age.secrets"]
        .into_iter()
        .map(|prefix| prefix.parse().expect("built-in option path"))
        .collect();

    registry.register(
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

type Fields<'a> = BTreeMap<&'a str, &'a EffectiveOption>;

fn expand(context: &ExpandContext<'_>) -> Result<ModuleOutput, String> {
    let config = context.options;
    let mut output = ModuleOutput::default();
    for (user, fields) in collect_authorized_keys(config) {
        let prefix = format!("users.users.{user}.openssh.authorizedKeys");
        let keys = field(&fields, &prefix, "keys")?.unwrap_or_default();
        let mut key_files = Vec::new();
        if let Some(option) = fields.get("keyFiles") {
            let path = format!("{prefix}.keyFiles");
            if option.effective_definitions.is_empty()
                && option
                    .value
                    .as_array()
                    .is_some_and(|files| !files.is_empty())
            {
                return Err(invalid(&path, &[], "key files require declaration origins"));
            }
            for definition in &option.effective_definitions {
                let files: Vec<String> = parse(
                    &definition.value,
                    &path,
                    std::slice::from_ref(&definition.origin),
                )?;
                key_files.extend(files.into_iter().map(|path| RepositoryFile {
                    path,
                    origin: definition.origin.clone(),
                }));
            }
        }
        output.resources.push(declare(
            ResourceDraft::AuthorizedKeys {
                user: user.into(),
                keys,
                key_files,
            },
            field_origins(fields.values().copied()),
        ));
    }

    let identities = config
        .options
        .iter()
        .find_map(|(path, option)| (path.as_str() == "age.identityPaths").then_some(option));
    let identity_paths: Vec<String> = identities
        .map(|option| parse(&option.value, "age.identityPaths", &field_origins([option])))
        .transpose()?
        .unwrap_or_default();

    for (name, fields) in collect_age_secrets(config) {
        let prefix = format!("age.secrets.{name}");
        let origins = field_origins(fields.values().copied().chain(identities));
        let path = field(&fields, &prefix, "path")?.ok_or_else(|| {
            invalid(
                &format!("{prefix}.path"),
                &origins,
                "required to deploy a named age secret",
            )
        })?;
        let option = fields.get("file").ok_or_else(|| {
            invalid(
                &format!("{prefix}.file"),
                &origins,
                "required to deploy a named age secret",
            )
        })?;
        let file_option = format!("{prefix}.file");
        let definition = option.effective_definitions.first().ok_or_else(|| {
            invalid(
                &file_option,
                &origins,
                "ciphertext requires a declaration origin",
            )
        })?;
        let ciphertext = RepositoryFile {
            path: parse(
                &definition.value,
                &file_option,
                std::slice::from_ref(&definition.origin),
            )?,
            origin: definition.origin.clone(),
        };
        let attributes = FileAttributes {
            owner: field(&fields, &prefix, "owner")?,
            group: field(&fields, &prefix, "group")?,
            mode: field(&fields, &prefix, "mode")?,
        };
        output.resources.push(declare(
            ResourceDraft::AgeSecret {
                path,
                ciphertext,
                identity_paths: identity_paths.clone(),
                attributes,
            },
            origins,
        ));
    }
    Ok(output)
}

fn collect_authorized_keys(config: &HostConfig) -> BTreeMap<&str, Fields<'_>> {
    let mut users = BTreeMap::<_, Fields<'_>>::new();
    for (path, option) in &config.options {
        if let Some((user, field)) = path
            .as_str()
            .strip_prefix("users.users.")
            .and_then(|path| path.split_once(".openssh.authorizedKeys."))
            && !user.contains('.')
            && matches!(field, "keys" | "keyFiles")
        {
            users.entry(user).or_default().insert(field, option);
        }
    }
    users
}

fn collect_age_secrets(config: &HostConfig) -> BTreeMap<&str, Fields<'_>> {
    let mut secrets = BTreeMap::<_, Fields<'_>>::new();
    for (path, option) in &config.options {
        if let Some((name, field)) = path
            .as_str()
            .strip_prefix("age.secrets.")
            .and_then(|path| path.split_once('.'))
            && matches!(field, "file" | "path" | "owner" | "group" | "mode")
        {
            secrets.entry(name).or_default().insert(field, option);
        }
    }
    secrets
}

fn declare(desired: ResourceDraft, origins: Vec<Origin>) -> DeclaredResource {
    DeclaredResource {
        origins,
        ..DeclaredResource::new(desired)
    }
}

fn field<T: DeserializeOwned>(
    fields: &Fields<'_>,
    prefix: &str,
    name: &str,
) -> Result<Option<T>, String> {
    fields
        .get(name)
        .map(|option| {
            parse(
                &option.value,
                &format!("{prefix}.{name}"),
                &field_origins([*option]),
            )
        })
        .transpose()
}

fn field_origins<'a>(options: impl IntoIterator<Item = &'a EffectiveOption>) -> Vec<Origin> {
    let mut origins = Vec::new();
    for option in options {
        for definition in &option.effective_definitions {
            if !origins.contains(&definition.origin) {
                origins.push(definition.origin.clone());
            }
        }
    }
    origins
}

fn parse<T: DeserializeOwned>(value: &Value, path: &str, origins: &[Origin]) -> Result<T, String> {
    serde_path_to_error::deserialize(value.clone())
        .map_err(|error| invalid(path, origins, &error.to_string()))
}

fn invalid(path: &str, origins: &[Origin], problem: &str) -> String {
    let sources = origins
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if sources.is_empty() {
        format!("{path}: {problem}")
    } else {
        format!("{path} at {sources}: {problem}")
    }
}
