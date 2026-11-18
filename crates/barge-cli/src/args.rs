use std::collections::HashMap;
use std::path::PathBuf;

use barge::{Absent, Error, Result};

pub(crate) fn parse_size(text: &str) -> Result<u64> {
    let trimmed = text.trim();
    let split = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (digits, suffix) = trimmed.split_at(split);
    if digits.is_empty() {
        return Err(Error::usage(format!(
            "size {text:?} must start with a number"
        )));
    }
    let value: u64 = digits
        .parse()
        .map_err(|_| Error::usage(format!("size {text:?} has too many digits")))?;
    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024 * 1024 * 1024 * 1024,
        other => {
            return Err(Error::usage(format!(
                "size {text:?} ends with {other:?}; use bytes, KiB, MiB, GiB or TiB"
            )))
        }
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| Error::usage(format!("size {text:?} is too large")))
}

pub(crate) fn parse_bool(field: &str, text: &str) -> Result<bool> {
    match text {
        "true" | "yes" => Ok(true),
        "false" | "no" => Ok(false),
        other => Err(Error::usage(format!(
            "--{field} {other:?} must be true or false"
        ))),
    }
}

pub(crate) fn parse_absent(text: &str) -> Result<Absent> {
    if text == "fail" {
        return Ok(Absent::Fail);
    }
    if let Some(path) = text.strip_prefix("fallback:") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(Error::usage(format!(
                "--absent {text:?} needs an absolute fallback path"
            )));
        }
        return Ok(Absent::Fallback { path });
    }
    Err(Error::usage(
        "--absent must be fail or fallback:<absolute path>",
    ))
}

pub(crate) fn expect_no_positionals(options: &Options) -> Result<()> {
    if let Some(extra) = options.positionals.first() {
        return Err(Error::usage(format!("unexpected argument {extra:?}")));
    }
    Ok(())
}

/// One command's parsed options.
#[derive(Debug, Default)]
pub(crate) struct Options {
    pub(crate) values: HashMap<String, String>,
    pub(crate) flags: Vec<String>,
    pub(crate) positionals: Vec<String>,
}

impl Options {
    pub(crate) fn parse(
        arguments: &[String],
        value_flags: &[&str],
        flag_flags: &[&str],
    ) -> Result<Self> {
        let mut options = Self::default();
        let mut index = 0;
        while index < arguments.len() {
            let argument = &arguments[index];
            index += 1;
            let Some(rest) = argument.strip_prefix("--") else {
                options.positionals.push(argument.clone());
                continue;
            };
            let (name, inline) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (rest, None),
            };
            if flag_flags.contains(&name) {
                if inline.is_some() {
                    return Err(Error::usage(format!("--{name} takes no value")));
                }
                options.flags.push(name.to_string());
                continue;
            }
            if value_flags.contains(&name) {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        let value = arguments
                            .get(index)
                            .cloned()
                            .ok_or_else(|| Error::usage(format!("--{name} needs a value")))?;
                        index += 1;
                        value
                    }
                };
                if options.values.insert(name.to_string(), value).is_some() {
                    return Err(Error::usage(format!("--{name} was given twice")));
                }
                continue;
            }
            return Err(Error::usage(format!("unknown option --{name}")));
        }
        Ok(options)
    }

    pub(crate) fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    pub(crate) fn required(&self, name: &str) -> Result<String> {
        self.get(name)
            .ok_or_else(|| Error::usage(format!("--{name} is required")))
    }

    pub(crate) fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag == name)
    }

    pub(crate) fn single(&self, command: &str, field: &str) -> Result<String> {
        match self.positionals.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(Error::usage(format!("{command} needs <{field}>"))),
            _ => Err(Error::usage(format!("{command} takes one <{field}>"))),
        }
    }

    pub(crate) fn two_positionals(&self, command: &str) -> Result<(String, String)> {
        match self.positionals.as_slice() {
            [area, id] => Ok((area.clone(), id.clone())),
            _ => Err(Error::usage(format!(
                "root {command} needs <namespace>/<area> <id>"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_absent, parse_size, Options};
    use barge::Absent;
    use std::path::PathBuf;

    #[test]
    fn sizes_parse_and_refuse() {
        assert_eq!(parse_size("10GiB").unwrap(), 10 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("1 MiB").unwrap(), 1024 * 1024);
        assert!(parse_size("ten").is_err());
        assert!(parse_size("1PiB").is_err());
    }

    #[test]
    fn options_refuse_unknown_flags() {
        let parsed = Options::parse(
            &["--volume".to_string(), "Builds".to_string()],
            &["volume"],
            &[],
        )
        .unwrap();
        assert_eq!(parsed.get("volume").as_deref(), Some("Builds"));
        assert!(Options::parse(&["--nope".to_string()], &[], &[]).is_err());
    }

    #[test]
    fn absent_policies_parse() {
        assert_eq!(parse_absent("fail").unwrap(), Absent::Fail);
        assert_eq!(
            parse_absent("fallback:/tmp/pool").unwrap(),
            Absent::Fallback {
                path: PathBuf::from("/tmp/pool")
            }
        );
        assert!(parse_absent("fallback:relative").is_err());
    }
}
