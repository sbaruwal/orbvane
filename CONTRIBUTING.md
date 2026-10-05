# Contributing to Orbvane

Thanks for helping. Bug reports, ideas and pull requests are all welcome.

## Reporting a bug

[Open an issue](https://github.com/sbaruwal/orbvane/issues/new/choose) with:

- what you did, what you expected, and what happened instead;
- your Orbvane version (Orbvane → About Orbvane) and macOS version;
- for a crash, the end of `~/Library/Application Support/Orbvane/logs/panic.log`;
- for a language server, debugger or agent problem, the relevant channel of the Output panel
  (View → Output, ⇧⌘U).

## Building and testing

```bash
git clone https://github.com/sbaruwal/orbvane.git
cd orbvane
cargo build
cargo test
cargo run -- <folder> [files...]
```

You need a recent stable Rust on macOS 14 or later. A few tests drive real tools (`node`, `dlv`,
`pytest`) and are skipped when those aren't installed; tests marked `#[ignore]` use real accounts
or services and only run when asked.

To try the app without touching your own settings, point it at a scratch folder:

```bash
ORBVANE_USER_DATA=/tmp/orbvane-scratch cargo run -- <folder>
```

## Making a change

- **One branch per change**, with a commit message that says what changed and why.
- **`cargo build` with no warnings, and `cargo test` passing,** before you open a pull request.
- **Write it ourselves.** Orbvane draws its own UI and writes its own parsers, protocols and
  widgets. New crates are only for low-level pieces nobody should rewrite (windowing, GPU, font
  shaping and the like); please ask in an issue before adding one.
- **Colors come from the theme.** Use the standard theme color keys, never hard-coded colors.
- **Every action is a command** (`crates/app/src/commands.rs`), so menus, shortcuts and the
  command palette all reach it. Use the conventional names, shortcuts and command ids.
- **Settings** are declared in `crates/settings/src/schema.rs`.
- **Don't name other editors or products** in code, comments, docs, test names or commit
  messages; describe the behavior in Orbvane's own words. Tools Orbvane runs or connects to (git,
  rust-analyzer, Claude Code, Codex...) may be named in plain text.
- **Update the README** when you add a feature, a setting, a shortcut or a crate. After changing
  dependencies, regenerate the notices with `python3 tools/third_party_notices.py`.

The README's "How it's built" section lists the crates and what each one does.

## Extensions

Extensions are written in Rust with the `orbvane-extension` crate; see
`examples/extensions/word-count` and `examples/extensions/todo-tree`. They're published through
[Orbvane's registry](https://github.com/sbaruwal/orbvane-extensions).

## License

By contributing, you agree that your contributions are dual licensed under the MIT and Apache 2.0
licenses, as described in the README.
