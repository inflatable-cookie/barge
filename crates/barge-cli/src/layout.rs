use std::path::Path;

use barge::{Absent, Barge, Class, Error, ErrorKind, Placement, Result};
use serde_json::json;

use crate::args::{expect_no_positionals, parse_absent, Options};
use crate::output::document;

pub(crate) fn layout(arguments: &[String], json: bool) -> Result<()> {
    let (command, rest) = arguments
        .split_first()
        .ok_or_else(|| Error::usage("layout needs show or set"))?;
    match command.as_str() {
        "show" => {
            let options = Options::parse(rest, &[], &[])?;
            expect_no_positionals(&options)?;
            let barge = Barge::open_read_only()?;
            let layout = barge.layout()?;
            let disagreements = barge.layout_disagreements()?;
            if json {
                document(
                    "barge.layout-show/1",
                    vec![
                        ("layout", json!(layout)),
                        ("disagreements", json!(disagreements)),
                    ],
                )?;
            } else {
                for (class, placement) in &layout.classes {
                    println!(
                        "{}: {} / {}",
                        class.name(),
                        placement.volume,
                        placement.path
                    );
                }
                for name in disagreements {
                    eprintln!("barge: layout record on {name} disagrees with account");
                }
            }
            Ok(())
        }
        "set" => {
            let options = Options::parse(rest, &["volume", "path", "absent"], &[])?;
            let class = options.single("layout set", "class")?.parse::<Class>()?;
            let volume = options.required("volume")?;
            let path = options.required("path")?;
            // Every argument is validated before the account is opened, so a
            // refused set creates no state.
            let absent = match options.get("absent") {
                Some(value) => Some(parse_absent(&value)?),
                None => None,
            };
            let barge = Barge::open()?;
            let absent = match absent {
                Some(absent) => absent,
                None => barge
                    .layout()?
                    .classes
                    .get(&class)
                    .map(|placement| placement.absent.clone())
                    .unwrap_or(Absent::Fail),
            };
            let placement = Placement {
                volume,
                path,
                absent,
            };
            let result = barge.set_layout(class, placement)?;
            if json {
                document("barge.layout-updated/1", vec![("layout", json!(result))])?;
            } else {
                println!("updated {} layout", class.name());
            }
            Ok(())
        }
        _ => Err(Error::usage("layout needs show or set")),
    }
}

pub(crate) fn resource_path(arguments: &[String], json: bool) -> Result<()> {
    let options = Options::parse(arguments, &["project"], &[])?;
    let class = options.single("path", "class")?.parse::<Class>()?;
    let resolved =
        Barge::open_read_only()?.resource_path(class, options.get("project").as_deref())?;
    if resolved.fallback {
        eprintln!("barge: using fallback for {}", class.name());
    }
    if json {
        document(
            "barge.path/1",
            vec![
                ("path", json!(resolved.path)),
                ("fallback", json!(resolved.fallback)),
            ],
        )?;
    } else {
        println!("{}", resolved.path.display());
    }
    Ok(())
}

pub(crate) fn resource_env(arguments: &[String]) -> Result<()> {
    let options = Options::parse(arguments, &["shell"], &[])?;
    expect_no_positionals(&options)?;
    let shell = options.get("shell").unwrap_or_else(|| "zsh".to_string());
    if !["zsh", "bash", "fish"].contains(&shell.as_str()) {
        return Err(Error::usage("shell must be zsh, bash or fish"));
    }
    let barge = Barge::open_read_only()?;
    match barge.resource_path(Class::Scratch, None) {
        Ok(resolved) => {
            let assignment = shell_assignment(&shell, "BARGE_SCRATCH", &resolved.path)?;
            if resolved.fallback {
                eprintln!("barge: using fallback for scratch");
            }
            println!("{assignment}");
        }
        Err(error) if matches!(error.kind(), ErrorKind::Refused | ErrorKind::NotFound) => {
            eprintln!("barge: {}", error.message())
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Render one shell assignment, quoting paths without invoking a shell.
pub(crate) fn shell_assignment(shell: &str, name: &str, value: &Path) -> Result<String> {
    let value = value
        .to_str()
        .ok_or_else(|| Error::usage("resource path is not valid UTF-8"))?;
    match shell {
        "bash" | "zsh" => Ok(format!("export {name}='{}'", value.replace('\'', "'\\''"))),
        "fish" => Ok(format!(
            "set -gx {name} '{}';",
            value.replace('\\', "\\\\").replace('\'', "\\'")
        )),
        _ => Err(Error::usage("shell must be zsh, bash or fish")),
    }
}

#[cfg(test)]
mod tests {
    use super::shell_assignment;
    use std::path::Path;

    #[test]
    fn shell_assignments_quote_paths() {
        let path = Path::new("/some path/it's here");
        assert_eq!(
            shell_assignment("bash", "BARGE_SCRATCH", path).unwrap(),
            "export BARGE_SCRATCH='/some path/it'\\''s here'"
        );
        assert_eq!(
            shell_assignment("fish", "BARGE_SCRATCH", path).unwrap(),
            "set -gx BARGE_SCRATCH '/some path/it\\'s here';"
        );
    }
}
