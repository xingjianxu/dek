use std::{
    collections::{BTreeMap, btree_map},
    fmt,
    marker::PhantomData,
    path::{Component, Path, PathBuf},
};

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, Visitor, value::MapAccessDeserializer},
};

use super::RepoError;

pub(super) const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    #[serde(deserialize_with = "unique_object_map")]
    pub hosts: BTreeMap<String, HostEntry>,
    #[serde(default, deserialize_with = "unique_map")]
    pub profiles: BTreeMap<String, ProfileEntry>,
    #[serde(default, deserialize_with = "unique_object_map")]
    pub modules: BTreeMap<String, ModuleEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestHeader {
    schema_version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostEntry {
    pub entry: PathBuf,
    pub target: Target,
    pub inputs: Option<InputPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "TargetFields", into = "TargetFields")]
pub enum Target {
    Local,
    Ssh(String),
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum TargetFields {
    Local(bool),
    Ssh(String),
}

impl TryFrom<TargetFields> for Target {
    type Error = &'static str;

    fn try_from(raw: TargetFields) -> Result<Self, Self::Error> {
        match raw {
            TargetFields::Local(true) => Ok(Self::Local),
            TargetFields::Local(false) => Err("local 必须为 true"),
            TargetFields::Ssh(alias)
                if !alias.is_empty()
                    && !alias.chars().any(|c| c.is_whitespace() || c.is_control()) =>
            {
                Ok(Self::Ssh(alias))
            }
            TargetFields::Ssh(_) => Err("ssh 必须是非空的 SSH 别名"),
        }
    }
}

impl From<Target> for TargetFields {
    fn from(target: Target) -> Self {
        match target {
            Target::Local => Self::Local(true),
            Target::Ssh(alias) => Self::Ssh(alias),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputPolicy {
    Sdk,
}

#[derive(Debug, Deserialize)]
#[serde(from = "ProfileDefinition")]
pub struct ProfileEntry {
    pub entry: PathBuf,
    pub inputs: Option<InputPolicy>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ProfileDefinition {
    Path(PathBuf),
    Detailed(Object<ProfileFields>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFields {
    entry: PathBuf,
    inputs: Option<InputPolicy>,
}

impl From<ProfileDefinition> for ProfileEntry {
    fn from(definition: ProfileDefinition) -> Self {
        match definition {
            ProfileDefinition::Path(entry) => Self {
                entry,
                inputs: None,
            },
            ProfileDefinition::Detailed(Object(ProfileFields { entry, inputs })) => {
                Self { entry, inputs }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModuleEntry {
    pub schema: PathBuf,
    pub expand: PathBuf,
    pub reads_options: Option<Vec<String>>,
    pub inputs: Option<InputPolicy>,
}

impl Manifest {
    pub(super) fn parse(path: &Path, contents: &str) -> Result<Self, RepoError> {
        let invalid = |message| RepoError::InvalidManifest {
            path: path.to_owned(),
            message,
        };
        let Object(header) = serde_json::from_str::<Object<ManifestHeader>>(contents)
            .map_err(|err| invalid(err.to_string()))?;
        if header.schema_version != SCHEMA_VERSION {
            return Err(invalid(format!(
                "不支持的 schemaVersion {}（期望 {SCHEMA_VERSION}）",
                header.schema_version
            )));
        }

        let mut decoder = serde_json::Deserializer::from_str(contents);
        let manifest: Self = serde_path_to_error::deserialize(&mut decoder)
            .map_err(|err| invalid(err.to_string()))?;
        decoder.end().map_err(|err| invalid(err.to_string()))?;
        manifest.validate().map_err(invalid)?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), String> {
        for (name, host) in &self.hosts {
            validate_name("hosts", name)?;
            validate_path(&format!("hosts[{name:?}].entry"), &host.entry)?;
        }
        for (name, profile) in &self.profiles {
            validate_name("profiles", name)?;
            validate_path(&format!("profiles[{name:?}].entry"), &profile.entry)?;
        }
        for (name, module) in &self.modules {
            validate_name("modules", name)?;
            validate_path(&format!("modules[{name:?}].schema"), &module.schema)?;
            validate_path(&format!("modules[{name:?}].expand"), &module.expand)?;
            for (index, option) in module.reads_options.iter().flatten().enumerate() {
                if option.split('.').any(str::is_empty)
                    || option.chars().any(|c| c.is_whitespace() || c.is_control())
                {
                    return Err(format!(
                        "modules[{name:?}].readsOptions[{index}] 需要有效的 option 路径：{option:?}"
                    ));
                }
            }
        }
        Ok(())
    }
}

fn validate_name(field: &str, name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.chars().any(char::is_control) {
        return Err(format!("{field} 包含无效名称 {name:?}"));
    }
    Ok(())
}

fn validate_path(field: &str, path: &Path) -> Result<(), String> {
    let invalid = || format!("{field} 需要仓库内的相对文件路径：{path:?}");
    let mut depth = 0;
    for component in path.components() {
        match component {
            Component::Normal(value) if !value.as_encoded_bytes().contains(&0) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            _ => return Err(invalid()),
        }
    }
    if depth == 0 {
        return Err(invalid());
    }
    Ok(())
}

fn unique_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    deserialize_named_map(deserializer, std::convert::identity)
}

fn unique_object_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    deserialize_named_map(deserializer, |Object(value)| value)
}

fn deserialize_named_map<'de, D, Raw, T>(
    deserializer: D,
    into_entry: fn(Raw) -> T,
) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    Raw: Deserialize<'de>,
{
    struct UniqueMap<Raw, T>(fn(Raw) -> T);

    impl<'de, Raw: Deserialize<'de>, T> Visitor<'de> for UniqueMap<Raw, T> {
        type Value = BTreeMap<String, T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map with unique names")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry()? {
                match values.entry(key) {
                    btree_map::Entry::Vacant(entry) => {
                        entry.insert((self.0)(value));
                    }
                    btree_map::Entry::Occupied(entry) => {
                        return Err(de::Error::custom(format!("重复名称 {:?}", entry.key())));
                    }
                }
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(UniqueMap(into_entry))
}

// Serde 的结构体派生也接受按字段顺序排列的数组，清单只采用对象写法。
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = Object<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Object)
            }
        }

        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}
