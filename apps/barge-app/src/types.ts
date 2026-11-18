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
}
