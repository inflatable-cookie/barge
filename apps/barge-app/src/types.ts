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
  doctorStatus(): Promise<DoctorStatus>;
  preferences(): Promise<Preferences>;
  /** Resolves with what was stored: launch at login comes back from the OS. */
  savePreferences(preferences: Preferences): Promise<Preferences>;
  openWindow(): Promise<void>;
  quit(): Promise<void>;
  machines(): Promise<MachineView[]>;
  /** Polls every daemon and returns the machines as they now stand. */
  refreshMachines(): Promise<MachineView[]>;
  discoverMachines(): Promise<DiscoveredMachineView[]>;
  startPair(input: PairStartInput): Promise<PairStartView>;
  pollPair(requestId: string): Promise<PairPollView>;
  cancelPair(requestId: string): Promise<void>;
  pendingPairs(): Promise<PendingPairPrompt[]>;
  approvePair(machineName: string, requestId: string): Promise<void>;
  denyPair(machineName: string, requestId: string): Promise<void>;
  machineClients(name: string): Promise<PairedClientView[]>;
  revokeClient(name: string, clientId: string): Promise<void>;
  removeMachine(name: string): Promise<void>;
  /** Records the click in the shared state at once and returns the machines as they now stand. */
  beginMachineAction(name: string, operation: string): Promise<MachineView[]>;
  machineAction(name: string, operation: string): Promise<void>;
  moonlight(): Promise<void>;
}

export interface DoctorStatus { observation: { findings: number; observed_at_unix_ms: number } | null; error: string | null }
export type PowerAction = "sleep" | "restart" | "shutdown";
/** The states `machine_state.rs` returns; the window words them, the menu words them the same way. */
export type Phase =
  | "offline"
  | "checking"
  | "waking"
  | "awake"
  | "pending"
  | "going_down"
  | "restarting"
  | "did_not_wake"
  | "still_on"
  | "unknown";
/** The actions a state offers. Anything not listed is not offered at all. */
export type Offer = "wake" | "sleep" | "restart" | "shutdown" | "cancel" | "moonlight";
export interface MachineStatus {
  phase: Phase;
  action: PowerAction | null;
  /** A countdown's end, on this Mac's clock. */
  deadline_unix_ms: number | null;
  /** When Barge last saw the machine; set only while it is unreachable. */
  last_seen_unix_ms: number | null;
  offers: Offer[];
  /** Whether Barge polls this machine fast until the state settles. */
  waiting: boolean;
}
export interface MachineView {
  name: string;
  hostname: string;
  mac: string;
  mode: "lan" | "tailscale";
  paired: boolean;
  address: string;
  daemon_port: number;
  reachable: boolean;
  error: string | null;
  seen_at_unix_ms: number | null;
  status: MachineStatus;
}
export interface DiscoveredMachineView {
  id: string;
  name: string;
  mode: string;
  protocol_version: string;
  address: string;
  port: number;
}
export interface PairStartInput {
  address: string;
  port: number;
  name: string | null;
  machine_id: string | null;
  existing_name: string | null;
}
export interface PairStartView {
  request_id: string;
  code: string;
  expires_at_unix_ms: number;
  machine_name: string;
}
export type PairOutcome = "pending" | "approved" | "denied" | "expired";
export interface PairPollView {
  status: PairOutcome;
  machine_name: string;
}
export interface PendingPairPrompt {
  machine_name: string;
  request_id: string;
  client_name: string;
  code: string;
  created_at_unix_ms: number;
  expires_at_unix_ms: number;
}
export interface PairedClientView {
  id: string;
  name: string;
  paired_at_unix_ms: number;
  this_client: boolean;
}
