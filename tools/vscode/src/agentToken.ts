import * as vscode from "vscode";
import { MindexApi } from "./api";
import { say } from "./brand";
import { showIssuedToken } from "./tokenDoc";
import {
    ACTION_PRESETS,
    actionsForPreset,
    AGENT_AUDIENCE,
    OFFERED_ACTIONS,
} from "./tokenGrants";

/**
 * Issue a scoped token from the one this extension already holds.
 *
 * WHAT THIS IS FOR. An agent handed a URL has no process holding a credential, so
 * the only way it can send one is for a person to paste it into a chat. That is
 * acceptable exactly when what gets pasted is narrow and expiring, and the point
 * of this command is that producing such a thing should not require a terminal on
 * the server's host. It is the same `mindex mint-token` capability, reachable
 * from where the person actually is.
 *
 * WHAT IT DELIBERATELY CANNOT DO. Nothing here is a security boundary — a webview
 * and a command palette are not places to enforce one, and the server does not
 * rely on them. `POST /auth/tokens` refuses any request that exceeds the minting
 * token: not a wider action set, not a project it does not hold, not a later
 * expiry. So every narrowing below is a *usability* measure. It picks the safe
 * thing by default and makes the dangerous shapes cost a deliberate tick, and if
 * it were bypassed the server would still refuse.
 *
 * The choices it makes, each because the alternative has a failure mode:
 *
 * - **This project only, never `*`.** A wildcard token is exactly as dangerous as
 *   the shared API key this whole mechanism replaced. There is no menu entry for
 *   one; minting it is a deliberate act on the server's host.
 * - **Two presets over the tick list, not instead of it.** Read-only and
 *   read-and-write are the two shapes people actually want, and assembling either
 *   from four checkboxes every time is work that teaches nothing. The presets are
 *   an ordering, not a narrowing: the full list is one item down the same menu, it
 *   is unchanged, and it is still the only way to reach `delete` — a destructive
 *   grant should not be one keystroke from the top of a list. The write modal fires
 *   for the preset exactly as it does for a tick, because the preset says *which*
 *   actions and the modal says what they cost.
 * - **Write actions are offered and pre-ticked off.** They used to be absent, on
 *   the argument that an agent has no working tree so `index` buys it nothing.
 *   That is true of an agent handed a URL and false of the case people actually
 *   hit — an agent running *on this machine*, editing files, which is expected to
 *   keep the index current afterwards. Refusing to issue such a token here does
 *   not prevent it; it moves the work to a shell, where the token that gets
 *   minted is usually wider than this one would have been. So the list is
 *   complete and the default is read-only, which puts the deliberation where it
 *   belongs: on the tick, not on the vocabulary.
 * - **`admin` and `mint` are not offered at all**, and that is a different call
 *   from the one above. Neither is a thing an agent needs in order to work on
 *   this project — `admin` is the global operator surface and `mint` is the power
 *   to keep issuing credentials after this one expires, which is precisely the
 *   bound a short lifetime is buying. Both remain mintable on the host.
 * - **Days, capped at seven.** Long enough for a working session and then some;
 *   short enough that losing one is bounded, which is the property that makes a
 *   pasted credential tolerable. The server's `[auth].max_token_days` and the
 *   minting token's own life both still apply on top.
 * - **Labelled `agent`.** The `aud` claim the CLI writes with `--for`. The server
 *   checks none of it; what it buys is that this token pasted into an editor's
 *   keychain is refused by the editor, with a sentence naming both audiences.
 * - **Clipboard, not a file and not a notification body.** A file is a copy
 *   nobody decided to keep; a notification truncates a 300-character token and
 *   puts it in a log. The clipboard is where it is going anyway — and when it is
 *   not (a config that has to be edited by hand, a token that has to be read out),
 *   `Show it` opens the read-only in-memory document in `tokenDoc.ts`, which is
 *   the same refusal to write a file wearing a different hat.
 */

/** The lifetimes offered. See the note above on why seven is the ceiling. */
const LIFETIMES: readonly { label: string; days: number; detail: string }[] = [
    { label: "1 day", days: 1, detail: "one working session" },
    { label: "3 days", days: 3, detail: "a few sessions" },
    { label: "7 days", days: 7, detail: "the longest this command will issue" },
];

/**
 * The tick list, as it was before the presets arrived.
 *
 * `undefined` is a dismissed picker and `[]` is a deliberate empty tick — two
 * different things, and only the second is worth a sentence. An empty grant is
 * refused rather than issued, because the server would issue it happily and the
 * holder would discover it one 403 at a time.
 */
async function pickActions(projectLabel: string): Promise<string[] | undefined> {
    const picked = await vscode.window.showQuickPick(
        OFFERED_ACTIONS.map((a) => ({
            label: a.label,
            description: a.detail,
            action: a.action,
            picked: a.default,
        })),
        {
            title: say(`token for an agent — ${projectLabel}`),
            placeHolder: "what may it do? (this project only)",
            canPickMany: true,
            ignoreFocusOut: true,
        }
    );
    if (picked === undefined) {
        return undefined;
    }
    if (picked.length === 0) {
        void vscode.window.showWarningMessage(
            say("no actions were selected, so there is no token worth issuing.")
        );
        return undefined;
    }
    return picked.map((p) => p.action);
}

export async function mintAgentToken(
    api: MindexApi,
    projectGuid: string,
    projectLabel: string
): Promise<void> {
    const preset = await vscode.window.showQuickPick(
        ACTION_PRESETS.map((p) => ({ label: p.label, description: p.detail, id: p.id })),
        {
            title: say(`token for an agent — ${projectLabel}`),
            placeHolder: "what may it do? (this project only)",
            ignoreFocusOut: true,
        }
    );
    if (preset === undefined) {
        return;
    }
    const granted = actionsForPreset(preset.id);
    const actions = granted === undefined ? await pickActions(projectLabel) : [...granted];
    if (actions === undefined) {
        return;
    }
    const writes = actions.filter((a) => a === "index" || a === "delete");

    // A second, modal confirmation for the write actions only. The tick already
    // said "yes"; this says what "yes" costs, at a moment when saying no is still
    // free — and it names the actions rather than the word "write", because
    // `delete` removing stored research is not what most people picture.
    if (writes.length > 0) {
        const ISSUE = "Issue it";
        const choice = await vscode.window.showWarningMessage(
            say(
                `this token will be able to ${writes.join(" and ")} in ${projectLabel}. ` +
                    "It is going somewhere a person will paste it, and there is no revocation " +
                    "list — until it expires, anything holding it can do this."
            ),
            { modal: true },
            ISSUE
        );
        if (choice !== ISSUE) {
            return;
        }
    }

    const lifetime = await vscode.window.showQuickPick(
        LIFETIMES.map((l) => ({ ...l, description: l.detail, detail: undefined })),
        {
            title: say(`token for an agent — ${projectLabel}`),
            placeHolder: `${actions.join(" + ")}, this project only — how long should it last?`,
            ignoreFocusOut: true,
        }
    );
    if (lifetime === undefined) {
        return;
    }

    const holder = await vscode.window.showInputBox({
        title: say("who is this token for?"),
        prompt: "A label, not a secret. It rides inside the token and appears in the server's logs — it is how one credential is told from another when revoking.",
        value: "agent",
        ignoreFocusOut: true,
        validateInput: (v) => (v.trim() === "" ? "a label is required" : undefined),
    });
    if (holder === undefined) {
        return;
    }

    const issued = await api.mintToken({
        sub: `agent:${holder.trim()}`,
        projects: [projectGuid],
        actions,
        audiences: [AGENT_AUDIENCE],
        days: lifetime.days,
    });

    await vscode.env.clipboard.writeText(issued.token);
    const until =
        issued.expires_at === undefined || issued.expires_at === null
            ? ""
            : ` until ${new Date(issued.expires_at * 1000).toLocaleString()}`;
    // What it can do is stated in the confirmation rather than left to the token:
    // the person about to paste this is the last one who can notice it is wrong,
    // and they cannot read a base64 payload. The server's echo is used rather
    // than the request, so a grant the server narrowed is reported as narrowed.
    //
    // `Show it` is a button rather than the default because revealing a credential
    // should be an act, not a side effect of issuing one — the clipboard already
    // serves the case where it is pasted straight on.
    const SHOW = "Show it";
    const choice = await vscode.window.showInformationMessage(
        say(
            `token copied to the clipboard — ${issued.actions.join(" + ")} on this project ` +
                `only${until}. It is shown once and stored nowhere.`
        ),
        SHOW
    );
    if (choice === SHOW) {
        await showIssuedToken(issued.token, {
            actions: issued.actions,
            projects: issued.projects,
            expiresAt: issued.expires_at,
        });
    }
}
