/**
 * Icons Poodle's bundled set lacks, as Lucide nodes (ISC licence, the same
 * family Poodle bundles), so they match its stroke and weight.
 */
import type { IconProp } from "@inflatable-cookie/poodle-svelte";
import type { Offer } from "./types";

const power: IconProp = [
  ["path", { d: "M12 2v10" }],
  ["path", { d: "M18.4 6.6a9 9 0 1 1-12.77.04" }],
];
const moon: IconProp = [["path", { d: "M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z" }]];
const powerOff: IconProp = [
  ["path", { d: "M18.36 6.64A9 9 0 0 1 20.77 15" }],
  ["path", { d: "M6.16 6.16a9 9 0 1 0 12.68 12.68" }],
  ["path", { d: "M12 2v4" }],
  ["path", { d: "m2 2 20 20" }],
];

/** One icon per action, on the power button and the small buttons alike. */
export const offerIcons: Record<Offer, IconProp> = {
  wake: power,
  sleep: moon,
  restart: "refresh-cw",
  shutdown: powerOff,
  cancel: "x",
  moonlight: "monitor-play",
};
