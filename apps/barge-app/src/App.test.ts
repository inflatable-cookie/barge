import { render, screen, fireEvent, waitFor, within } from "@testing-library/svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App.svelte";
import { observedAge } from "./machineModel";
import type { AppApi, MachineStatus, MachineView, Preferences, VolumeSummary } from "./types";

const preferences: Preferences = { launch_at_login: false, refresh_interval: 10, theme: "system", show_window_at_launch: true };

beforeEach(() => {
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() }) });
});

afterEach(() => {
  vi.useRealTimers();
});

function fixture(volumes: VolumeSummary[] = []): AppApi {
  return {
    volumes: vi.fn().mockResolvedValue(volumes),
    doctorStatus: vi.fn().mockResolvedValue({ observation: null, error: null }),
    preferences: vi.fn().mockResolvedValue({ ...preferences }),
    savePreferences: vi.fn(async (next: Preferences) => next),
    openWindow: vi.fn().mockResolvedValue(undefined),
    quit: vi.fn().mockResolvedValue(undefined),
    machines: vi.fn().mockResolvedValue([]),
    addMachine: vi.fn(), pairMachine: vi.fn(), removeMachine: vi.fn(), machineAction: vi.fn(), moonlight: vi.fn(),
  };
}

async function settingsView() {
  await fireEvent.click(await screen.findByRole("button", { name: "Settings" }));
}

describe("window navigation", () => {
  it("runs the storage read when opening Storage, not when opening Machines", async () => {
    const api = fixture();
    render(App, { props: { api } });
    await screen.findByRole("heading", { name: "Machines" });
    expect(api.volumes).not.toHaveBeenCalled();
    await fireEvent.click(screen.getByRole("button", { name: "Storage" }));
    await waitFor(() => expect(api.volumes).toHaveBeenCalledOnce());
  });
  it("shows the machines, storage, activity and settings views", async () => {
    const api = fixture();
    render(App, { props: { api } });
    expect(await screen.findByRole("heading", { name: "Machines" })).toBeInTheDocument();
    await fireEvent.click(screen.getByRole("button", { name: "Storage" }));
    expect(await screen.findByText("No volumes registered")).toBeInTheDocument();
    await fireEvent.click(screen.getByRole("button", { name: "Activity" }));
    expect(screen.getByText("No activity yet")).toBeInTheDocument();
    await settingsView();
    expect(screen.getByRole("switch", { name: "Launch at login" })).not.toBeChecked();
    expect(screen.getByRole("switch", { name: "Show window at launch" })).toBeChecked();
    expect(screen.queryByText(/Barge lives in the menu bar/)).not.toBeInTheDocument();
  });

  it("shows a registered but absent volume without calling it empty", async () => {
    render(App, { props: { api: fixture([{ name: "Builds", presence: "absent", free_bytes: null }]) } });
    await fireEvent.click(screen.getByRole("button", { name: "Storage" }));
    expect(await screen.findByText("Builds")).toBeInTheDocument();
    expect(screen.getByText("Absent")).toBeInTheDocument();
    expect(screen.queryByText("No volumes registered")).not.toBeInTheDocument();
  });

  it("shows populated storage", async () => {
    render(App, { props: { api: fixture([{ name: "Builds", presence: "present", free_bytes: 1024 ** 3 }]) } });
    await fireEvent.click(screen.getByRole("button", { name: "Storage" }));
    expect(await screen.findByText("1.0 GiB free")).toBeInTheDocument();
  });

  it("reads storage on open and on Refresh only, never on a timer", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const api = fixture([{ name: "Builds", presence: "present", free_bytes: 1024 ** 3 }]);
    render(App, { props: { api, initialView: "storage" } });
    await vi.waitFor(() => expect(screen.getByText("1.0 GiB free")).toBeInTheDocument());
    expect(api.volumes).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(120_000);
    await fireEvent(window, new Event("focus"));
    expect(api.volumes).toHaveBeenCalledTimes(1);

    await fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await vi.waitFor(() => expect(api.volumes).toHaveBeenCalledTimes(2));
  });

  it("marks the volumes stale with their observation time when a refresh fails", async () => {
    const api = fixture([{ name: "Builds", presence: "present", free_bytes: 1024 ** 3 }]);
    render(App, { props: { api } });
    await fireEvent.click(screen.getByRole("button", { name: "Storage" }));
    expect(await screen.findByText("1.0 GiB free")).toBeInTheDocument();

    vi.mocked(api.volumes).mockRejectedValue(new Error("the volume is not mounted"));
    await fireEvent.click(screen.getByRole("button", { name: "Refresh" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("the volume is not mounted");
    expect(screen.getByText(/showing an earlier observation/)).toBeInTheDocument();
    // The last observation stays on screen, marked, rather than looking current.
    expect(screen.getByText("1.0 GiB free")).toBeInTheDocument();
    expect(document.querySelector("time[datetime]")).not.toBeNull();
  });
});

describe("settings", () => {
  it("says when settings could not be loaded", async () => {
    const api = fixture();
    vi.mocked(api.preferences).mockRejectedValue(new Error("the config store is unreadable"));
    render(App, { props: { api } });
    await settingsView();
    expect(await screen.findByRole("alert")).toHaveTextContent("Could not load settings");
  });

  it("saves a changed setting and adopts what the store reports", async () => {
    const api = fixture();
    vi.mocked(api.savePreferences).mockImplementation(async (next: Preferences) => ({ ...next, launch_at_login: false }));
    render(App, { props: { api } });
    await settingsView();
    const login = await screen.findByRole("switch", { name: "Launch at login" });
    await fireEvent.click(login);
    await waitFor(() => expect(api.savePreferences).toHaveBeenCalled());
    // The OS refused the registration, so the switch shows what it really holds.
    await waitFor(() => expect(screen.getByRole("switch", { name: "Launch at login" })).not.toBeChecked());
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("puts a failed save back and says so", async () => {
    const api = fixture();
    vi.mocked(api.savePreferences).mockRejectedValue(new Error("the config store is read-only"));
    render(App, { props: { api } });
    await settingsView();
    const login = await screen.findByRole("switch", { name: "Launch at login" });
    await fireEvent.click(login);

    expect(await screen.findByRole("alert")).toHaveTextContent("Could not save settings");
    await waitFor(() => expect(screen.getByRole("switch", { name: "Launch at login" })).not.toBeChecked());
    expect(api.savePreferences).toHaveBeenCalledOnce();
  });

  it("changes the theme through Poodle's Select and saves it", async () => {
    const api = fixture();
    render(App, { props: { api } });
    await settingsView();
    const theme = await screen.findByRole("combobox", { name: "Theme" });
    await fireEvent.change(theme, { target: { value: "dark" } });
    await waitFor(() => expect(api.savePreferences).toHaveBeenCalledOnce());
    expect(vi.mocked(api.savePreferences).mock.calls[0][0].theme).toBe("dark");
  });
});


describe("doctor in Storage", () => {
  it("keeps volumes and shows doctor failure and age in Storage, then shows recovery", async () => {
    const api = fixture([{ name: "Builds", presence: "present", free_bytes: 1024 ** 3 }]);
    vi.mocked(api.doctorStatus).mockResolvedValueOnce({ observation: { findings: 0, observed_at_unix_ms: Date.now() - 12_000 }, error: "layout unreadable" }).mockResolvedValue({ observation: { findings: 2, observed_at_unix_ms: Date.now() }, error: null });
    render(App, { props: { api, initialView: "storage" } });
    expect(await screen.findByText("layout unreadable")).toBeInTheDocument();
    expect(screen.getByText("1.0 GiB free")).toBeInTheDocument();
    expect(screen.getByText(/Last doctor result: OK/)).toBeInTheDocument();
    expect(screen.getByText(/earlier result/)).toBeInTheDocument();
    expect(document.querySelector("time[datetime]")).not.toBeNull();
    await fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(await screen.findByText(/Last doctor result: 2 findings/)).toBeInTheDocument();
    expect(screen.queryByText("layout unreadable")).not.toBeInTheDocument();
  });
});

describe("Machines", () => {
  const now = () => Date.now();
  function status(fields: Partial<MachineStatus> & Pick<MachineStatus, "phase">): MachineStatus {
    return { action: null, deadline_unix_ms: null, last_seen_unix_ms: null, offers: [], waiting: false, ...fields };
  }
  const AWAKE = ["Sleep", "Restart…", "Shut down…", "Open Moonlight"];
  function machine(fields: Partial<MachineView> & { status: MachineStatus }): MachineView {
    return {
      name: "bazzite", hostname: "bazzite.local", mac: "00:11:22:33:44:55", mode: "lan", address: "bazzite.local",
      daemon_port: 9777, reachable: true, error: null, seen_at_unix_ms: now(), ...fields,
    };
  }
  const awake = () => machine({ status: status({ phase: "awake", offers: ["sleep", "restart", "shutdown", "moonlight"] }) });
  const pendingSleep = (reachable = true) => machine({
    reachable,
    error: reachable ? null : "Connection timed out",
    status: status({ phase: "pending", action: "sleep", deadline_unix_ms: now() + 12_000, offers: ["cancel"], waiting: true }),
  });

  async function card(name = "bazzite") {
    return screen.findByLabelText(`${name} machine`);
  }

  it("shows an empty state with the add button when nothing is registered", async () => {
    render(App, { props: { api: fixture() } });
    expect(await screen.findByText(/No machines yet/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add machine" })).toBeInTheDocument();
    expect(screen.queryByText("This Mac")).not.toBeInTheDocument();
  });

  it.each([
    ["offline", machine({ reachable: false, error: "Connection timed out", status: status({ phase: "offline", offers: ["wake"], last_seen_unix_ms: now() - 720_000 }) }), "Offline", ["Wake"]],
    ["waking", machine({ reachable: false, status: status({ phase: "waking", waiting: true, last_seen_unix_ms: now() - 720_000 }) }), "Waking…", []],
    ["awake", awake(), "Awake", AWAKE],
    ["pending", pendingSleep(), /Sleeping in \d+ s/, ["Cancel"]],
    ["going down", machine({ status: status({ phase: "going_down", action: "sleep", waiting: true }) }), "Going to sleep…", []],
    ["restarting", machine({ status: status({ phase: "restarting", action: "restart", waiting: true }) }), "Restarting…", []],
    ["didn't wake", machine({ reachable: false, status: status({ phase: "did_not_wake", offers: ["wake"] }) }), "Didn't wake", ["Wake"]],
    ["still on", machine({ status: status({ phase: "still_on", offers: ["sleep", "restart", "shutdown", "moonlight"] }) }), "Still on", AWAKE],
  ])("a card in %s shows its label and only the actions that state offers", async (_, value, label, offered) => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([value]);
    render(App, { props: { api } });
    const region = await card();
    expect(within(region).getByText(label)).toBeInTheDocument();
    for (const action of ["Wake", ...AWAKE, "Cancel"]) {
      expect(!!within(region).queryByRole("button", { name: action }), action).toBe(offered.includes(action));
    }
  });

  it("names the machine in a restart confirmation and sends it only after confirming", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([awake()]);
    render(App, { props: { api } });
    const region = await card();
    await fireEvent.click(within(region).getByRole("button", { name: "Restart…" }));
    expect(await screen.findByText("Restart bazzite?")).toBeInTheDocument();
    expect(api.machineAction).not.toHaveBeenCalled();
    await fireEvent.click(screen.getByRole("button", { name: "Restart" }));
    await waitFor(() => expect(api.machineAction).toHaveBeenCalledWith("bazzite", "restart"));
  });

  it("explains that a cancel cannot be confirmed while the machine is unreachable", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([pendingSleep(false)]);
    render(App, { props: { api } });
    expect(await screen.findByText(/A cancel can't be confirmed until it answers again/)).toBeInTheDocument();
  });

  it("shows the last seen age only for an unreachable machine", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([machine({ reachable: false, status: status({ phase: "offline", offers: ["wake"], last_seen_unix_ms: now() - 720_000 }) })]);
    render(App, { props: { api } });
    const region = await card();
    expect(within(region).getByText(/Last seen/)).toBeInTheDocument();
  });

  it.each([
    ["offline", machine({ reachable: false, status: status({ phase: "offline", offers: ["wake"] }) }), "Wake", "wake"],
    ["awake", awake(), "Sleep", "sleep"],
    ["pending", pendingSleep(), "Cancel", "cancel"],
    ["didn't wake", machine({ reachable: false, status: status({ phase: "did_not_wake", offers: ["wake"] }) }), "Wake", "wake"],
    ["still on", machine({ status: status({ phase: "still_on", offers: ["sleep", "restart", "shutdown", "moonlight"] }) }), "Sleep", "sleep"],
  ])("the power button in %s is %s and sends it", async (_, value, label, offer) => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([value]);
    render(App, { props: { api } });
    const region = await card();
    // The large button comes first in the card, in the avatar position.
    const power = within(region).getAllByRole("button")[0];
    expect(power).toHaveAccessibleName(label);
    expect(power).toBeEnabled();
    await fireEvent.click(power);
    await waitFor(() => expect(api.machineAction).toHaveBeenCalledWith("bazzite", offer));
  });

  it.each([
    ["waking", machine({ reachable: false, status: status({ phase: "waking", waiting: true }) }), "Waking…"],
    ["going to sleep", machine({ status: status({ phase: "going_down", action: "sleep", waiting: true }) }), "Going to sleep…"],
    ["shutting down", machine({ status: status({ phase: "going_down", action: "shutdown", waiting: true }) }), "Shutting down…"],
    ["restarting", machine({ status: status({ phase: "restarting", action: "restart", waiting: true }) }), "Restarting…"],
  ])("the power button while %s shows the wait and is disabled", async (_, value, label) => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([value]);
    render(App, { props: { api } });
    const region = await card();
    const power = within(region).getAllByRole("button")[0];
    expect(power).toHaveAccessibleName(label);
    expect(power).toBeDisabled();
    expect(region.querySelector('.power[data-waiting="true"]')).not.toBeNull();
  });

  it("shows the countdown under the Cancel power button while pending", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([pendingSleep()]);
    render(App, { props: { api } });
    const region = await card();
    expect(region.querySelector(".power .countdown")?.textContent).toMatch(/^\d+ s$/);
  });

  it("opens the details dialog with the connection, Pair and Remove, and closes it", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([awake()]);
    render(App, { props: { api } });
    const region = await card();
    expect(region.querySelector("details")).toBeNull();
    expect(within(region).queryByRole("button", { name: /Pair|Remove/ })).toBeNull();
    await fireEvent.click(within(region).getByRole("button", { name: "Details" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("00:11:22:33:44:55")).toBeInTheDocument();
    expect(within(dialog).getByText("9777")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Pair…" })).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Remove…" })).toBeInTheDocument();
    await fireEvent.keyDown(document.activeElement ?? dialog, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("asks before removing from the details dialog", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([awake()]);
    render(App, { props: { api } });
    await fireEvent.click(within(await card()).getByRole("button", { name: "Details" }));
    await fireEvent.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Remove…" }));
    expect(await screen.findByText("Remove bazzite?")).toBeInTheDocument();
    expect(api.removeMachine).not.toHaveBeenCalled();
  });

  it("never shows the last observation in the future, even as polls outrun a slow clock", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const api = fixture();
    // Each poll stamps its own receipt time, a little ahead of the window's clock.
    vi.mocked(api.machines).mockImplementation(async () => [
      machine({ seen_at_unix_ms: Date.now() + 3_000, status: status({ phase: "waking", waiting: true }) }),
    ]);
    render(App, { props: { api } });
    await fireEvent.click(within(await card()).getByRole("button", { name: "Details" }));
    const dialog = await screen.findByRole("dialog");
    const age = () => within(dialog).getByText((_, node) => node?.tagName === "TIME").textContent;
    expect(age()).toBe("now");
    // The waiting machine polls every 2 s; Poodle's TimeAgo, ticking every 30 s, read "in 10s" here.
    await vi.advanceTimersByTimeAsync(10_000);
    await vi.waitFor(() => expect(vi.mocked(api.machines).mock.calls.length).toBeGreaterThan(3));
    expect(age()).toBe("now");
    expect(dialog.textContent).not.toMatch(/\bin \d/);
  });

  it("polls fast while a machine is waiting, then returns to the refresh interval", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const api = fixture();
    vi.mocked(api.machines)
      .mockResolvedValueOnce([machine({ reachable: false, status: status({ phase: "waking", waiting: true }) })])
      .mockResolvedValue([awake()]);
    render(App, { props: { api } });
    await vi.waitFor(() => expect(screen.getByText("Waking…")).toBeInTheDocument());
    await vi.advanceTimersByTimeAsync(2000);
    await vi.waitFor(() => expect(screen.getByText("Awake")).toBeInTheDocument());
    const polls = vi.mocked(api.machines).mock.calls.length;
    await vi.advanceTimersByTimeAsync(9000);
    expect(api.machines).toHaveBeenCalledTimes(polls);
    await vi.advanceTimersByTimeAsync(1000);
    expect(api.machines).toHaveBeenCalledTimes(polls + 1);
  });

  it("shows a failed cancel and keeps the countdown visible", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([pendingSleep()]);
    vi.mocked(api.machineAction).mockRejectedValue(new Error("timeout"));
    render(App, { props: { api } });
    const region = await card();
    await fireEvent.click(within(region).getByRole("button", { name: "Cancel" }));
    expect(await screen.findByText(/Cancel could not be confirmed/)).toBeInTheDocument();
    expect(within(region).getByText(/Sleeping in/)).toBeInTheDocument();
  });

  it("clears a recovered refresh error without clearing a cancel failure", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const api = fixture();
    vi.mocked(api.machines).mockRejectedValueOnce(new Error("registry unreadable")).mockResolvedValue([pendingSleep()]);
    vi.mocked(api.machineAction).mockRejectedValue(new Error("cancel timeout"));
    render(App, { props: { api } });
    await vi.waitFor(() => expect(screen.getByText(/registry unreadable/)).toBeInTheDocument());
    await vi.advanceTimersByTimeAsync(10_000);
    await vi.waitFor(() => expect(screen.queryByText(/registry unreadable/)).not.toBeInTheDocument());
    await fireEvent.click(within(await card()).getByRole("button", { name: "Cancel" }));
    await vi.waitFor(() => expect(screen.getByText(/cancel timeout/)).toBeInTheDocument());
    await vi.advanceTimersByTimeAsync(10_000);
    expect(screen.getByText(/cancel timeout/)).toBeInTheDocument();
  });
});

describe("observedAge", () => {
  const now = 1_800_000_000_000;
  it.each([
    [now + 5_000, "now"],
    [now + 3_600_000, "now"],
    [now, "now"],
    [now - 4_999, "now"],
    [now - 12_000, "12 s ago"],
    [now - 720_000, "12 min ago"],
    [now - 3 * 3_600_000, "3 h ago"],
    [now - 2 * 86_400_000, "2 d ago"],
  ])("reads %d as %s", (at, text) => {
    expect(observedAge(at, now)).toBe(text);
  });
});
