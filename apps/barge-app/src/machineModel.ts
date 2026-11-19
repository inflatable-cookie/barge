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
