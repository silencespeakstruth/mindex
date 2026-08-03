/**
 * The product name, spelled once.
 *
 * It is **MINDex** — not "mindex", which is the *identifier*: the command ids, the
 * setting ids, the `.mindex` filename, the CLI binaries and the npm package name all
 * stay lowercase and must never be swept along with the prose. Anything a user reads
 * goes through here; anything a machine matches does not.
 */
export const BRAND = "MINDex";

/**
 * A notification line: `MINDex: something happened`.
 *
 * A helper rather than a template at each call site because there were ~30 of them
 * hand-writing the same prefix, and a prefix written thirty times is a prefix that
 * drifts.
 */
export function say(message: string): string {
    return `${BRAND}: ${message}`;
}
