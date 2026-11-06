// Small runtime decoders for JSON from the server. Responses are `unknown` until a decoder
// accepts them; everything past `api/client.ts` works with the decoded types.

export type Decoder<T> = (value: unknown, path: string) => T;
export type Decoded<D> = D extends Decoder<infer T> ? T : never;

export class DecodeError extends Error {
  constructor(
    readonly path: string,
    readonly expected: string,
    readonly actual: unknown,
  ) {
    super(`${path || "response"}: expected ${expected}, got ${describe(actual)}`);
    this.name = "DecodeError";
  }
}

function describe(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return "an array";
  if (typeof value === "string") return `"${value.slice(0, 40)}"`;
  return typeof value;
}

export const string: Decoder<string> = (value, path) => {
  if (typeof value !== "string") throw new DecodeError(path, "a string", value);
  return value;
};

export const integer: Decoder<number> = (value, path) => {
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new DecodeError(path, "an integer", value);
  }
  return value;
};

export const number: Decoder<number> = (value, path) => {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new DecodeError(path, "a number", value);
  }
  return value;
};

export const boolean: Decoder<boolean> = (value, path) => {
  if (typeof value !== "boolean") throw new DecodeError(path, "a boolean", value);
  return value;
};

/** An RFC 3339 timestamp, kept as the server's string. */
export const timestamp: Decoder<string> = (value, path) => {
  const text = string(value, path);
  if (!Number.isFinite(Date.parse(text))) throw new DecodeError(path, "an RFC 3339 time", value);
  return text;
};

/** Any JSON value, for type-specific payloads that are only displayed. */
export const json: Decoder<unknown> = (value) => value;

export function oneOf<const T extends readonly string[]>(...values: T): Decoder<T[number]> {
  return (value, path) => {
    if (typeof value !== "string" || !values.includes(value)) {
      throw new DecodeError(path, `one of ${values.join(", ")}`, value);
    }
    return value as T[number];
  };
}

/** `Option<T>` on the Rust side: `null`, or absent when the server skips it. */
export function nullable<T>(decode: Decoder<T>): Decoder<T | null> {
  return (value, path) => (value === null || value === undefined ? null : decode(value, path));
}

export function array<T>(decode: Decoder<T>): Decoder<T[]> {
  return (value, path) => {
    if (!Array.isArray(value)) throw new DecodeError(path, "an array", value);
    return value.map((item, index) => decode(item, `${path}[${index}]`));
  };
}

export function record<T>(decode: Decoder<T>): Decoder<Record<string, T>> {
  return (value, path) => {
    if (!isPlainObject(value)) throw new DecodeError(path, "an object", value);
    const out: Record<string, T> = {};
    for (const [key, item] of Object.entries(value)) out[key] = decode(item, `${path}.${key}`);
    return out;
  };
}

type Shape = Record<string, Decoder<unknown>>;

/** An object with the given fields. Unknown fields are ignored, so the server may add some. */
export function object<S extends Shape>(shape: S): Decoder<{ [K in keyof S]: Decoded<S[K]> }> {
  return (value, path) => {
    if (!isPlainObject(value)) throw new DecodeError(path, "an object", value);
    const out: Record<string, unknown> = {};
    for (const [key, decode] of Object.entries(shape)) {
      out[key] = decode(value[key], path ? `${path}.${key}` : key);
    }
    return out as { [K in keyof S]: Decoded<S[K]> };
  };
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
