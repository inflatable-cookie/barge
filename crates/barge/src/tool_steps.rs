//! Tool knowledge a move plan carries: the manual step a tool needs after its
//! data has moved.
//!
//! This is a fixed list of data, not an adapter: Barge never calls the tool, it
//! names the step and the plan renders it beside the other manual steps, as the
//! [known link list](crate::known_links) renders the links a move would strand.
//! Adding a tool is adding a row; a tool that needs more than a note is the
//! operator's to handle by hand.

use std::path::Path;

/// One manual step a tool needs when its data moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolStep {
    /// The source's extension the step belongs to, compared case-insensitively.
    pub extension: &'static str,
    /// The step, with `{destination}` replaced by where the data now lives.
    pub step: &'static str,
}

/// The list, as data. Keep it small: every row is a tool Barge tells the operator
/// about after a move.
pub const TOOL_STEPS: &[ToolStep] = &[ToolStep {
    extension: "pvm",
    step: "Re-register in Parallels: prlctl unregister, then prlctl register {destination}",
}];

/// Every step the tools in [`TOOL_STEPS`] need for `source` moving to
/// `destination`, in list order. A source no row names needs no step from here.
pub fn for_move(source: &Path, destination: &Path) -> Vec<String> {
    let mut steps = Vec::new();
    for tool in TOOL_STEPS {
        let matches = source.extension().is_some_and(|extension| {
            extension
                .to_str()
                .is_some_and(|extension| extension.eq_ignore_ascii_case(tool.extension))
        });
        if matches {
            steps.push(
                tool.step
                    .replace("{destination}", &destination.display().to_string()),
            );
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn only_a_named_extension_gets_its_step() {
        let destination = PathBuf::from("/volumes/Builds/machines/Example.pvm");
        let steps = for_move(Path::new("/volumes/Builds/Example.PVM"), &destination);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("/volumes/Builds/machines/Example.pvm"));
        assert!(steps[0].contains("prlctl register"));
        assert!(for_move(Path::new("/volumes/Builds/Example.vmx"), &destination).is_empty());
        assert!(for_move(Path::new("/volumes/Builds/Example"), &destination).is_empty());
    }
}
