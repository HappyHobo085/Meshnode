import { expect, it } from "vitest";
import { RingBuffer } from "./ringbuffer";

it("ringbuffer_keeps_last_n", () => {
  const rb = new RingBuffer<number>(3);
  expect(rb.length).toBe(0);
  expect(rb.toArray()).toEqual([]);
  rb.push(1);
  rb.push(2);
  expect(rb.toArray()).toEqual([1, 2]);
  rb.push(3);
  rb.push(4);
  rb.push(5);
  expect(rb.length).toBe(3);
  expect(rb.toArray()).toEqual([3, 4, 5]);
  for (let i = 6; i <= 100; i++) rb.push(i);
  expect(rb.toArray()).toEqual([98, 99, 100]);
  expect(rb.slice(1)).toEqual([99, 100]);
  expect(rb.slice(0, 2)).toEqual([98, 99]);
  expect(rb.slice(2, 99)).toEqual([100]);
  expect(rb.slice(5, 9)).toEqual([]);
  rb.clear();
  expect(rb.length).toBe(0);
  expect(rb.toArray()).toEqual([]);
});

it("ringbuffer_rejects_a_non_positive_capacity", () => {
  expect(() => new RingBuffer(0)).toThrow();
});
