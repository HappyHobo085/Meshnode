/** A fixed-capacity FIFO: once full, each push drops the oldest item. O(1) push. */
export class RingBuffer<T> {
  readonly capacity: number;
  #items: (T | undefined)[];
  #start = 0;
  #length = 0;

  constructor(capacity: number) {
    if (!Number.isInteger(capacity) || capacity <= 0) {
      throw new RangeError(`capacity must be a positive integer, got ${capacity}`);
    }
    this.capacity = capacity;
    this.#items = new Array<T | undefined>(capacity);
  }

  get length(): number {
    return this.#length;
  }

  push(item: T): void {
    if (this.#length < this.capacity) {
      this.#items[(this.#start + this.#length) % this.capacity] = item;
      this.#length++;
    } else {
      this.#items[this.#start] = item;
      this.#start = (this.#start + 1) % this.capacity;
    }
  }

  /** Oldest first. */
  toArray(): T[] {
    const out = new Array<T>(this.#length);
    for (let i = 0; i < this.#length; i++) {
      out[i] = this.#items[(this.#start + i) % this.capacity] as T;
    }
    return out;
  }

  /** Items `start` (inclusive) to `end` (exclusive), oldest first, like `Array.slice`
   *  with non-negative bounds; costs only the items returned. */
  slice(start: number, end: number = this.#length): T[] {
    const a = Math.max(0, Math.min(start, this.#length));
    const b = Math.max(a, Math.min(end, this.#length));
    const out = new Array<T>(b - a);
    for (let i = a; i < b; i++) out[i - a] = this.#items[(this.#start + i) % this.capacity] as T;
    return out;
  }

  clear(): void {
    this.#items = new Array<T | undefined>(this.capacity);
    this.#start = 0;
    this.#length = 0;
  }
}
