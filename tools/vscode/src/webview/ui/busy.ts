/**
 * The webview half of the busy discipline: paint what the host is doing.
 *
 * The host owns the decision (see `src/busy.ts`); this only reflects it, so it
 * holds no state about *whether* something is running — only about the one thing
 * the host cannot know, which is why a control was already disabled before the
 * busy key arrived.
 *
 * That composition is the load-bearing part. `#runs-delete` is disabled because
 * nothing is selected; the GC button is disabled because there is nothing to
 * collect. A naive `el.disabled = busy` re-enables both when an unrelated call
 * finishes, which is a worse bug than the double-click it was added to prevent:
 * a button that offers an action the page has no arguments for. So a control's
 * own verdict is remembered here and the busy state is layered over it.
 *
 * Controls declare their key in the DOM (`data-busy-key`), not in a registry:
 * rows and preview panes are rebuilt constantly, and a registry would need a
 * lifetime hook at every one of those sites to stay honest.
 */

/** What each control's own logic last said, independent of any busy key. */
const ownDisabled = new WeakMap<HTMLElement, boolean>();

/** Keys currently held, so a control built *while* busy is born disabled. */
const held = new Set<string>();

type Disableable = HTMLElement & { disabled?: boolean };

/**
 * A control's own enable/disable verdict. Every direct `el.disabled = …` write
 * goes through this instead, or the busy layer will forget it.
 */
export function setEnabled(el: Disableable, enabled: boolean): void {
    ownDisabled.set(el, !enabled);
    paint(el);
}

/** The host says a key started or stopped. Repaint everything wearing it. */
export function applyBusy(key: string, busy: boolean): void {
    if (busy) {
        held.add(key);
    } else {
        held.delete(key);
    }
    for (const el of document.querySelectorAll<Disableable>(`[data-busy-key="${key}"]`)) {
        paint(el);
    }
}

/**
 * Paint a control that was just built. Call after inserting anything carrying a
 * `data-busy-key`: without it, a row rendered during an in-flight call is the
 * one live button on an otherwise frozen page.
 */
export function paintBusy(root: ParentNode = document): void {
    for (const el of root.querySelectorAll<Disableable>("[data-busy-key]")) {
        paint(el);
    }
}

function paint(el: Disableable): void {
    // Seed from the authored markup the first time we touch a control, or the
    // first unrelated busy key to clear would *enable* a button that shipped
    // `disabled` — offering an action the page has no arguments for.
    if (!ownDisabled.has(el)) {
        ownDisabled.set(el, el.disabled === true);
    }
    const key = el.dataset.busyKey;
    const busy = key !== undefined && held.has(key);
    const disabled = (ownDisabled.get(el) ?? false) || busy;
    if (el.disabled !== undefined) {
        el.disabled = disabled;
    }
    // `aria-busy` rather than only `disabled` so the state is legible to a
    // screen reader as "working", not as "unavailable" — they are different
    // facts and only one of them resolves on its own.
    el.setAttribute("aria-busy", busy ? "true" : "false");
    spin(el, busy);
}

/**
 * Swap the control's glyph for a spinner while it works.
 *
 * The original class list is stashed on the element rather than recomputed: the
 * icon a button carries is authored in one place (HTML or a builder), and
 * reconstructing it here would be a second copy that drifts.
 */
function spin(el: Element, busy: boolean): void {
    const icon = el.querySelector<HTMLElement>("[data-busy-icon]");
    if (icon === null) {
        return;
    }
    if (busy) {
        icon.dataset.restIcon ??= icon.className;
        icon.className = "codicon codicon-loading codicon-modifier-spin";
    } else if (icon.dataset.restIcon !== undefined) {
        icon.className = icon.dataset.restIcon;
    }
}
