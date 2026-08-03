import * as vscode from "vscode";
import { ChallengeRequest, ConfigResponse, ResearchEffort } from "./api";

/**
 * The challenge launch dialog: a QuickPick chain, not a form.
 *
 * A challenge deliberately has almost nothing to fill in — the server refuses a
 * question, a scope and context (`deny_unknown_fields`), because all three come
 * from the subject: re-deriving the subject's claims through the tools IS the
 * refutation, and outside hearsay must not feed it. What is left — effort, and
 * optionally a model and a time cap — is exactly the popup-first shape the rest
 * of the extension already uses (`docs/claude/vscode.md`). Esc anywhere aborts.
 */
export async function pickChallengeOptions(
    subject: { seq: number; title: string },
    cfg: ConfigResponse | undefined
): Promise<ChallengeRequest | undefined> {
    const title = `Challenge #${subject.seq} — ${subject.title}`;
    const research = cfg?.research;

    const efforts: ResearchEffort[] = ["low", "medium", "high"];
    const effortPick = await vscode.window.showQuickPick(
        efforts.map((e) => {
            const info = research?.effort?.[e];
            return {
                label: e,
                description:
                    info === undefined
                        ? undefined
                        : `${info.max_steps} lookups · up to ` +
                          `${Math.round((info.worst_case_seconds ?? info.max_seconds) / 60)} min worst case`,
            };
        }),
        {
            title,
            placeHolder:
                "The challenge re-derives the report's claims through the tools, on the " +
                "report's own scope. You only choose how hard it tries.",
        }
    );
    if (effortPick === undefined) {
        return undefined;
    }
    const effort = effortPick.label;

    // Offered only when the server confirmed what Ollama holds — the same
    // offering-vs-validating rule as the Ask form's model select.
    let model: string | undefined;
    const models = research?.models ?? [];
    if (models.length > 0) {
        const def = research?.default_model;
        const modelPick = await vscode.window.showQuickPick(
            [
                {
                    label: def === undefined || def === "" ? "(server default)" : def,
                    description: "the server's configured default",
                    value: undefined as string | undefined,
                },
                ...models
                    .filter((m) => m !== def)
                    .map((m) => ({
                        label: m,
                        description: undefined,
                        value: m,
                    })),
            ],
            { title, placeHolder: "Model for the challenge run" }
        );
        if (modelPick === undefined) {
            return undefined;
        }
        model = modelPick.value;
    }

    // One budget axis, the one worth a prompt: how long the caller is willing to
    // wait. Blank keeps the effort preset; the rest of the axes stay presets —
    // a challenge is launched from a list, not composed in a form.
    const presetSeconds = research?.effort?.[effort]?.max_seconds;
    const cap = research?.max_request_seconds;
    const secondsRaw = await vscode.window.showInputBox({
        title,
        prompt:
            "Max investigation seconds — leave blank for the effort preset" +
            (presetSeconds !== undefined ? ` (${presetSeconds}s)` : "") +
            (cap !== undefined ? `, capped at ${cap}` : ""),
        validateInput: (v) => {
            if (v.trim() === "") {
                return undefined;
            }
            const n = Number(v.trim());
            if (!Number.isInteger(n) || n <= 0) {
                return "A positive whole number of seconds, or blank.";
            }
            if (cap !== undefined && n > cap) {
                return `The server caps requests at ${cap} seconds.`;
            }
            return undefined;
        },
    });
    if (secondsRaw === undefined) {
        return undefined;
    }
    const seconds = secondsRaw.trim() === "" ? undefined : Number(secondsRaw.trim());

    return {
        effort,
        model,
        budget: seconds === undefined ? undefined : { max_seconds: seconds },
    };
}
