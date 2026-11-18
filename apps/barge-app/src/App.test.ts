import { render, screen, fireEvent, waitFor } from "@testing-library/svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App.svelte";
import type { AppApi, Preferences, VolumeSummary } from "./types";

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
    preferences: vi.fn().mockResolvedValue({ ...preferences }),
    savePreferences: vi.fn(async (next: Preferences) => next),
    openWindow: vi.fn().mockResolvedValue(undefined),
    quit: vi.fn().mockResolvedValue(undefined),
  };
}

async function settingsView() {
  await fireEvent.click(await screen.findByRole("button", { name: "Settings" }));
}

describe("tray popover", () => {
  it("shows empty state and opens the window or quits", async () => {
    const api = fixture();
    render(App, { props: { api, popover: true } });
    expect(await screen.findByText("No volumes registered")).toBeInTheDocument();
    expect(screen.getByText("No machines yet")).toBeInTheDocument();
    await fireEvent.click(screen.getByRole("button", { name: "Open Barge" }));
    await fireEvent.click(screen.getByRole("button", { name: "Quit" }));
    expect(api.openWindow).toHaveBeenCalledOnce();
    expect(api.quit).toHaveBeenCalledOnce();
  });

  it("distinguishes available, absent and mismatched volumes", async () => {
    render(App, { props: { api: fixture([
      { name: "Builds", presence: "present", free_bytes: 10 * 1024 ** 3 },
      { name: "Archive", presence: "absent", free_bytes: null },
      { name: "Other", presence: "mismatch", free_bytes: null },
    ]), popover: true } });
    // Sizes are binary: GiB, not GB.
    expect(await screen.findByText(/10.0 GiB free/)).toBeInTheDocument();
    expect(screen.queryByText(/GB free/)).not.toBeInTheDocument();
    expect(screen.getByText(/Absent/)).toBeInTheDocument();
    expect(screen.getByText(/Different volume at mount/)).toBeInTheDocument();
  });

  it("refreshes storage when the popover is opened again", async () => {
    const api = fixture([{ name: "Builds", presence: "present", free_bytes: 1024 ** 3 }]);
    render(App, { props: { api, popover: true } });
    expect(await screen.findByText(/1.0 GiB free/)).toBeInTheDocument();

    vi.mocked(api.volumes).mockResolvedValue([{ name: "Builds", presence: "absent", free_bytes: null }]);
    await fireEvent.blur(window);
    await fireEvent.focus(window);

    expect(await screen.findByText(/Absent/)).toBeInTheDocument();
    expect(screen.queryByText(/1.0 GiB free/)).not.toBeInTheDocument();
    expect(api.volumes).toHaveBeenCalledTimes(2);
  });

  it("says a failed read failed instead of claiming there is no storage", async () => {
    const api = fixture();
    vi.mocked(api.volumes).mockRejectedValue(new Error("the account state is unreadable"));
    render(App, { props: { api, popover: true } });
    expect(await screen.findByRole("alert")).toHaveTextContent("Could not read storage");
    expect(screen.queryByText("No volumes registered")).not.toBeInTheDocument();
  });
});

describe("window navigation", () => {
  it("shows the machines, storage, activity and settings views", async () => {
    const api = fixture();
    render(App, { props: { api } });
    expect(await screen.findByText("No machines yet")).toBeInTheDocument();
    await fireEvent.click(screen.getByRole("button", { name: "Storage" }));
    expect(await screen.findByText("No volumes registered")).toBeInTheDocument();
    await fireEvent.click(screen.getByRole("button", { name: "Activity" }));
    expect(screen.getByText("No activity yet")).toBeInTheDocument();
    await settingsView();
    expect(screen.getByRole("switch", { name: "Launch at login" })).not.toBeChecked();
    expect(screen.getByRole("switch", { name: "Show window at launch" })).toBeChecked();
    expect(screen.getByText(/menu bar at the top of the screen/)).toBeInTheDocument();
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
