import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, test, vi } from "vitest";
import { App } from "./App";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async () => "9.8.7"),
}));

afterEach(cleanup);

// Fails if the version from the backend is not shown.
test("shows the version received from the backend", async () => {
  render(<App />);
  expect(await screen.findByText("v9.8.7")).toBeTruthy();
  expect(screen.getByText("Hearth")).toBeTruthy();
});
