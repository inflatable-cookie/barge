# Barge app design

Barge has no visual system of its own: tokens, themes and components come from
[Poodle](https://github.com/inflatable-cookie/poodle) (`iceberg` light,
`midnight` dark). The window's form, layout and interaction rules are in
[docs/knowledge/app.md](../../docs/knowledge/app.md). Keep to Poodle's
components and semantic tokens; add local CSS only for layout.
