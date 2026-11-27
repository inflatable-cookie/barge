//! Read-only CLI preview of a run request.

use barge::run::{DenyReason, RunPreview, RunRequest};
use barge::{Barge, Error, Result};
use serde_json::json;

use crate::output::document;

pub(crate) fn run(arguments: &[String], json: bool) -> Result<()> {
    let [command] = arguments else {
        return Err(Error::usage(
            "run needs resolve with a JSON request on stdin",
        ));
    };
    if command != "resolve" {
        return Err(Error::usage(format!("unknown run command {command:?}")));
    }
    let request: RunRequest = serde_json::from_reader(std::io::stdin().lock())
        .map_err(|error| Error::usage(format!("cannot read run request from stdin: {error}")))?;
    let preview = Barge::open_read_only()?.run_preview(&request)?;
    if json {
        return document(
            "barge.run-resolution/1",
            vec![("resolution", json!(preview))],
        );
    }
    match preview {
        RunPreview::LocalNative => println!("local native"),
        RunPreview::LocalContainer { profile } => println!("local container profile {profile}"),
        RunPreview::Denied { reason } => match reason {
            DenyReason::NoMatchingPlatform => println!("denied: no matching platform"),
            DenyReason::UnmetCapability => println!("denied: unmet capability"),
            DenyReason::FallbackForbidden => println!("denied: local fallback forbidden"),
            DenyReason::ProfileFull {
                profile,
                active,
                max,
            } => {
                println!("denied: profile {profile} is full ({active}/{max} slots)")
            }
        },
        _ => return Err(Error::unsupported("unrecognised run resolution")),
    }
    Ok(())
}
