use dek_core::{HostConfig, Origin};

const PREFIX: &str = "/cached-origin/";

/// Stores source locations separately from the reusable result.
#[derive(Default)]
pub(super) struct Sources(Vec<Origin>);

impl Sources {
    pub(super) fn reference(&mut self, origin: &Origin) -> Origin {
        let index = self.0.len();
        self.0.push(origin.clone());
        Origin::Json {
            file: origin.file().into(),
            pointer: format!("{PREFIX}{index}")
                .parse()
                .expect("a generated JSON Pointer"),
        }
    }

    pub(super) fn resolve(&self, origin: &mut Origin) {
        let Origin::Json { file, pointer } = origin else {
            return;
        };
        if let Some(index) = pointer
            .as_str()
            .strip_prefix(PREFIX)
            .and_then(|value| value.parse::<usize>().ok())
            && let Some(current) = self.0.get(index)
            && current.file() == file
        {
            *origin = current.clone();
        }
    }
}

pub(super) fn map_config_origins(config: &mut HostConfig, mut map: impl FnMut(&mut Origin)) {
    for option in config.options.values_mut() {
        for definition in option
            .effective_definitions
            .iter_mut()
            .chain(&mut option.overridden_definitions)
        {
            map(&mut definition.origin);
        }
    }
}

pub(super) fn file_origin(origin: &mut Origin) {
    *origin = Origin::Json {
        file: origin.file().into(),
        pointer: "".parse().expect("root JSON Pointer"),
    };
}
