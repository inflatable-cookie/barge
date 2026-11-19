Final docs and app quality pass: fix the `rev` pinning claim in app.md, add the
`unsupported` error kind to the consumption contract, let architecture own the
exit codes and machines.md own the LAN threat model with links from the other
file, and link the quality contract from the docs index. The app keeps the
full Longhorn preferences shape with the reason recorded in app.md, uses one
template tray image for every display, and splits the renderer into shell,
Machines, Storage, Settings and Activity components. Longhorn Git dependencies
move to `[workspace.dependencies]` at one `rev`, and the app crate gains a
description.
