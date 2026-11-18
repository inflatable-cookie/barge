export type Presence = "present" | "absent" | "mismatch";
export interface VolumeSummary { name: string; presence: Presence; free_bytes: number | null }
export type Theme = "system" | "light" | "dark";
export interface Preferences { launch_at_login: boolean; refresh_interval: number; theme: Theme }
export interface AppApi {
  volumes(): Promise<VolumeSummary[]>;
  preferences(): Promise<Preferences>;
  savePreferences(preferences: Preferences): Promise<void>;
  openWindow(): Promise<void>;
  quit(): Promise<void>;
}
