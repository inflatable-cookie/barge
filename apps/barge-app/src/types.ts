export type Presence = "present" | "absent" | "mismatch";
export interface VolumeSummary { name: string; presence: Presence; free_bytes: number | null }
export type Theme = "system" | "light" | "dark";
export interface Preferences {
  launch_at_login: boolean;
  refresh_interval: number;
  theme: Theme;
  show_window_at_launch: boolean;
}
export interface AppApi {
  volumes(): Promise<VolumeSummary[]>;
  preferences(): Promise<Preferences>;
  /** Resolves with what was stored: launch at login comes back from the OS. */
  savePreferences(preferences: Preferences): Promise<Preferences>;
  openWindow(): Promise<void>;
  quit(): Promise<void>;
  machines(): Promise<MachineView[]>;
  localMachine(): Promise<LocalMachine>;
  addMachine(input: NewMachine): Promise<void>;
  pairMachine(name: string, key: string): Promise<void>;
  removeMachine(name: string): Promise<void>;
  machineAction(name: string, operation: string): Promise<void>;
  moonlight(): Promise<void>;
}

export type PowerAction = "sleep" | "restart" | "shutdown";
export interface Observation {
  power: "awake" | "asleep" | "unknown";
  observed_at_unix_ms: number;
  pending?: { action: PowerAction; deadline_unix_ms: number } | null;
}
export interface MachineView {
  name: string; hostname: string; mac: string; mode: "lan" | "tailscale";
  address: string; reachable: boolean; observation: Observation | null; error: string | null;
}
export interface LocalMachine {
  name: string; model: string | null; uptime_seconds: number | null;
  volumes: VolumeSummary[]; doctor_findings: number | null; storage_error: string | null;
}
export interface NewMachine {
  name: string; hostname: string; mac: string; tailscale_name: string | null; key: string | null;
}
