# Barge app

Status: Machines built; the remaining first-release capabilities below are planned.

The Barge app is a macOS menu-bar app for the two things an operator checks
most often: the machines Barge controls, and the storage it manages. It sits
next to the CLI and never replaces it. Everything the app does, the CLI can
also do, and both go through the same library code.

## Decisions (Tom, 2026-11-18)

- **Stack:** Tauri, with [Poodle](https://github.com/inflatable-cookie/poodle)
  (`poodle-core` and `poodle-svelte`) for the UI and
  [Longhorn](https://github.com/inflatable-cookie/longhorn) for the app
  backbone and services. Visual tokens, themes and components come from
  Poodle; Barge defines no visual system of its own.
- **Form:** a menu-bar app, with a window opened on demand.
- **First release:** Machines and Storage.
- **Agent control:** Longhorn's agent-control surface is useful for testing
  the UI, but not needed to use Barge. It's added when there's time.

The first design pass (barge PR #81) contributed the interaction rules below.
Its visual system is replaced by Poodle.

## Form

- **Menu-bar icon:** a monochrome template image opens a native menu with this
  Mac's state and storage summary, registered machines and their power actions,
  Moonlight, Open Barge, Settings and Quit. The menu updates while Barge runs.
- **The app is never reachable only through the menu bar.** macOS parks menu-bar
  items off-screen when the space beside the notch is full, so:
  - the window opens when the app launches, controlled by the "Show window at
    launch" setting (on by default);
  - launching the app again while it runs reopens the window (macOS
    `RunEvent::Reopen`);
- **Window on demand:** also opened from the menu. The app shows a Dock icon
  only while the window is open (`ActivationPolicy::Accessory` otherwise).
- **Launch at login** is a setting, off by default.

## First release scope

### Machines

- **List:** this Mac always appears first without registration, followed by each registered machine, with its observed state and age: online,
  checking, waking, unreachable, or pending an action.
- **Detail:**
  - connection: LAN or Tailscale mode, address, MAC;
  - the last observation;
  - power controls and any pending action with its countdown and Cancel;
  - Moonlight launch, which the MoonDeck Remote user relies on.
- **Add, pair and remove a machine:**
  - LAN mode: paste the key that `barged pair` printed;
  - Tailscale mode: enter the tailnet name.

  The app writes the same records the CLI does. See [Keys](#keys).

### Storage

- **Volumes:** identity, present, absent or mismatched, free space against
  reserve and floor.
- **Layout:** each class, its volume and folder, and whether the two layout
  copies agree.
- **Bindings:** each binding with its doctor state (ok, drifted with observed
  and expected values, missing, unavailable).
- **Survey:** top-level items classified as class folders, area paths,
  Barge's own, system, or unknown. Sizes are walked only on request.
- **Moves:** build a `barge move` plan from a survey item or a class, review
  it (rename, bindings, layout change, consumers to stop, manual steps), and
  apply it.
  - Applying needs an explicit confirmation naming the move, plus the
    `--confirm-stopped` acknowledgement when bound consumers exist.
  - The plan the operator reviewed is exactly the plan applied, as in the CLI.
- **Prune:** dry-run results only in the first release. Applying a prune
  stays a CLI action until there's a reason to add it.

### Activity

A log kept by the app of the actions taken through it: time, target, result,
and any error. It never invents history. Actions taken through the CLI don't
appear, which the view says plainly.

### Settings

Launch at login, show window at launch, the refresh interval, notifications on
or off per kind, and the theme (Poodle's system, light or dark).

### Out of the first release

Worktrees (with the worktree lane), agent control, app updates, applying
prune, and machine metrics beyond power state.

## Interaction rules

- **Show what was observed, and when.** Never infer a current state. An
  unreachable daemon is "unreachable", with the last observation and its age,
  never "asleep".
- **The daemon owns countdowns.** On reconnect or reopen, the app reads the
  pending action and its deadline from `barged`; it never restarts a local
  timer. While an action is pending, a second action is unavailable and
  Cancel is always visible. If the connection drops mid-countdown, say that a
  cancel can't be confirmed until it's back.
- **Confirmation names the target.** Restart, shutdown and applying a move
  need a confirmation that names the machine or move and what it interrupts.
  Escape cancels it.
- **Absent is not empty.** An absent volume shows as absent, with its data
  unavailable, never as an empty list. A volume mismatch is its own state.
- **Errors stay visible,** with what to do next. A failed or timed-out cancel
  is never shown as success.
- **Accessibility follows Poodle's contracts:** keyboard access, focus,
  announcements without per-tick noise, reduced motion, and system text size.

## Architecture

```
Svelte + Poodle (renderer)
        │  typed Tauri commands, registered by qualified path
Tauri Rust host (apps/barge-app/src-tauri)
  ├── barge          storage: read-only opener for reads; a writable
  │                  open only inside an explicit action
  ├── barge-machine  registry, Wake-on-LAN, barged client
  └── Longhorn       config, settings, notifications, operation,
                     credential keyring
```

- **Library, not CLI:** the Rust side calls `barge` and `barge-machine`
  directly. It never shells out to the CLI or parses CLI output.
- **Reads use the read-only account opener.** A writable open happens only
  inside an action the operator started (pair, move apply), and is released
  when the action ends. The app holds no Barge locks or holds while idle.
- **Refresh:**
  - machine state is polled while the app runs, at the refresh interval
    (default 10 s), and once on open;
  - the menu's local storage summary refreshes at that interval; the Storage
    view reads on open and on Refresh;
  - survey sizes and doctor's binding checks run on request, never in the
    background. The menu shows the last doctor result and its age; its Refresh
    item runs doctor.
- **Longhorn crates** (the "minimal" shape plus services):
  - `longhorn-core`, `longhorn-config`, `longhorn-settings`,
    `longhorn-settings-config`, `longhorn-tauri-config` and
    `longhorn-tauri-settings` for app preferences;
  - `longhorn-notifications` and `longhorn-tauri-notifications` for results:
    a countdown fired, a move finished, doctor found drift;
  - `longhorn-operation` and `longhorn-tauri-operation` for a running move
    apply;
  - `longhorn-credential-keyring` if keys move to the Keychain (see
    [Keys](#keys)).

  Rust crates are taken by git tag; TypeScript packages are pinned in
  `apps/barge-app/bun.lock`, which bun installs and the Effigy selectors use.
  The scaffold's renderer talks to its own typed commands, so it does not import
  Longhorn's TypeScript packages yet; they return when the Longhorn settings
  surface or agent control is rendered.
- **The menu bar** uses Tauri's tray API. Longhorn has no menu-bar system
  today.
- **Poodle components,** a first mapping:
  - `SidebarNav` for the window's navigation (Machines, Storage, Activity,
    Settings);
  - `StatusIndicator` and `TimeAgo` for observed state;
  - `ConfirmAction` and `AlertDialog` for confirmations;
  - `Progress` for countdowns and move progress;
  - `MetricTile` and `Meter` for space;
  - `DataTable` for survey and bindings;
  - `Callout` and `RemediationBanner` for doctor findings;
  - `EmptyState`, `DetailShell` and `SettingsShell`;
  - `ToastStack` for transient results;

## Keys

LAN mode stores each machine's key owner-only in Barge's account state
(barge#081), where the CLI reads it. The app must use the same key. Two
options:
- **Same file (first release):** the app reads and writes the file the CLI
  uses. One source of truth, and nothing new.
- **Keychain:** move key storage in `barge-machine` to the macOS Keychain, for
  the CLI and the app alike, perhaps through `longhorn-credential-keyring`.
  That's better at rest, but it's a change to `barge-machine` and its tests,
  so it gets its own brief.

Recommended: the same file now, and the Keychain as a later brief, so the two
interfaces never disagree.

## Repository layout

- `apps/barge-app/`: the Tauri app; Svelte renderer, `src-tauri` and
  `bun.lock`.
- `src-tauri` is a workspace member, so the workspace checks cover it. Oxide
  links only `crates/barge`, whose dependency tree must not change because of
  the app.

## Running it

`effigy app:dev` from the repository root installs the renderer's dependencies
from `bun.lock` and starts the app; it is the only command needed. The window
opens on launch, so the app is usable even when macOS hides its menu-bar item.

## Testing

- Renderer: Vitest and Svelte Testing Library over every view's states:
  online, unreachable, pending, absent volume, mismatch, drift, and empty.
  `apps/barge-app/fixtures.html?view=<view>&scenario=<scenario>` mounts the real
  renderer against a fake api, so a view's states can be screenshotted without a
  build.
- Rust: command tests against the fake OS identity and a loopback `barged`
  stub, as the crates already do.
- End-to-end through Longhorn's agent-control surface, when it's added.

## Build order

1. **Scaffold (built):** Tauri, the Longhorn minimal shape and Poodle; tray icon,
   window; settings; an empty Machines view with real navigation.
   Tom reviews it running.
2. **Machines (built):** list, detail, power with countdown and cancel, pairing,
   Moonlight and the native tray menu.
3. **Storage, read views:** volumes, layout, bindings with doctor, and survey.
4. **Moves:** plan from survey, review, apply with confirmation, and progress
   through `longhorn-operation`.
5. **Activity and notifications.**

Later: agent control, app updates, Keychain keys, and worktrees.

Instead of a separate visual design pass, Tom reviews the running app after
steps 1 and 2: with Poodle supplying the components, the build is the
quickest honest picture.
