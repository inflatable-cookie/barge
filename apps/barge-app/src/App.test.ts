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
  const api: AppApi = {
    volumes: vi.fn().mockResolvedValue(volumes),
    doctorStatus: vi.fn().mockResolvedValue({ observation: null, error: null }),
    preferences: vi.fn().mockResolvedValue({ ...preferences }),
    savePreferences: vi.fn(async (next: Preferences) => next),
    openWindow: vi.fn().mockResolvedValue(undefined),
    quit: vi.fn().mockResolvedValue(undefined),
    machines: vi.fn().mockResolvedValue([]),
    addMachine: vi.fn(), pairMachine: vi.fn(), removeMachine: vi.fn(), beginMachineAction: vi.fn(), machineAction: vi.fn(), moonlight: vi.fn(),
  };
  // A recorded click answers with the machines as they stand; a test that wants
  // a different answer stubs it.
  vi.mocked(api.beginMachineAction).mockImplementation(async () => api.machines());
  return api;
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
  /** What the awake card's menu offers, in order. */
  const AWAKE = ["Restart…", "Shut down…", "Open Moonlight", "Details…"];
  function machine(fields: Partial<MachineView> & { status: MachineStatus }): MachineView {
    return {
      name: "bazzite", hostname: "bazzite.local", mac: "00:11:22:33:44:55", mode: "lan", address: "bazzite.local",
      daemon_port: 9777, reachable: true, error: null, seen_at_unix_ms: now(), ...fields,
    };
  }
  const offline = () => machine({
    reachable: false, error: "Connection timed out",
    status: status({ phase: "offline", offers: ["wake"], last_seen_unix_ms: now() - 720_000 }),
  });
  const waking = () => machine({
    reachable: false, error: "Connection timed out",
    status: status({ phase: "waking", waiting: true, last_seen_unix_ms: now() - 720_000 }),
  });
  const awake = () => machine({ status: status({ phase: "awake", offers: ["sleep", "restart", "shutdown", "moonlight"] }) });
  const stillOn = () => machine({ status: status({ phase: "still_on", offers: ["sleep", "restart", "shutdown", "moonlight"] }) });
  const pendingSleep = (reachable = true) => machine({
    reachable,
    error: reachable ? null : "Connection timed out",
    status: status({ phase: "pending", action: "sleep", deadline_unix_ms: now() + 12_000, offers: ["cancel"], waiting: true }),
  });
  const goingDown = (action: "sleep" | "shutdown") => machine({ status: status({ phase: "going_down", action, waiting: true }) });
  const restarting = () => machine({ status: status({ phase: "restarting", action: "restart", waiting: true }) });
  const didNotWake = () => machine({ reachable: false, status: status({ phase: "did_not_wake", offers: ["wake"] }) });

  const MENU_BUTTON = "Actions for bazzite";

  async function card(name = "bazzite") {
    return screen.findByLabelText(`${name} machine`);
  }

  /**
   * The card's actions menu, opened from the `...` button at its top right.
   * Poodle's Menu wraps its trigger, so the wrapper and the icon button inside
   * it carry the same accessible name.
   */
  async function openActions(region: HTMLElement) {
    const [trigger] = within(region).getAllByRole("button", { name: MENU_BUTTON });
    await fireEvent.click(trigger);
    return screen.findByRole("menu", { name: "bazzite actions" });
  }

  async function openDetails(region: HTMLElement) {
    const menu = await openActions(region);
    await fireEvent.click(within(menu).getByRole("menuitem", { name: "Details…" }));
    return screen.findByRole("dialog");
  }

  /** A command that stays in flight until the test finishes it. */
  function deferred<T = void>() {
    let finish!: (value: T) => void;
    let fail!: (reason: unknown) => void;
    const promise = new Promise<T>((resolve, reject) => { finish = resolve; fail = reject; });
    return { promise, finish, fail };
  }

  it("shows an empty state with the add button when nothing is registered", async () => {
    render(App, { props: { api: fixture() } });
    expect(await screen.findByText(/No machines yet/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add machine" })).toBeInTheDocument();
    expect(screen.queryByText("This Mac")).not.toBeInTheDocument();
  });

  it.each([
    ["offline", offline(), "Offline", "Wake", ["Details…"]],
    ["waking", waking(), "Waking…", "Waking…", ["Details…"]],
    ["awake", awake(), "Awake", "Sleep", AWAKE],
    ["pending", pendingSleep(), /^Sleeping in \d+ s$/, "Cancel", ["Details…"]],
    ["going to sleep", goingDown("sleep"), "Going to sleep…", "Going to sleep…", ["Details…"]],
    ["shutting down", goingDown("shutdown"), "Shutting down…", "Shutting down…", ["Details…"]],
    ["restarting", restarting(), "Restarting…", "Restarting…", ["Details…"]],
    ["didn't wake", didNotWake(), "Didn't wake", "Wake", ["Details…"]],
    ["still on", stillOn(), "Still on", "Sleep", AWAKE],
  ])("a card in %s shows its state once and offers only that state's actions", async (_, value, label, power, items) => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([value]);
    render(App, { props: { api } });
    const region = await card();
    expect(within(region).getByText(label)).toBeInTheDocument();
    // One what-I-can-do-now control in the avatar position, one menu beside it.
    expect(within(region).getByRole("button", { name: power })).toBeInTheDocument();
    const menu = await openActions(region);
    expect(within(menu).getAllByRole("menuitem").map((item) => item.textContent?.trim())).toEqual(items);
  });

  it("shows the wait at once, before the command resolves", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([offline()]);
    vi.mocked(api.beginMachineAction).mockResolvedValue([waking()]);
    const sent = deferred();
    vi.mocked(api.machineAction).mockReturnValue(sent.promise);
    render(App, { props: { api } });
    const region = await card();
    await fireEvent.click(within(region).getByRole("button", { name: "Wake" }));

    // The click is recorded in the shared state, so the card shows the wait
    // while the daemon is still being asked.
    expect(api.beginMachineAction).toHaveBeenCalledWith("bazzite", "wake");
    expect(await within(region).findByText("Waking…")).toBeInTheDocument();
    expect(region.querySelector('.power[data-waiting="true"]')).not.toBeNull();
    expect(api.machineAction).toHaveBeenCalledWith("bazzite", "wake");
    expect(within(region).getByRole("button", { name: "Waking…" })).toBeDisabled();

    // The poll owns the state afterwards, not the renderer's copy.
    sent.finish();
    await waitFor(() => expect(within(region).getByText("Offline")).toBeInTheDocument());
  });

  it("says so and puts the card back when the send fails", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([offline()]);
    vi.mocked(api.beginMachineAction).mockResolvedValue([waking()]);
    vi.mocked(api.machineAction).mockRejectedValue(new Error("the wake packet was refused"));
    render(App, { props: { api } });
    const region = await card();
    await fireEvent.click(within(region).getByRole("button", { name: "Wake" }));

    expect(await screen.findByText("Wake failed: the wake packet was refused")).toBeInTheDocument();
    expect(await within(region).findByText("Offline")).toBeInTheDocument();
    expect(within(region).getByRole("button", { name: "Wake" })).toBeEnabled();
  });

  it("shows the countdown once, in the state label", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([pendingSleep()]);
    render(App, { props: { api } });
    const region = await card();
    expect(within(region).getByText(/^Sleeping in \d+ s$/)).toBeInTheDocument();
    expect(within(region).getAllByText(/\b\d+ s\b/)).toHaveLength(1);
    expect(region.querySelector(".countdown")).toBeNull();
    // The ring carries the progress and the button keeps the cancel glyph.
    expect(within(region).getByRole("button", { name: "Cancel" })).toBeInTheDocument();
    expect(region.querySelector('.power[data-waiting="true"]')).not.toBeNull();
  });

  it("names the machine in a restart confirmation and sends it only after confirming", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([awake()]);
    render(App, { props: { api } });
    const menu = await openActions(await card());
    await fireEvent.click(within(menu).getByRole("menuitem", { name: "Restart…" }));
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
    vi.mocked(api.machines).mockResolvedValue([offline()]);
    render(App, { props: { api } });
    const region = await card();
    expect(within(region).getByText(/Last seen/)).toBeInTheDocument();
  });

  it.each([
    ["offline", offline(), "Wake", "wake"],
    ["awake", awake(), "Sleep", "sleep"],
    ["pending", pendingSleep(), "Cancel", "cancel"],
    ["didn't wake", didNotWake(), "Wake", "wake"],
    ["still on", stillOn(), "Sleep", "sleep"],
  ])("the power button in %s sends the action it names", async (_, value, label, offer) => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([value]);
    render(App, { props: { api } });
    const region = await card();
    const power = within(region).getByRole("button", { name: label });
    expect(power).toBeEnabled();
    await fireEvent.click(power);
    await waitFor(() => expect(api.machineAction).toHaveBeenCalledWith("bazzite", offer));
  });

  it.each([
    ["waking", waking(), "Waking…"],
    ["going to sleep", goingDown("sleep"), "Going to sleep…"],
    ["shutting down", goingDown("shutdown"), "Shutting down…"],
    ["restarting", restarting(), "Restarting…"],
  ])("the power button while %s shows the wait and is disabled", async (_, value, label) => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([value]);
    render(App, { props: { api } });
    const region = await card();
    const power = within(region).getByRole("button", { name: label });
    expect(power).toBeDisabled();
    expect(region.querySelector('.power[data-waiting="true"]')).not.toBeNull();
  });

  it("opens the details dialog from the menu with Pair and Remove, and closes it", async () => {
    const api = fixture();
    vi.mocked(api.machines).mockResolvedValue([awake()]);
    render(App, { props: { api } });
    const region = await card();
    expect(region.querySelector("details")).toBeNull();
    const dialog = await openDetails(region);
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
    await fireEvent.click(within(await openDetails(await card())).getByRole("button", { name: "Remove…" }));
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
    const dialog = await openDetails(await card());
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
      .mockResolvedValueOnce([waking()])
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
