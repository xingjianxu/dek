mod git_config;

use dek_core::{
    DesiredResource, EffectiveOption, FileAttributes, NotifyWhen, OptionPath, Origin, PackageId,
    PackageResource, PackageSource, Presence, ResourceId, ServiceEffect, ServiceNotification,
    SystemdUnitResource, UnitDefinition, UnitId, UnitScope,
};

use super::{
    DeclaredResource, ExpandContext, ExpansionError, FileSource, ModuleDefinition, ModuleOutput,
    ModuleRegistry, ResourceDraft, SchemaError, SchemaRegistry,
};

const SCHEMA_FILE: &str = "builtin/apps/options.json";
const SYSTEM_PACKAGES: &str = "environment.systemPackages";
const AUR_PACKAGES: &str = "dek.arch.aurPackages";
const GIT_ENABLED: &str = "programs.git.enable";
const GIT_CONFIG: &str = "programs.git.config";
const OPENSSH_ENABLED: &str = "services.openssh.enable";
const CADDY_ENABLED: &str = "services.caddy.enable";
const CADDY_CONFIG: &str = "services.caddy.configFile";
const CADDYFILE: &str = "/etc/caddy/Caddyfile";

/// Register the static schemas for the built-in package, Git, and service options.
pub fn register_app_schemas(registry: &mut SchemaRegistry) -> Result<(), SchemaError> {
    registry.register_json("builtin.apps", SCHEMA_FILE, include_str!("options.json"))
}

/// Register the Rust expanders for the built-in package, Git, and service options.
pub fn register_app_modules(registry: &mut ModuleRegistry<'_>) -> Result<(), ExpansionError> {
    for (path, source) in [
        (SYSTEM_PACKAGES, PackageSource::Official),
        (AUR_PACKAGES, PackageSource::Aur),
    ] {
        registry.register_builtin(module(path, &[path], None), move |context| {
            expand_packages(context, path, source)
        })?;
    }
    registry.register_builtin(
        module(
            "programs.git",
            &[GIT_ENABLED, GIT_CONFIG],
            Some(GIT_ENABLED),
        ),
        expand_git,
    )?;
    registry.register_builtin(
        module(
            "services.openssh",
            &[OPENSSH_ENABLED],
            Some(OPENSSH_ENABLED),
        ),
        expand_openssh,
    )?;
    registry.register_builtin(
        module(
            "services.caddy",
            &[CADDY_ENABLED, CADDY_CONFIG],
            Some(CADDY_ENABLED),
        ),
        expand_caddy,
    )?;
    Ok(())
}

fn module(name: &str, reads: &[&str], enabled_by: Option<&str>) -> ModuleDefinition {
    ModuleDefinition {
        name: name.into(),
        origin: Origin::Json {
            file: SCHEMA_FILE.into(),
            pointer: "".parse().expect("valid root JSON Pointer"),
        },
        reads_options: Some(reads.iter().copied().map(option_path).collect()),
        enabled_by: enabled_by.map(option_path),
    }
}

fn option_path(path: &str) -> OptionPath {
    path.parse().expect("built-in option path")
}

fn option<'a>(context: &ExpandContext<'a>, path: &str) -> Option<&'a EffectiveOption> {
    context.options.options.get(&option_path(path))
}

fn effective_origins(option: &EffectiveOption) -> Vec<Origin> {
    option
        .effective_definitions
        .iter()
        .map(|definition| definition.origin.clone())
        .collect()
}

fn option_error(path: &str, option: &EffectiveOption, message: &str) -> String {
    let origins = option
        .effective_definitions
        .iter()
        .map(|definition| definition.origin.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if origins.is_empty() {
        format!("{path}: {message}")
    } else {
        format!("{path} at {origins}: {message}")
    }
}

fn package(source: PackageSource, name: &str) -> PackageId {
    PackageId {
        source,
        name: name.into(),
    }
}

fn present_package(package: PackageId, origins: Vec<Origin>) -> DeclaredResource {
    let mut declaration = DeclaredResource::new(ResourceDraft::Ready(DesiredResource::Package(
        PackageResource {
            package,
            state: Presence::Present,
        },
    )));
    declaration.origins = origins;
    declaration
}

fn system_service(unit: UnitId, package: PackageId, origins: Vec<Origin>) -> DeclaredResource {
    let mut declaration = DeclaredResource::new(ResourceDraft::Ready(
        DesiredResource::SystemdUnit(SystemdUnitResource {
            unit,
            definition: UnitDefinition::default(),
            enabled: Some(true),
            running: Some(true),
            activates: None,
        }),
    ));
    declaration.origins = origins;
    declaration.depends_on.push(ResourceId::Package(package));
    declaration
}

fn system_unit(name: &str) -> UnitId {
    UnitId {
        scope: UnitScope::System,
        name: name.into(),
    }
}

fn expand_packages(
    context: &ExpandContext<'_>,
    path: &str,
    source: PackageSource,
) -> Result<ModuleOutput, String> {
    let Some(option) = option(context, path) else {
        return Ok(ModuleOutput::default());
    };
    let names = option
        .value
        .as_array()
        .ok_or_else(|| option_error(path, option, "expected a list of package names"))?;
    let mut output = ModuleOutput::default();
    for value in names {
        let Some(name) = value.as_str() else {
            return Err(option_error(path, option, "expected a string package name"));
        };
        if name.is_empty()
            || name.starts_with(['.', '-'])
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"@._+-".contains(&byte))
        {
            return Err(option_error(
                path,
                option,
                &format!("invalid Arch package name {name:?}"),
            ));
        }
        let origins = option
            .effective_definitions
            .iter()
            .filter(|definition| {
                definition
                    .value
                    .as_array()
                    .is_some_and(|values| values.contains(value))
            })
            .map(|definition| definition.origin.clone())
            .collect();
        output
            .resources
            .push(present_package(package(source, name), origins));
    }
    Ok(output)
}

fn expand_git(context: &ExpandContext<'_>) -> Result<ModuleOutput, String> {
    let enabled = option(context, GIT_ENABLED).expect("registry checked Git enable");
    let package = package(PackageSource::Official, "git");
    let mut output = ModuleOutput {
        resources: vec![present_package(package.clone(), effective_origins(enabled))],
        ..ModuleOutput::default()
    };
    if let Some(config) = option(context, GIT_CONFIG) {
        let sections = config.value.as_object().ok_or_else(|| {
            option_error(GIT_CONFIG, config, "expected an object of Git sections")
        })?;
        let bytes =
            git_config::render(sections).map_err(|err| option_error(GIT_CONFIG, config, &err))?;
        let mut file = DeclaredResource::new(ResourceDraft::File {
            path: "/etc/gitconfig".into(),
            attributes: FileAttributes {
                mode: Some("0644".into()),
                ..FileAttributes::default()
            },
            content: FileSource::Inline(bytes),
        });
        file.origins = effective_origins(config);
        file.depends_on.push(ResourceId::Package(package));
        output.resources.push(file);
    }
    Ok(output)
}

fn expand_openssh(context: &ExpandContext<'_>) -> Result<ModuleOutput, String> {
    let enabled = option(context, OPENSSH_ENABLED).expect("registry checked OpenSSH enable");
    let origins = effective_origins(enabled);
    let package = package(PackageSource::Official, "openssh");
    Ok(ModuleOutput {
        resources: vec![
            present_package(package.clone(), origins.clone()),
            system_service(system_unit("sshd.service"), package, origins),
        ],
        ..ModuleOutput::default()
    })
}

fn expand_caddy(context: &ExpandContext<'_>) -> Result<ModuleOutput, String> {
    let enabled = option(context, CADDY_ENABLED).expect("registry checked Caddy enable");
    let origins = effective_origins(enabled);
    let package = package(PackageSource::Official, "caddy");
    let unit = system_unit("caddy.service");
    let mut output = ModuleOutput {
        resources: vec![present_package(package.clone(), origins.clone())],
        ..ModuleOutput::default()
    };
    let mut service = system_service(unit.clone(), package.clone(), origins);
    if let Some(config) = option(context, CADDY_CONFIG) {
        let path = config.value.as_str().ok_or_else(|| {
            option_error(CADDY_CONFIG, config, "expected a relative source file path")
        })?;
        let Some(origin) = config
            .effective_definitions
            .first()
            .map(|item| item.origin.clone())
        else {
            return Err(option_error(
                CADDY_CONFIG,
                config,
                "a source file needs a declaration location",
            ));
        };
        let mut file = DeclaredResource::new(ResourceDraft::File {
            path: CADDYFILE.into(),
            attributes: FileAttributes::default(),
            content: FileSource::Relative {
                path: path.into(),
                origin,
            },
        });
        file.origins = effective_origins(config);
        file.depends_on.push(ResourceId::Package(package));
        file.notifications.push(ServiceNotification {
            unit,
            effect: ServiceEffect::Reload,
            when: NotifyWhen::ContentChanged,
        });
        service.depends_on.push(ResourceId::Path(CADDYFILE.into()));
        output.resources.push(file);
    }
    output.resources.push(service);
    Ok(output)
}
