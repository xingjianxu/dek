use std::collections::{BTreeMap, BTreeSet};

use dek_core::{
    DesiredResource, HostConfig, ResourceId, SystemdUnitResource, UnitDefinition, UnitId, UnitScope,
};
use serde::Deserialize;
use serde_json::Value;

use super::{
    ModuleOutput, accounts::ManagedAccounts, invalid, member_origins, object, origins, parse, ready,
};

type Directives = BTreeMap<String, Value>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UnitOptions {
    enabled: Option<bool>,
    running: Option<bool>,
    description: Option<String>,
    #[serde(default)]
    unit_config: Directives,
    #[serde(default)]
    service_config: Directives,
    #[serde(default)]
    timer_config: Directives,
    #[serde(default)]
    install_config: Directives,
    wanted_by: Option<Vec<String>>,
    #[serde(default)]
    drop_ins: BTreeMap<String, String>,
}

#[derive(Clone, Copy)]
enum UnitKind {
    Service,
    Timer,
}

impl UnitKind {
    fn suffix(self) -> &'static str {
        match self {
            Self::Service => ".service",
            Self::Timer => ".timer",
        }
    }
}

pub(super) fn expand(
    config: &HostConfig,
    accounts: &ManagedAccounts,
    output: &mut ModuleOutput,
) -> Result<(), String> {
    let mut units = Vec::new();
    for (path, option) in &config.options {
        let Some((scope, kind)) = scope_and_kind(path.as_str()) else {
            continue;
        };
        for (name, value) in object(&option.value, path.as_str(), &origins(option))? {
            let sources = member_origins(option, name);
            let label = format!("{path} unit {name:?}");
            let options: UnitOptions = parse(value, &label, &sources)?;
            let depends_on = account_dependencies(&scope, &options.service_config, accounts);
            let unit = expand_unit(output, scope.clone(), kind, name, options)
                .map_err(|error| invalid(&label, &sources, &error))?;
            units.push((unit, sources, depends_on));
        }
    }

    let managed: BTreeSet<_> = units.iter().map(|(unit, _, _)| unit.unit.clone()).collect();
    for (unit, sources, mut depends_on) in units {
        if let Some(service) = &unit.activates
            && managed.contains(service)
        {
            depends_on.push(ResourceId::SystemdUnit(service.clone()));
        }
        output.resources.push(ready(
            DesiredResource::SystemdUnit(unit),
            sources,
            depends_on,
        ));
    }
    Ok(())
}

fn account_dependencies(
    scope: &UnitScope,
    directives: &Directives,
    accounts: &ManagedAccounts,
) -> Vec<ResourceId> {
    match scope {
        UnitScope::User { user } if accounts.users.contains(user) => {
            vec![ResourceId::User(user.clone())]
        }
        UnitScope::System => {
            let configured = |key| match directives.get(key) {
                Some(Value::Array(values)) => values.last().and_then(Value::as_str),
                Some(value) => value.as_str(),
                None => None,
            };
            let mut dependencies = Vec::new();
            if let Some(user) = configured("User").filter(|name| accounts.users.contains(*name)) {
                dependencies.push(ResourceId::User(user.into()));
            }
            if let Some(group) = configured("Group").filter(|name| accounts.groups.contains(*name))
            {
                dependencies.push(ResourceId::Group(group.into()));
            }
            dependencies
        }
        UnitScope::User { .. } | UnitScope::UserGlobal => Vec::new(),
    }
}

fn scope_and_kind(path: &str) -> Option<(UnitScope, UnitKind)> {
    let (scope, collection) = if let Some(collection) = path.strip_prefix("systemd.user.") {
        (UnitScope::UserGlobal, collection)
    } else if let Some(collection) = path.strip_prefix("systemd.") {
        (UnitScope::System, collection)
    } else {
        let (user, collection) = path.strip_prefix("dek.users.")?.split_once(".systemd.")?;
        if user.contains('.') {
            return None;
        }
        (UnitScope::User { user: user.into() }, collection)
    };
    let kind = match collection {
        "services" => UnitKind::Service,
        "timers" => UnitKind::Timer,
        _ => return None,
    };
    Some((scope, kind))
}

fn expand_unit(
    output: &mut ModuleOutput,
    scope: UnitScope,
    kind: UnitKind,
    name: &str,
    mut options: UnitOptions,
) -> Result<SystemdUnitResource, String> {
    let base = name.strip_suffix(kind.suffix()).ok_or_else(|| {
        format!(
            "expected a full unit name with the {} suffix",
            kind.suffix()
        )
    })?;
    if !valid_file_name(base) {
        return Err(
            "expected a unit name without path separators, spaces or control characters".into(),
        );
    }
    let unit = UnitId {
        scope: scope.clone(),
        name: name.into(),
    };
    let activates = match kind {
        UnitKind::Service => {
            if !options.timer_config.is_empty() {
                return Err("a service cannot specify timerConfig".into());
            }
            None
        }
        UnitKind::Timer => {
            if !options.service_config.is_empty() {
                return Err("a timer cannot specify serviceConfig".into());
            }
            let name = match options.timer_config.get("Unit") {
                Some(Value::String(name))
                    if name.strip_suffix(".service").is_some_and(valid_file_name) =>
                {
                    name.clone()
                }
                Some(_) => return Err("timerConfig.Unit must be a full service name".into()),
                None => format!("{base}.service"),
            };
            Some(UnitId { scope, name })
        }
    };
    if let Some(description) = options.description {
        add_directive(
            &mut options.unit_config,
            "Description",
            Value::from(description),
        )?;
    }
    if let Some(wanted_by) = options.wanted_by {
        add_directive(
            &mut options.install_config,
            "WantedBy",
            Value::from(wanted_by),
        )?;
    }

    let mut text = String::new();
    for (section, directives) in [
        ("Unit", options.unit_config),
        ("Service", options.service_config),
        ("Timer", options.timer_config),
        ("Install", options.install_config),
    ] {
        render_section(&mut text, section, directives)?;
    }

    let content = (!text.is_empty()).then(|| output.add_content(text.into_bytes()));
    let mut drop_ins = BTreeMap::new();
    for (name, text) in options.drop_ins {
        if !name.strip_suffix(".conf").is_some_and(valid_file_name) {
            return Err(format!("invalid systemd drop-in name {name:?}"));
        }
        match kind {
            UnitKind::Timer if contains_assignment(&text, &["Unit"]) => {
                return Err(format!(
                    "timer drop-in {name:?} cannot assign Unit; use timerConfig.Unit"
                ));
            }
            UnitKind::Service
                if unit.scope == UnitScope::System
                    && contains_assignment(&text, &["User", "Group"]) =>
            {
                return Err(format!(
                    "system service drop-in {name:?} cannot assign User or Group; use serviceConfig"
                ));
            }
            _ => {}
        }
        drop_ins.insert(name, output.add_content(text.into_bytes()));
    }

    Ok(SystemdUnitResource {
        unit,
        definition: UnitDefinition { content, drop_ins },
        enabled: options.enabled,
        running: options.running,
        activates,
    })
}

fn contains_assignment(text: &str, keys: &[&str]) -> bool {
    let mut logical = String::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(prefix) = line.strip_suffix('\\') {
            logical.push_str(prefix);
            logical.push(' ');
            continue;
        }
        logical.push_str(line);
        if logical
            .split_once('=')
            .is_some_and(|(name, _)| keys.contains(&name.trim()))
        {
            return true;
        }
        logical.clear();
    }
    false
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\'))
}

fn add_directive(directives: &mut Directives, key: &str, value: Value) -> Result<(), String> {
    if let Some(prior) = directives.get(key) {
        if *prior != value {
            return Err(format!(
                "directive {key} is specified twice with different values"
            ));
        }
    } else {
        directives.insert(key.into(), value);
    }
    Ok(())
}

fn render_section(text: &mut String, section: &str, directives: Directives) -> Result<(), String> {
    if directives.is_empty() {
        return Ok(());
    }
    text.push_str(&format!("[{section}]\n"));
    for (key, value) in directives {
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err(format!("invalid systemd directive {section}.{key}"));
        }
        if let Value::Array(values) = value {
            for value in values {
                render_directive(text, section, &key, &value)?;
            }
        } else {
            render_directive(text, section, &key, &value)?;
        }
    }
    text.push('\n');
    Ok(())
}

fn render_directive(
    text: &mut String,
    section: &str,
    key: &str,
    value: &Value,
) -> Result<(), String> {
    let rendered = match value {
        Value::String(s) if s.trim_end().ends_with('\\') => {
            return Err(format!(
                "systemd directive {section}.{key} ends with a continuation backslash"
            ));
        }
        Value::String(s) if !s.contains(['\n', '\r', '\0']) => s.clone(),
        Value::Bool(true) => "yes".into(),
        Value::Bool(false) => "no".into(),
        Value::Number(number) => number.to_string(),
        _ => {
            return Err(format!(
                "systemd directive {section}.{key} expects a single-line scalar or a list of scalars"
            ));
        }
    };
    text.push_str(&format!("{key}={rendered}\n"));
    Ok(())
}
