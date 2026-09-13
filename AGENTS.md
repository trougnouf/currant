# AGENTS.md

Read and maintain [SPECS.md](./SPECS.md) as the project spec.

Keep the codebase clean, lean, and maintainable. If something needs refactoring, do it before proceeding with the request. If a request would add significant complexity, flag it before proceeding.

Do not use Title Case. Write headers, labels, commit messages, and prose in sentence case.

Before committing, run:

    cargo clippy --all-targets --all-features --fix --allow-dirty && cargo fmt --all

Commit directly to the current working branch (typically `main`) unless told otherwise.

Use conventional commit prefixes: `feat`, `fix`, `refactor`, `doc`, `perf`, `style`, `test`, `chore`, `revert`. Optionally scope with `type(scope): message` (e.g. `feat(tui): ...`). A change that fixes something not yet released should be a `chore`, not a `fix`. Write one meaningful line — prefix + scope + what changed, briefly why if non-obvious. Add a body only when the change has non-obvious reasoning the diff does not show.

Stage files explicitly (`git add <path> ...`), never `git add -A` or `git add .` — other agents may have unrelated work in the working tree.

Architecture: core (`cassis-core`) is pure logic (catalog, query engine, controller, scanner, scrobble). Frontends (TUI, future Android) wrap `Arc<Mutex<PlayerController>>` and fire `PlayerIntent`s. Audio is frontend-owned. The catalog is an embedded SQLite database with a read-only connection for queries (WAL mode) and a write connection for mutations.
