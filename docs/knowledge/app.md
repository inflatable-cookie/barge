# Barge app

Status: Machines and Storage read views built; the Machines cards were reworked on Tom's 2026-11-19 feedback and tightened after PR #106 (instant wait, one countdown, one actions menu). Moves, Activity and notifications remain planned.

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

## Decisions (Tom, 2026-11-19)

Tom ran the app after the machines menu (PR #96) and asked for:

- a menu that shows only what the operator can click now, not disabled
  entries for every action;
- no "This Mac" in the menu or the window, and no awake line for this Mac;
- a card per machine in the window, with the actions that apply at that
  moment, in the style of MoonDeck Remote;
- a visible waiting state after a power action, not a return to "unavailable"
  until the next poll.

After reviewing those cards (PR #103) the same day, Tom asked for:

- a profile-card layout: Wake and Sleep as a large icon button in the avatar
  position, everything else to its right, and Restart, Shut down and Moonlight
  as small icon buttons;
- details in a modal or a page, not a revealer, which also broke the grid;
- a roomier grid, so a card isn't squashed;
- no native title bar: floating traffic lights over a top strip that drags the
  window;
- a fix for "Last observation: in 5s".

After using the cards once more (PR #106) Tom asked for:

- the wait to show as soon as a button is clicked: the wake packet went out at
  once, but the card sat unchanged for 3–4 s;
- a countdown shown once, not in the label and again under the button;
- the small action buttons consolidated into one `[...]` menu at the card's top
  right, because a sleeping machine's single icon claimed a whole row;
- the card down to the height of its power button and text.

## Form

- **Menu-bar icon:** a monochrome template image opens a native menu with the
  storage volumes, each registered machine with only the actions its state
  offers, Open Barge, Settings and Quit. The menu updates while Barge runs.
  Its contents are described under [Native menu](#native-menu).
- **The app is never reachable only through the menu bar.** macOS parks menu-bar
  items off-screen when the space beside the notch is full, so:
  - the window opens when the app launches, controlled by the "Show window at
    launch" setting (on by default);
  - launching the app again while it runs reopens the window (macOS
    `RunEvent::Reopen`);
- **Window on demand:** also opened from the menu. The app shows a Dock icon
  only while the window is open (`ActivationPolicy::Accessory` otherwise).
- **Launch at login** is a setting, off by default.

### Window

On macOS the window has no title bar (`titleBarStyle: Overlay`, title
hidden). The native traffic lights float in a 40 px strip along the top, inset
from the corner over the sidebar, and keep their hover glyphs, full screen and
accessibility. The whole strip is the drag region (`data-tauri-drag-region`),
above the sidebar and the content alike; nothing interactive sits in it, and no
content sits under the lights. The "Barge" title sits in the strip, vertically
centred with the traffic lights and just to their right; it is part of the
drag region and takes no pointer events, so the whole strip keeps dragging.
The title's 90 px left margin and the native lights' `(18, 22)` position are
paired in the renderer CSS and Tauri window command.
The sidebar and the content share one top inset below the strip
(`--top-inset`), so the first navigation item and the page heading share a top
line. Other platforms keep their decorations.

### Native menu

Only clickable items, plus a disabled header per machine. Items appear and
disappear as state changes; the countdown text updates every second.

```
Stash · 2,284 GiB free        → opens the Storage view
─────────
bazzite — Awake               (disabled header)
  Sleep
  Restart…
  Shut down…
  Open Moonlight
─────────
Open Barge
Settings…
Quit
```

- Each registered volume is one storage line that opens the Storage view.
  "· N issues" is added only when the last doctor result has findings.
- Offline: the header reads "bazzite — Offline" with Wake beneath it only.
  Waiting states show the header only. Pending shows its countdown and Cancel.
- A chosen action shows in the menu's header at once, before the daemon answers
  and before the next poll: the click is recorded in the shared state the
  window also reads, and is rolled back if the send fails.
- The error row shows only when there is an error.

## First release scope

### Machines

- **Cards:** one profile-style card per registered machine, with its state
  from [machines](machines.md#states): Offline, Waking…, Awake, Sleeping in
  12 s, Going to sleep…, Shutting down…, Restarting…, Didn't wake, Still on, or
  Unknown.
  - **Power button,** in the avatar position: a large round icon button whose
    ring carries the state colour. Wake (power icon) when offline or after a
    failed wake, Sleep (moon) when awake or still on, Cancel (✕) while a
    countdown runs. While a machine waits (Waking, Going to sleep, Shutting
    down, Restarting) the button shows that action, an arc runs round the ring,
    and it can't be pressed.
  - **The click shows at once.** Pressing the power button or a menu action
    shows its wait in the same frame: the click is recorded in the shared Rust
    state before the daemon answers, and a refused send rolls it back with an
    error. The renderer keeps no state model of its own.
  - **One countdown:** it appears in the state label only ("Sleeping in 6 s").
    The button carries the progress in its ring, with no seconds beneath it; a
    waiting state says its progress once the same way.
  - **Beside it:** the name with one `[...]` button at the card's top right, the
    state label, and a muted line with the mode and address
    (`LAN · bazzite.local`). An offline machine folds its last observation into
    the state label (`Offline for 27 s`, `Offline for 5 min`, `Offline for
    2 h`); the card has no separate last-seen line. The `[...]` button opens
    a menu with the rest of what the state offers — Restart, Shut down, Open
    Moonlight — then a separator and Details; with nothing else to offer,
    Details alone. (The native menu-bar menu keeps its ellipses: Restart…,
    Shut down…, Details….) Both the power button
    and the menu button have an accessible label and a tooltip.
  - **Height:** the card is the power button and its text, with no row of icons
    along the bottom.
  - Only the actions the state offers appear. Restart and Shut down name the
    machine and ask first. Moonlight launch, which the MoonDeck Remote user
    relies on, is an action the awake card offers.
- **Grid:** cards are at least 380 px wide, with generous gaps; the grid wraps
  by width, and one machine fills the row rather than sitting cramped.
- **Details dialog:** opened from the card's menu (Details…): connection mode,
  address, port, MAC, last observation and the last error, with Pair… and
  Remove… (Remove asks first). A dialog rather than a page keeps the window to
  one level of navigation.
- **Ages** come from the window's own one-second clock and are never in the
  future: an observation stamped slightly ahead reads "now". (Poodle's
  `TimeAgo` keeps a private clock that ticks every 30 s, so a fresh poll read
  "in 5s".)
- **Empty:** a short line and the Add machine button.
- **Waiting:** after an action, a machine is polled every 2 s until the target
  state is observed or its timeout passes, then at the refresh interval (see
  [machines](machines.md#states)).
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
  unreachable machine reads "Offline for 27 s" from its last observation,
  never "asleep". Ages read in the past tense ("12 s ago", "now") and appear only
  for a stale observation, not on every line.
- **The daemon owns countdowns.** On reconnect or reopen, the app reads the
  pending action and its deadline from `barged`; it never restarts a local
  timer. While an action is pending, a second action is unavailable and
  Cancel is always visible. If the connection drops mid-countdown, say that a
  cancel can't be confirmed until it's back.
- **A click answers before the network does.** The action the operator chose is
  written to the shared state as the click is accepted, so the card and the
  menu show its wait without waiting for the command or the next poll. A
  refused send removes that record and reports the error; the next poll is
  always the truth.
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
  - opening the window first shows what the registry names with the shared
    observations (no network; a machine with no observation yet reads
    Checking…), then polls once on open; while the app runs, machine state is
    polled at the refresh interval (default 10 s), every 2 s while a machine
    is waiting on an action;
  - the menu's local storage summary refreshes at the refresh interval; the
    Storage view reads on open and on Refresh;
  - survey sizes and doctor's binding checks run on request, never in the
    background. Opening the Storage view runs doctor. The menu shows the issue
    count from the last doctor result and nothing else about doctor; it has no
    Refresh item.
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

  Rust crates are pinned by `rev` to an exact commit; TypeScript packages are pinned in
  `apps/barge-app/bun.lock`, which bun installs and the Effigy selectors use.
  The scaffold's renderer talks to its own typed commands, so it does not import
  Longhorn's TypeScript packages yet; they return when the Longhorn settings
  surface or agent control is rendered.
- **Preferences keep the full Longhorn shape now.** Four settings still go
  through Longhorn's config store, settings registry and apply unit rather
  than a plain JSON file, because the registry is already the second writer:
  the settings surface and agent control render through it next, and the
  store gives one atomic handle both writers share, with validation and a
  rollback for the OS login item. A plain file would duplicate that under a
  second authority and need replacing when the settings surface lands.
- **The menu bar** uses Tauri's tray API with one template image: it is
  marked `icon_as_template`, so macOS tints and scales it for any display.
  Longhorn has no menu-bar system today.
- **Poodle components,** a first mapping:
  - `SidebarNav` for the window's navigation (Machines, Storage, Activity,
    Settings);
  - `Card` for a machine, `IconButton` for its power button, `Menu` for its
    actions, and `Dialog` with `DetailItem` for its details;
  - `StatusIndicator` for observed state, and `TimeAgo` outside Machines;
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

- Rust: the state model is a pure function (`machine_state.rs`), tested row by
  row against the states table, each timeout, restart passing through
  unreachable, cancel, and a stale observation. The observations (`machines.rs`)
  keep a click's wait before the daemon answers and roll it back on a refused
  send. The menu's item set per state is tested in `menu_plan.rs`.
- Renderer: Vitest and Svelte Testing Library over each machine card state and
  the actions its menu offers, the wait showing before a deferred command
  resolves, a refused send rolling back, the countdown appearing once, plus
  storage's absent volume, mismatch, drift, and empty.
  `apps/barge-app/fixtures.html?view=<view>&scenario=<scenario>&theme=<light|dark>`
  mounts the real renderer against a fake api, so a view's states can be
  screenshotted without a build. `&details` opens a machine's details;
  `&menu` opens its actions menu; `&chrome=native` drops the macOS top strip.
- Rust: command tests against the fake OS identity and a loopback `barged`
  stub, as the crates already do.
- End-to-end through Longhorn's agent-control surface, when it's added.

## Build order

1. **Scaffold (built):** Tauri, the Longhorn minimal shape and Poodle; tray icon,
   window; settings; an empty Machines view with real navigation.
   Tom reviews it running.
2. **Machines (built, reworked 2026-11-19):** cards, power with countdown and
   cancel, waiting states, pairing, Moonlight and the native tray menu.
3. **Storage, read views:** volumes, layout, bindings with doctor, and survey.
4. **Moves:** plan from survey, review, apply with confirmation, and progress
   through `longhorn-operation`.
5. **Activity and notifications.**

Later: agent control, app updates, Keychain keys, and worktrees.

Instead of a separate visual design pass, Tom reviews the running app after
steps 1 and 2: with Poodle supplying the components, the build is the
quickest honest picture.
