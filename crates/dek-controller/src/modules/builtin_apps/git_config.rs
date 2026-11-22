use std::{fmt::Write, slice};

use serde_json::{Map, Value};

pub(super) fn render(sections: &Map<String, Value>) -> Result<Vec<u8>, String> {
    let mut output = String::new();
    for (section, value) in sections {
        if section.is_empty()
            || !section
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(format!("invalid Git section {section:?}"));
        }
        let settings = value
            .as_object()
            .ok_or_else(|| format!("Git section {section:?} expects an object"))?;

        let mut header_written = false;
        for (key, setting) in settings.iter().filter(|(_, setting)| !setting.is_object()) {
            if !header_written {
                writeln!(output, "[{section}]").expect("writing to a string");
                header_written = true;
            }
            write_setting(&mut output, section, key, setting)?;
        }
        for (subsection, values) in settings
            .iter()
            .filter_map(|(name, setting)| setting.as_object().map(|map| (name, map)))
        {
            if subsection.contains(['\n', '\r', '\0']) {
                return Err(format!(
                    "invalid Git subsection {subsection:?} in {section:?}"
                ));
            }
            let quoted = subsection.replace('\\', "\\\\").replace('"', "\\\"");
            writeln!(output, "[{section} \"{quoted}\"]").expect("writing to a string");
            let subsection_path = format!("{section}.{subsection}");
            for (key, setting) in values {
                write_setting(&mut output, &subsection_path, key, setting)?;
            }
        }
    }
    Ok(output.into_bytes())
}

fn write_setting(
    output: &mut String,
    section: &str,
    key: &str,
    value: &Value,
) -> Result<(), String> {
    if !key.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(format!("invalid Git variable {key:?} in {section:?}"));
    }
    let values = value
        .as_array()
        .map_or(slice::from_ref(value), Vec::as_slice);
    for value in values {
        let rendered = match value {
            Value::String(text) => quote(text).ok_or_else(|| {
                format!("invalid control character in Git setting {section}.{key}")
            })?,
            Value::Bool(_) | Value::Number(_) => value.to_string(),
            _ => {
                return Err(format!(
                    "Git setting {section}.{key} expects a string, boolean, number, or list of those values"
                ));
            }
        };
        writeln!(output, "\t{key} = {rendered}").expect("writing to a string");
    }
    Ok(())
}

fn quote(value: &str) -> Option<String> {
    let mut text = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => text.push_str("\\\\"),
            '"' => text.push_str("\\\""),
            '\n' => text.push_str("\\n"),
            '\t' => text.push_str("\\t"),
            '\u{8}' => text.push_str("\\b"),
            '\0' | '\r' => return None,
            character => text.push(character),
        }
    }
    text.push('"');
    Some(text)
}
