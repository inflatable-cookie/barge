//! How a machine's state reads in the window. The states and the offers come
//! from the Rust state model (`machine_state.rs`); this file only words them.
//! Keep the words in step with `machine_state::label`, which the menu uses.

import type { MachineStatus, Offer } from "./types";

export type Tone = "neutral" | "info" | "success" | "warning" | "danger" | "pending";

export function countdownSeconds(deadline: number, now: number): number {
  return Math.max(0, Math.ceil((deadline - now) / 1000));
}

export function stateLabel(status: MachineStatus, now: number): string {
  switch (status.phase) {
    case "offline": return "Offline";
    case "checking": return "Checking…";
    case "waking": return "Waking…";
    case "awake": return "Awake";
    case "unknown": return "Unknown";
    case "did_not_wake": return "Didn't wake";
    case "still_on": return "Still on";
    case "restarting": return "Restarting…";
    case "going_down": return status.action === "shutdown" ? "Shutting down…" : "Going to sleep…";
    case "pending": {
      const verb = status.action === "restart" ? "Restarting" : status.action === "shutdown" ? "Shutting down" : "Sleeping";
      const seconds = status.deadline_unix_ms === null ? 0 : countdownSeconds(status.deadline_unix_ms, now);
      return `${verb} in ${seconds} s`;
    }
  }
}

export function tone(status: MachineStatus): Tone {
  switch (status.phase) {
    case "awake": return "success";
    case "waking":
    case "pending":
    case "going_down":
    case "restarting": return "pending";
    case "did_not_wake":
    case "still_on": return "warning";
    default: return "neutral";
  }
}

export function offerLabel(offer: Offer): string {
  switch (offer) {
    case "wake": return "Wake";
    case "sleep": return "Sleep";
    case "restart": return "Restart…";
    case "shutdown": return "Shut down…";
    case "cancel": return "Cancel";
    case "moonlight": return "Open Moonlight";
  }
}

/** The one most likely action, shown prominently: Wake, Sleep or Cancel. */
export function primaryOffer(status: MachineStatus): Offer | null {
  return (["wake", "sleep", "cancel"] as const).find(offer => status.offers.includes(offer)) ?? null;
}

/** Restart and shut down name the machine and ask first. */
export function needsConfirmation(offer: Offer): boolean {
  return offer === "restart" || offer === "shutdown";
}

/**
 * How long ago an observation was, on the app's own clock. A last observation
 * is never in the future: if the clocks disagree slightly, it reads "now".
 * (Poodle's `TimeAgo` keeps a private clock that ticks every 30 s, so a fresh
 * poll read as "in 5s"; this follows the window's one-second clock instead.)
 */
export function observedAge(at: number, now: number): string {
  const seconds = Math.floor(Math.max(0, now - at) / 1000);
  if (seconds < 5) return "now";
  if (seconds < 60) return `${seconds} s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  return `${Math.floor(hours / 24)} d ago`;
}

/** The power button shows an action's icon, even while it waits on one. */
export type PowerIcon = Exclude<Offer, "moonlight">;

/** The card's large round button, in the avatar position. */
export interface PowerButton {
  /** What a press sends, or null while there is nothing to press. */
  offer: Offer | null;
  icon: PowerIcon;
  label: string;
  tooltip: string;
}

/**
 * Wake when offline, Sleep when awake, Cancel while a countdown runs.
 * Otherwise the machine is waiting (or unknown): the button shows what is
 * happening and can't be pressed.
 */
export function powerButton(status: MachineStatus, now: number): PowerButton {
  const offer = primaryOffer(status);
  if (offer === "cancel") {
    const action = status.action === "restart" ? "restart" : status.action === "shutdown" ? "shutdown" : "sleep";
    return { offer, icon: "cancel", label: "Cancel", tooltip: `Cancel the ${action === "shutdown" ? "shut down" : action}` };
  }
  if (offer === "wake") return { offer, icon: "wake", label: "Wake", tooltip: "Wake" };
  if (offer === "sleep") return { offer, icon: "sleep", label: "Sleep", tooltip: "Sleep" };
  const label = stateLabel(status, now);
  const icon: PowerIcon = status.phase === "restarting" ? "restart"
    : status.phase === "going_down" ? (status.action === "shutdown" ? "shutdown" : "sleep")
    : "wake";
  return { offer: null, icon, label, tooltip: label };
}

/** The smaller actions beside the power button: everything else the state offers. */
export function secondaryOffers(status: MachineStatus): Offer[] {
  const primary = primaryOffer(status);
  return status.offers.filter(offer => offer !== primary);
}
