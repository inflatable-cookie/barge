# Desktop UI design

Status: design proposed for Tom's review; no UI is built by this design pass. The [machine behaviour](machines.md#behaviour) and [resource model](resources.md) remain authoritative. The HTML and PNGs in [the design folder](../design/ui/) are static illustrations, not live state or interactive prototypes. Sample names, capacities, times and findings are illustrative.

## Information architecture

The desktop window uses a persistent sidebar and a content pane. Machines comes first; Storage, Worktrees and Activity have reserved places in the full design. The first release exposes Machines only, rather than links to unavailable pages. The Storage mockup illustrates a later release with the full navigation. The menu bar provides daily power controls without opening the window. A selected machine opens its detail in the content pane; navigation back to the overview remains explicit. The Mac is identified as local; remote actions belong to the PC, not the Mac.

- **Machines:** overview of this Mac and registered PCs, each with a textual state and observation age. Detail shows connection, last observation, power controls, pending action and Moonlight. Refresh updates the observation. Registration and connection details provide name, address, MAC and broadcast configuration; setup explains the daemon's Tailscale allowlist. There is no Buddy PIN pairing: access is determined by the daemon's allowlist. An empty registry invites the operator to add a machine; invalid or unreadable registry data is an error with a retry and details, never an empty list.
- **Storage (later):** volumes and identity, free space and reserve/floor, class placements, bindings and doctor findings. Volume detail exposes survey and class placement. A finding shows observed and expected values and a route to review it. A move plan shows source, destination, affected bindings and exact proposed changes before explicit approval; it does not imply an unapproved move has happened. Empty state offers registration; an absent drive is named as absent, with unavailable data and any explicitly configured fallback identified. A volume identity mismatch is distinct from absence and is never treated as the expected drive.
- **Worktrees (later):** pools by location, leases and holders, and warm/available state. An empty state explains that no pools are registered; an unavailable volume leaves its pools unavailable, not empty.
- **Activity:** recent actions and results, including failures and pending actions, with time and target. An empty state says there are no recent actions; loading and retrieval errors are distinct. The first-release implementation should only show activity where backed by actual data, not invent history.

Long names and errors wrap. On narrow windows the sidebar may collapse to labelled navigation and content stacks; controls remain reachable by keyboard and scrolling. No responsive state hides a pending action or its cancel control.

## Machine interactions

Show the last observed state and its age, not an inferred current state. Checking, online, waking, unreachable and pending are distinct textual states. If the daemon is unreachable, say so and retain the last observation with its age; it is not evidence that the PC is asleep. Offer retry and Wake when configured; wake is a LAN broadcast and may not work away from home. Disable unavailable remote power and Moonlight actions with an explanation. Failure to send or cancel remains visible with recovery; a refused or timed-out cancellation must not be presented as success.

Sleep starts a 15-second countdown by default. Restart and shutdown first require a confirmation naming the target and interruption, then use the same countdown. While pending, show the action, remaining time and an action-specific Cancel control until it fires. The daemon's deadline is authoritative; on reconnect or reopening either surface, retrieve the pending state rather than restarting a local timer. A second action is unavailable while one is pending. If the connection is lost during the countdown, show the last known pending action and age, and explain that cancellation cannot be confirmed until reconnected. Confirmation traps focus, offers Cancel and Escape, and returns focus to the invoking control.

## Menu bar

A monochrome system template icon opens a compact popover showing the PC's state and observation age. Sleep, Wake, Restart, Shut down and Moonlight follow the same availability, confirmation and countdown rules as the window. A pending action and Cancel take precedence over other power actions. Unreachable retains Wake and retry, with honest last-observed state. Open Barge leads to the full window. Use native menu-bar placement and dismissal; the popover is not a separate source of truth.

## Visual system

A quiet, structured instrument panel: cool neutral surfaces, a restrained blue for selection and primary action, and text labels for every state. Native system sans (`-apple-system`, BlinkMacSystemFont, Segoe UI, sans-serif) keeps the app familiar. The base is 14 px at 1.45 line height. Labels and supporting text are 12 px, row titles 15 px, section headings 17 px, the brand 18 px, machine initials 19 px, dialog headings 20 px, metrics 24 px and page titles and countdowns 28 px. Page titles use 1.2 line height; countdowns use 1. Use tabular numerals for countdowns and measurements. The mockups use spacing of 3, 4, 5, 7, 8, 9, 10, 12, 14, 15, 16, 18, 20, 22, 24, 28, 30, 32, 35, 36, 38, 45 and 56 px, plus a 100 px footer gap in the narrow layout. The progress track is 6 px high. Controls and navigation have 8 px corners, identity tiles 10 px, panels 12 px, dialogs and the popover 14 px, and progress tracks 4 px. The mockups use a 220 px sidebar (150 px at narrow widths), 42 px identity tiles, a 430 px dialog and a 360 px popover; the content pane is capped at 1100 px. The sidebar footer uses remaining flexible space, not a fixed gap. Rows and rules organize lists; panels are reserved for meaningful grouped states.

| Token | Light | Dark | Use |
| --- | --- | --- | --- |
| canvas | `#ffffff` | `#141c27` | main background |
| sidebar | `#f2f4f7` | `#101721` | navigation |
| surface | `#ffffff` | `#202b39` | controls and dialog |
| ink | `#182432` | `#eef3f9` | primary text |
| muted | `#506174` | `#b4c2d1` | secondary text |
| line | `#cbd3dd` | `#536275` | decorative separators |
| control border | `#617183` | `#a9b8c8` | secondary control boundaries |
| soft | `#edf2f8` | `#263647` | state panels |
| selected | `#dce8f8` | `#253e5a` | selected navigation |
| accent / focus | `#174f94` | `#a8ccff` | action, selection, focus |
| on accent | `#ffffff` | `#10263e` | filled action text |
| success | `#176640` | `#8fdbad` | status marker |
| warning | `#80520b` | `#f2cb83` | warning text and marker |
| danger | `#a52e35` | `#ffb3b6` | error text and marker; destructive button uses light danger with white in both themes and the control border in dark |

Component inventory: sidebar navigation, window heading and refresh, machine/volume rows, labelled status marker and age, primary and secondary buttons, destructive confirmation dialog, pending-action panel with countdown and cancel, warning and error notices, data rows, empty states, and menu-bar popover. Use 3 px focus outlines with 3 px offset. Hover, pressed, disabled, loading and error states must be distinguishable without colour alone; disabled controls retain readable labels and explain why. Announce meaningful state changes and errors to assistive technology without announcing every countdown tick. All actions have accessible names, adequate targets, logical focus order and keyboard access. Support system text enlargement and avoid clipping at zoom. Respect system light/dark appearance and reduced motion. State transitions may use a 150 ms ease-out; reduced motion removes transitions. No animation is needed for a countdown to communicate urgency.

Contrast checked using WCAG relative luminance: ink and muted on canvas and soft; accent on canvas and selected; on-accent on accent; warning on soft; white on destructive button, in both appearances. All these text pairs meet AA (4.5:1 or greater); state is also expressed in words. Secondary control borders are checked against canvas, surface and soft panel backgrounds in both themes and exceed 3:1. The dark destructive button uses the control border against its red fill (3.41:1) and dialog surface (7.08:1). Focus indicators must achieve 3:1 against adjacent surfaces in the implementation, including each control state.

## Data boundary

The Tauri Rust side calls the `barge` library for storage and `barge-machine` for registry, wake and daemon operations. It exposes a narrow typed interface to the UI; it never shells out to the CLI or parses CLI output. Reads report errors and observation times rather than replacing unknown data with defaults. Storage remains daemon-free, and the UI is not a dependency of consumers of the library.
