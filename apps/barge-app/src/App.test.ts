import { render, screen, fireEvent, waitFor } from "@testing-library/svelte";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App.svelte";
import type { AppApi, VolumeSummary } from "./types";

beforeEach(() => {
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() }) });
});

function fixture(volumes: VolumeSummary[] = []): AppApi {
  return {
    volumes: vi.fn().mockResolvedValue(volumes),
    preferences: vi.fn().mockResolvedValue({ launch_at_login: false, refresh_interval: 10, theme: "system" }),
    savePreferences: vi.fn().mockResolvedValue(undefined),
    openWindow: vi.fn().mockResolvedValue(undefined),
    quit: vi.fn().mockResolvedValue(undefined),
  };
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
    expect(await screen.findByText(/10.0 GB free/)).toBeInTheDocument();
    expect(screen.getByText(/Absent/)).toBeInTheDocument();
    expect(screen.getByText(/Different volume at mount/)).toBeInTheDocument();
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
    await fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    const checkbox = screen.getByRole("checkbox", { name: "Launch at login" });
    expect(checkbox).not.toBeChecked();
    await fireEvent.click(checkbox);
    await waitFor(() => expect(api.savePreferences).toHaveBeenCalled());
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
    expect(await screen.findByText("1.0 GB free")).toBeInTheDocument();
  });
});
