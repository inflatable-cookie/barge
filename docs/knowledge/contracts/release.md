# Release changelog

Barge records each unreleased pull request in one short fragment under
`changelog.d/`, named `<task-or-pr>-<slug>.<type>.md`. Types are `added`,
`changed`, `fixed`, `removed` and `security`. Leave `CHANGELOG.md`'s
`[Unreleased]` section empty; `effigy barge:changelog:check` enforces fragment shape
and this empty section. It does not prove that every pull request added its own
fragment, so the retained reviewer checks that entry on each change.

Before an authorized release, preview the fold with
`effigy barge:changelog:fold --version <x.y.z> [--date YYYY-MM-DD] --dry-run`,
then fold with the same command without `--dry-run`. Release preparation needs the
project's existing explicit release authority. Folding updates `CHANGELOG.md`
and removes the folded fragments; it does not tag or publish a release.

The helper is copied unchanged from Northstar's published PR 81 merge
`9f5d50a15c71f6f3ee269f65ba0222009996af9b`,
`skills/northstar/scripts/changelog.py` (SHA-256
`e28f482e7ba129b4ada9f6a6c806aac3ab5e9de76cfede203c81e4538e9ad387`). Its
synthetic CLI test is based on the same merge's
`skills/northstar/scripts/changelog.test.sh` (SHA-256
`23635f2ba7f50642ba956bac54a046edfd89822ac6e00aba4723227fe557e024`); only
its temporary-directory isolation was adapted to use explicit `TMPDIR` paths.
