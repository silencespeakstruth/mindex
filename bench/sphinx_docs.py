#!/usr/bin/env python3
"""Turn a project's own Sphinx documentation into descriptive retrieval queries.

THE TASK THIS BUILDS FOR, and why it replaced the first one. mindex is a search
engine, not an agent: it finds code that matches a description, it does not
reason from a symptom to a cause. Issue localization — "here is a bug report,
name the files to fix" — needs that reasoning, and in this system it belongs to
`/research`. Scoring `search` on it measures the gap between matching and
inferring, and would answer the roadmap questions (does ColBERT earn its 99.6%
of stored bytes?) on the wrong task.

What a caller actually asks is "show me how SQL caching works". So the query is
a **description of behaviour that already exists**, and the ground truth has to
say which code that description is about.

WHERE THE GROUND TRUTH COMES FROM, and why it is not us. A project's own
documentation is prose written by its maintainers describing its own code, and
Sphinx makes the link explicit and machine-readable in two forms:

    .. currentmodule:: django.core.cache        directives — the API blocks
    .. class:: BaseCache
    :class:`~django.template.Engine`            inline roles — prose pointers

django carries 3 212 of the first and 8 589 of the second. Nobody wrote them
for a benchmark.

THE CENTRAL RULE: **an explicit code reference is answer key, never query.**
Every directive and every dotted role is removed from the query text and used
as gold. What is left is the natural-language description — which is exactly
the input a caller supplies. Code blocks go too: a doctest that reads
`from django.core.cache import cache` hands over the answer, and a query
containing code is code-to-code matching rather than the description-to-code
retrieval being measured.

Two things are measured rather than assumed, because both would otherwise
quietly decide the result:

  * **Resolution is verified against the source tree by AST.** A directive
    claiming `.. class:: Engine` under `django.template` is only believed if
    that file really defines `Engine`. Unverifiable references are dropped and
    counted, never guessed at.
  * **Lexical overlap between the query and the gold file is recorded per
    instance.** A query whose words are the file's own identifiers is the
    obvious case, and a lexical baseline will win it. The whole value of dense
    and ColBERT retrieval lives in the queries where the wording and the code
    share nothing — so pooling the two hides the only effect worth measuring.
"""

from __future__ import annotations

import ast
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# An RST section heading is a line of text underlined by punctuation. The
# underline must be at least as long as the title, which is what stops a line
# of dashes inside a table from being read as one.
UNDERLINE = re.compile(r"^([=\-`:'\"~^_*+#<>])\1{1,}\s*$")

# `.. directive:: argument`, and the subset of directives that name code.
DIRECTIVE = re.compile(r"^\.\.\s+([a-zA-Z][a-zA-Z0-9_-]*)::\s*(.*)$")
CODE_DIRECTIVES = {
    "module",
    "currentmodule",
    "class",
    "method",
    "function",
    "attribute",
    "data",
    "exception",
    "classmethod",
    "staticmethod",
    "property",
}
# Directives whose *body* is code rather than prose, so it never enters a query.
CODE_BLOCK_DIRECTIVES = {
    "code-block",
    "code",
    "sourcecode",
    "doctest",
    "literalinclude",
}

# `:class:`~django.template.Engine`` — the roles that point at code.
CODE_ROLE = re.compile(
    r":(class|func|meth|attr|mod|exc|data|obj):`~?([a-zA-Z_][a-zA-Z0-9_.]*)"
    r"(?:\s*\([^`]*\))?`"
)
# Any other role: keep the text a reader sees, drop the markup.
ANY_ROLE = re.compile(r":[a-zA-Z:+-]+:`~?([^`]+)`")
LITERAL = re.compile(r"``([^`]+)``")
TARGET_LINE = re.compile(r"^\.\.\s+_[^:]+:\s*$")

# Splits an identifier the way a reader would say it: snake_case and CamelCase
# both become words, so `get_or_set` overlaps with "get or set".
WORD = re.compile(r"[a-z0-9]+")
CAMEL = re.compile(r"(?<=[a-z0-9])(?=[A-Z])")

# Below this a "section" is a stub — a heading plus a cross-reference — and
# carries no description to retrieve from.
MIN_QUERY_CHARS = 120
# Above this the query stops being a question and becomes a chapter. Also keeps
# every query far below the vector-store token ceiling (PROTOCOL §4.2).
MAX_QUERY_CHARS = 1500

# The SHORT variant. A whole documentation section is not what anybody types
# into a search box: an agent calling the MCP `search` tool, a person at
# `mindex-search.sh`, the VS Code Ask field — all supply a handful of words.
# Measured on the long corpus, the median query is 562 characters on django and
# 1 089 on scikit-learn, so the long variant asks the retriever a question no
# caller asks, and F2 found the ColBERT rerank's sign FLIPS across exactly this
# axis (PROTOCOL §12.8). So the same sections are also emitted as short queries:
# same gold, same everything, one variable changed.
SHORT_QUERY_CHARS = 200
# Below this a short query is a fragment rather than a question.
MIN_SHORT_QUERY_CHARS = 25
# Sentence end that is not an abbreviation or a decimal: `. ` after a lowercase
# letter or a digit. Deliberately crude — this cuts prose, not code.
SENTENCE_END = re.compile(r"(?<=[a-z0-9)\]])[.!?](?:\s|$)")


@dataclass
class Section:
    """One documentation section: a heading, its prose, and the code it names."""

    doc_path: str
    heading: str
    level: int
    lineno: int
    body_lines: list[str] = field(default_factory=list)
    module_context: str | None = None


@dataclass
class Resolution:
    """Why a corpus has the instances it has. Published, never absorbed."""

    refs_total: int = 0
    refs_resolved: int = 0
    unknown_module: int = 0
    symbol_not_defined: int = 0
    ambiguous_symbol: int = 0
    empty_module: int = 0
    excluded_doc: int = 0
    sections_total: int = 0
    dropped_no_gold: int = 0
    dropped_short_query: int = 0
    kept: int = 0


# ---------------------------------------------------------------------------
# The source tree, read as an index of what is defined where
# ---------------------------------------------------------------------------


def is_test_path(rel: str) -> bool:
    """A test file is not where a documented name is defined.

    The same argument that keeps tests out of the issue tier's gold sets, and
    it was found here the same way — by reading instances. `sgd.rst` describes
    `SGDRegressor`, and scikit-learn's `linear_model/tests/test_sgd.py` defines
    a subclass of that name, so the real class became "ambiguous" and was
    dropped while `Ridge` and `Lasso` — named in the same section only as
    alternatives the reader might prefer — survived. The gold set came out
    naming everything the section was NOT about.

    Deliberately narrow: `tests/` (plural) and the two test-file spellings, and
    NOT `test/` (singular). `django/test/` is public API — `django.test.Client`
    is documented and is where that behaviour lives — and a wider rule scored
    585 references unresolvable where 389 had been, silently deleting real gold
    while fixing the ambiguity.
    """
    parts = rel.split("/")
    name = parts[-1]
    return (
        "tests" in parts
        or name.startswith("test_")
        or name.removesuffix(".py").endswith("_test")
    )


class SymbolIndex:
    """Maps dotted module paths and symbol names onto files, verified by AST.

    This is what makes the ground truth checkable rather than assumed. A doc
    directive is a claim about the code; parsing the code is how the claim is
    tested. Anything that fails to resolve is dropped and counted, because a
    gold set containing a file that does not define what the prose is about
    would score every system on a coincidence.
    """

    def __init__(self, repo_root: Path, package: str) -> None:
        self.repo_root = repo_root
        self.package = package
        self.modules: dict[str, str] = {}
        self.defs: dict[str, set[str]] = {}
        self.qualnames: dict[str, set[str]] = {}
        # Files that define at least one class or function. A file that defines
        # nothing is a re-export list, and it cannot be the answer to "where
        # does this behaviour live": measured on django, `db/models/__init__.py`
        # is 138 lines with zero definitions and was gold 146 times. Its chunks
        # are import statements, so retrieving it helps no caller either.
        self.defining: set[str] = set()
        self._build()

    def _build(self) -> None:
        root = self.repo_root / self.package
        for path in sorted(root.rglob("*.py")):
            rel = path.relative_to(self.repo_root).as_posix()
            if is_test_path(rel):
                continue
            dotted = rel[: -len(".py")].replace("/", ".")
            dotted = dotted.removesuffix(".__init__")
            self.modules[dotted] = rel
            try:
                tree = ast.parse(path.read_text(encoding="utf-8", errors="replace"))
            except (SyntaxError, ValueError):
                # A file this Python cannot parse (py2 leftovers, templates)
                # still exists as a module; it just contributes no symbols.
                continue
            self._collect(tree, rel)

    def _collect(self, tree: ast.Module, rel: str) -> None:
        for node in tree.body:
            if isinstance(node, ast.ClassDef | ast.FunctionDef | ast.AsyncFunctionDef):
                self.defs.setdefault(node.name, set()).add(rel)
                self.defining.add(rel)
            if isinstance(node, ast.ClassDef):
                for sub in node.body:
                    if isinstance(sub, ast.FunctionDef | ast.AsyncFunctionDef):
                        self.qualnames.setdefault(f"{node.name}.{sub.name}", set()).add(
                            rel
                        )

    def module_file(self, dotted: str) -> str | None:
        return self.modules.get(dotted)

    def reexported(self, name: str, module_path: str) -> tuple[str | None, str]:
        """Where `name`, named through `module_path`, is actually defined.

        A public API rarely lives in the module the documentation names:
        `sklearn.decomposition.PCA` is defined in `_pca.py` and re-exported by
        `__init__.py`. Accepted only when exactly one file in the package
        defines the name, so an ambiguous one never becomes gold.

        Returns `(None, "")` when the name is defined nowhere — the caller's
        cue to fall back to the module file itself.
        """
        owners = self.defs.get(name, set())
        if len(owners) > 1:
            # A common name defined in several places — `CharField` lives in
            # both `db/models/fields` and `forms/fields`. The dotted path
            # already says which: prefer owners under the named module's own
            # directory. Dropping these outright discarded real references.
            prefix = module_path.rsplit("/", 1)[0] + "/"
            narrowed = {o for o in owners if o.startswith(prefix)}
            if len(narrowed) == 1:
                return next(iter(narrowed)), "symbol"
            owners = narrowed or owners
        if len(owners) == 1:
            return next(iter(owners)), "symbol"
        if len(owners) > 1:
            return None, "ambiguous"
        return None, ""

    def resolve(
        self, target: str, module_context: str | None
    ) -> tuple[str | None, str]:
        """Resolve one reference to a file. Returns (path, outcome).

        Outcome is one of `module`, `symbol`, `qualname`, `unknown_module`,
        `not_defined`, `ambiguous` — reported so the drop reasons are visible.
        """
        # A whole module named outright.
        if target in self.modules:
            path = self.modules[target]
            return (path, "module") if path in self.defining else (None, "empty_module")

        # `django.template.Engine` — longest module prefix, then the symbol.
        parts = target.split(".")
        for cut in range(len(parts) - 1, 0, -1):
            dotted = ".".join(parts[:cut])
            rest = ".".join(parts[cut:])
            if dotted not in self.modules:
                continue
            path = self.modules[dotted]
            head = rest.split(".")[0]
            # Defined right here — the ordinary case, and the strongest.
            if path in self.defs.get(head, set()):
                return path, "symbol"
            if rest in self.qualnames and path in self.qualnames[rest]:
                return path, "qualname"
            resolved, outcome = self.reexported(head, path)
            if resolved or outcome:
                return resolved, outcome
            # The prose named a module and something that is not a definition
            # in it (a settings constant, a singleton's method). The module
            # file is what the prose is about — unless it defines nothing, in
            # which case it is a re-export shim and there is nothing there to
            # find.
            return (path, "module") if path in self.defining else (None, "empty_module")

        # A bare name under a `currentmodule::`.
        if module_context:
            ctx = self.modules.get(module_context)
            if ctx:
                head = target.split(".")[0]
                if ctx in self.defs.get(head, set()):
                    return ctx, "symbol"
                # The same re-export step the dotted branch takes, and omitting
                # it here was a real defect: scikit-learn writes
                # `.. currentmodule:: sklearn.decomposition` and then `:class:`PCA``,
                # so 904 of its 1966 references — 46% — resolved to a package
                # `__init__.py` that defines nothing and were dropped, when the
                # class they name is defined one file away.
                resolved, outcome = self.reexported(head, ctx)
                if resolved or outcome:
                    return resolved, outcome
                return (
                    (ctx, "module") if ctx in self.defining else (None, "empty_module")
                )

        # No module context: accept only an unambiguous package-wide name.
        owners = self.defs.get(target.split(".")[0], set())
        if len(owners) == 1:
            return next(iter(owners)), "symbol"
        if len(owners) > 1:
            return None, "ambiguous"
        return None, "unknown_module"


# ---------------------------------------------------------------------------
# Reading the documentation
# ---------------------------------------------------------------------------


def split_sections(text: str, doc_path: str) -> list[Section]:
    """Split one RST file into sections, tracking the module in scope.

    Levels come from the order in which underline characters first appear,
    which is how RST itself defines them — there is no fixed `=` then `-`
    convention to rely on.
    """
    lines = text.splitlines()
    order: list[str] = []
    sections: list[Section] = []
    current: Section | None = None
    module_context: str | None = None

    i = 0
    while i < len(lines):
        line = lines[i]
        nxt = lines[i + 1] if i + 1 < len(lines) else ""
        m = DIRECTIVE.match(line.strip())
        if m and m.group(1) in {"module", "currentmodule"} and m.group(2).strip():
            module_context = m.group(2).strip()

        is_heading = (
            line.strip()
            and UNDERLINE.match(nxt)
            and len(nxt.strip()) >= len(line.strip())
            and not line.startswith(" ")
        )
        if is_heading:
            char = nxt.strip()[0]
            if char not in order:
                order.append(char)
            current = Section(
                doc_path=doc_path,
                heading=line.strip(),
                level=order.index(char),
                lineno=i + 1,
                module_context=module_context,
            )
            sections.append(current)
            i += 2
            continue

        if current is not None:
            current.body_lines.append(line)
            # A module directive inside a section applies from there on.
            if m and m.group(1) in {"module", "currentmodule"} and m.group(2).strip():
                current.module_context = m.group(2).strip()
        i += 1

    return sections


def code_refs(body: str) -> list[tuple[str, str]]:
    """Every explicit code reference in a section: (kind, dotted target).

    `currentmodule` is deliberately absent. It sets the namespace for the
    directives that follow and asserts nothing about what the prose is about —
    and taking it as gold was measured to be actively wrong: it resolves to a
    package's `__init__.py`, which in django is a re-export shim, so 25% of the
    first corpus had gold consisting only of files that define nothing the
    prose describes. `module` stays: that one does document its module.
    """
    refs: list[tuple[str, str]] = []
    for line in body.splitlines():
        m = DIRECTIVE.match(line.strip())
        if m and m.group(1) in CODE_DIRECTIVES and m.group(1) != "currentmodule":
            target = m.group(2).strip()
            if target:
                # `cache.set(key, value)` — the signature is not part of the name.
                refs.append((m.group(1), target.split("(")[0].strip()))
    for kind, target in CODE_ROLE.findall(body):
        refs.append((kind, target))
    return refs


def shorten(query: str) -> str | None:
    """The first sentence or two of a query, up to `SHORT_QUERY_CHARS`.

    Cut at a sentence boundary rather than at a character count: a query
    truncated mid-clause is not a shorter question, it is a broken one, and the
    difference would show up as retrieval quality. Returns None when no
    boundary lands in range — better to drop the instance from the short corpus
    than to invent a question nobody would ask. The heading leads, because that
    is the part that reads like something typed into a search box.
    """
    head = query[: SHORT_QUERY_CHARS + 120]
    cuts = [m.end() for m in SENTENCE_END.finditer(head)]
    usable = [c for c in cuts if MIN_SHORT_QUERY_CHARS <= c <= SHORT_QUERY_CHARS]
    if usable:
        return query[: usable[-1]].strip()
    # No sentence ends in range. If the whole query is already short, it is its
    # own short form; otherwise there is nothing honest to emit.
    if len(query) <= SHORT_QUERY_CHARS:
        return query.strip()
    return None


def strip_for_query(body: str) -> str:
    """Everything a reader sees, minus every pointer at the answer.

    Directives, their indented bodies and code blocks are removed outright:
    they are the answer key, and a doctest line reading
    `from django.core.cache import cache` would hand the file over. Roles keep
    only their last component, which is what `~` renders and what a person
    asking the question would actually say — `Engine`, not
    `django.template.Engine`.
    """
    out: list[str] = []
    lines = body.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()
        m = DIRECTIVE.match(stripped)
        if m or TARGET_LINE.match(stripped):
            # Skip the directive and everything indented under it.
            indent = len(line) - len(line.lstrip())
            i += 1
            while i < len(lines):
                nxt = lines[i]
                if not nxt.strip():
                    i += 1
                    continue
                if len(nxt) - len(nxt.lstrip()) > indent:
                    i += 1
                    continue
                break
            continue
        if stripped.endswith("::"):
            # An RST literal block: prose line, then indented code.
            out.append(stripped[:-2].rstrip())
            indent = len(line) - len(line.lstrip())
            i += 1
            while i < len(lines):
                nxt = lines[i]
                if not nxt.strip() or len(nxt) - len(nxt.lstrip()) > indent:
                    i += 1
                    continue
                break
            continue
        if stripped.startswith((">>>", "...")):
            i += 1
            continue
        out.append(line)
        i += 1

    text = "\n".join(out)
    text = CODE_ROLE.sub(lambda m: m.group(2).rsplit(".", 1)[-1], text)
    text = ANY_ROLE.sub(lambda m: m.group(1), text)
    text = LITERAL.sub(lambda m: m.group(1), text)
    # Emphasis and link markup only. An earlier version also stripped `_`,
    # which does not mark up anything in the middle of a word and turned every
    # identifier in the prose into a non-word — `has_error` became `haserror`,
    # `label_suffix` became `labelsuffix`. That made queries harder in a way no
    # real caller would reproduce, and it corrupted the overlap measurement
    # that the whole comparison is stratified on.
    text = re.sub(r"`_{1,2}", "", text)  # trailing link markers: `text`_
    text = re.sub(r"[*`|]", "", text)
    text = re.sub(r"\s+", " ", text).strip()
    return text


# ---------------------------------------------------------------------------
# Difficulty: how much of the query is already in the file
# ---------------------------------------------------------------------------


def words(text: str) -> set[str]:
    return set(WORD.findall(CAMEL.sub(" ", text).lower()))


def file_vocabulary(index: SymbolIndex, rel_path: str) -> set[str]:
    """The words a lexical matcher would find in a file's names and path.

    Deliberately identifiers and path, not the whole file: prose comments would
    put ordinary English into every file's vocabulary and make every query look
    obvious.
    """
    vocab = words(rel_path.replace("/", " ").replace(".py", ""))
    for name, owners in index.defs.items():
        if rel_path in owners:
            vocab |= words(name)
    for name, owners in index.qualnames.items():
        if rel_path in owners:
            vocab |= words(name)
    return vocab


def lexical_overlap(query: str, gold: list[str], index: SymbolIndex) -> float:
    """Share of the query's content words that appear in the gold files' names.

    This is the axis the whole comparison rests on. A high value is the obvious
    case: the query is already spelled the way the code is, and BM25 will find
    it. A low value is what dense and ColBERT retrieval exist for. Pooling the
    two averages the only effect worth measuring into invisibility.
    """
    q = {w for w in words(query) if len(w) > 3}
    if not q:
        return 0.0
    vocab: set[str] = set()
    for path in gold:
        vocab |= file_vocabulary(index, path)
    return len(q & vocab) / len(q)


def overlap_bucket(value: float) -> str:
    if value >= 0.25:
        return "obvious"
    if value >= 0.10:
        return "mixed"
    return "non-obvious"


# ---------------------------------------------------------------------------
# Self-test. Every rule here decides what a query says and what counts as its
# answer, and a mistake in any of them produces a plausible corpus rather than
# an error — which is how the first three defects survived until someone read
# the instances. So each rule is pinned against a hand-written case.
# ---------------------------------------------------------------------------

SELF_TEST_RST = """\
Setting up the cache
====================

The cache system requires setup: tell it where cached data should live,
whether in a database or in memory. See :class:`~django.core.cache.Thing`.

.. code-block:: pycon

    >>> from django.core.cache import cache
    >>> cache.set("k", 1)

Basic usage
-----------

.. currentmodule:: django.core.cache

The basic interface is:

.. method:: cache.set(key, value)

Values with an underscore like has_error and label_suffix stay whole.
"""


def self_test() -> int:
    failures: list[str] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        print(f"  {'ok  ' if ok else 'FAIL'} {name}{'  ' + detail if detail else ''}")
        if not ok:
            failures.append(name)

    sections = split_sections(SELF_TEST_RST, "docs/topics/cache.txt")
    check(
        "two sections found",
        [s.heading for s in sections] == ["Setting up the cache", "Basic usage"],
        str([s.heading for s in sections]),
    )
    check("levels differ", sections[0].level != sections[1].level)
    check(
        "currentmodule becomes context",
        sections[1].module_context == "django.core.cache",
        str(sections[1].module_context),
    )

    body0 = "\n".join(sections[0].body_lines)
    q0 = strip_for_query(body0)
    # The doctest names the module outright; a query containing it would be
    # code-to-code matching with the answer written in.
    check("code block removed from query", "from django.core.cache" not in q0, q0[:60])
    check("prose kept", "cached data should live" in q0)
    # The role is answer key, so only its display name survives.
    check(
        "role reduced to its last component", "Thing" in q0 and "django.core" not in q0
    )

    body1 = "\n".join(sections[1].body_lines)
    q1 = strip_for_query(body1)
    check("directive removed from query", "cache.set(key, value)" not in q1, q1[:70])
    check(
        "underscores survive in prose",
        "has_error" in q1 and "label_suffix" in q1,
        q1[-60:],
    )

    refs0 = code_refs(body0)
    check("role captured as a reference", ("class", "django.core.cache.Thing") in refs0)
    refs1 = code_refs(body1)
    kinds1 = {k for k, _ in refs1}
    check("method directive captured", ("method", "cache.set") in refs1, str(refs1))
    check("currentmodule is NOT a reference", "currentmodule" not in kinds1)

    # An underline shorter than its title is not a heading — a stray line of
    # dashes inside a table would otherwise split a section in two.
    short = split_sections("A long title here\n---\n\nbody\n", "x.txt")
    check("short underline is not a heading", short == [], str(short))

    check(
        "identifier splits into words",
        words("get_or_set") == {"get", "or", "set"}
        and words("CharField") == {"char", "field"},
    )
    check(
        "overlap buckets are ordered",
        overlap_bucket(0.9) == "obvious"
        and overlap_bucket(0.15) == "mixed"
        and overlap_bucket(0.0) == "non-obvious",
    )

    resolver_self_test(check)

    print("\nself-test:", "FAILED" if failures else "PASS")
    return 1 if failures else 0


# A package shaped like the two real ones: a re-exporting `__init__.py`, the
# file that actually defines the class, and the same name defined twice in
# different subpackages. Every resolver rule below was written in response to a
# defect found by reading instances, so each has a case here.
SELF_TEST_PKG = {
    "pkg/__init__.py": "from .core import Engine\n",
    "pkg/core.py": "class Engine:\n    pass\n",
    "pkg/decomposition/__init__.py": "from ._pca import PCA\n__all__ = ['PCA']\n",
    "pkg/decomposition/_pca.py": "class PCA:\n    def fit(self):\n        pass\n",
    "pkg/db/fields.py": "class CharField:\n    pass\n",
    "pkg/forms/fields.py": "class CharField:\n    pass\n",
    "pkg/db/__init__.py": "",
    "pkg/forms/__init__.py": "",
    # A test double of the same name. Counted as a definition, it made the real
    # class ambiguous and dropped it from the gold set.
    "pkg/decomposition/tests/test_pca.py": "class PCA:\n    pass\n",
}


def resolver_self_test(check: Any) -> None:
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for rel, src in SELF_TEST_PKG.items():
            p = root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(src)
        idx = SymbolIndex(root, "pkg")

        check(
            "dotted name resolves past a re-export shim",
            idx.resolve("pkg.decomposition.PCA", None)
            == ("pkg/decomposition/_pca.py", "symbol"),
            str(idx.resolve("pkg.decomposition.PCA", None)),
        )
        # The defect this file was fixed for: scikit-learn names classes bare
        # under `currentmodule`, and the bare branch did not take this step.
        check(
            "bare name under currentmodule resolves past the shim",
            idx.resolve("PCA", "pkg.decomposition")
            == ("pkg/decomposition/_pca.py", "symbol"),
            str(idx.resolve("PCA", "pkg.decomposition")),
        )
        check(
            "a shim defining nothing is never gold",
            idx.resolve("pkg.decomposition", None) == (None, "empty_module"),
            str(idx.resolve("pkg.decomposition", None)),
        )
        check(
            "a module that defines something is gold",
            idx.resolve("pkg.core", None) == ("pkg/core.py", "module"),
            str(idx.resolve("pkg.core", None)),
        )
        # Two definitions, and the dotted path says which subpackage is meant.
        check(
            "a duplicated name is disambiguated by the named package",
            idx.resolve("pkg.forms.CharField", None)
            == ("pkg/forms/fields.py", "symbol"),
            str(idx.resolve("pkg.forms.CharField", None)),
        )
        check(
            "a duplicated name with no context is refused",
            idx.resolve("CharField", None) == (None, "ambiguous"),
            str(idx.resolve("CharField", None)),
        )
        check(
            "a test double does not make the real class ambiguous",
            idx.resolve("PCA", "pkg.decomposition")[1] == "symbol"
            and "tests" not in str(idx.defs.get("PCA")),
            str(idx.defs.get("PCA")),
        )
        check(
            "a qualified method resolves to its class's file",
            idx.resolve("pkg.decomposition.PCA.fit", None)[0]
            == "pkg/decomposition/_pca.py",
            str(idx.resolve("pkg.decomposition.PCA.fit", None)),
        )


if __name__ == "__main__":
    import sys

    sys.exit(self_test())
