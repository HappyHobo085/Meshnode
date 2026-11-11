import { fireEvent, render, screen } from "@testing-library/svelte";
import { expect, it, vi } from "vitest";
import Confirm from "./Confirm.svelte";

it("confirm_enables_on_exact_match", async () => {
  const onConfirm = vi.fn();
  render(Confirm, {
    title: "Remove phone?",
    consequence: "A removed device can never rejoin with the same identity.",
    typeToConfirm: "phone",
    actionLabel: "Remove",
    onConfirm,
  });
  const input = screen.getByRole("textbox");
  const action = screen.getByRole("button", { name: "Remove" });
  expect(action).toBeDisabled();

  for (const wrong of ["Phone", "phone ", "phon", ""]) {
    await fireEvent.input(input, { target: { value: wrong } });
    expect(action).toBeDisabled();
  }

  await fireEvent.input(input, { target: { value: "phone" } });
  expect(action).toBeEnabled();
  await fireEvent.click(action);
  expect(onConfirm).toHaveBeenCalledTimes(1);
});

it("confirm_shows_the_error_and_hint_when_the_action_fails", async () => {
  const onConfirm = vi.fn().mockRejectedValue({
    code: "lastHost",
    message: "This is the last host",
    hint: "Add another host first",
    reason: null,
  });
  render(Confirm, {
    title: "Remove desk?",
    consequence: "A removed device can never rejoin with the same identity.",
    typeToConfirm: "desk",
    actionLabel: "Remove",
    onConfirm,
  });
  await fireEvent.input(screen.getByRole("textbox"), { target: { value: "desk" } });
  await fireEvent.click(screen.getByRole("button", { name: "Remove" }));
  const alert = await screen.findByRole("alert");
  expect(alert).toHaveTextContent("This is the last host");
  expect(alert).toHaveTextContent("Add another host first");
});
