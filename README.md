<p align="center"><img src="assets/brand/banner.svg" alt="Orbvane" width="100%"></p>

<h1 align="center">Orbvane</h1>

<p align="center"><b>A native code editor for Mac, written in Rust.</b> Language servers, debugging,
git, terminals, tasks, tests and extensions, drawn by its own GPU toolkit.</p>

<p align="center"><i>No browser engine, no web view, no telemetry.<br>
A real Mac app: native menus and dialogs, Metal rendering, and the shortcuts your fingers already know.</i></p>

<p align="center">
  <img alt="brew: sbaruwal/tap/orbvane" src="https://img.shields.io/badge/brew-sbaruwal%2Ftap%2Forbvane-e0a93b">
  <img alt="platform: macOS" src="https://img.shields.io/badge/platform-macOS-555">
  <img alt="language: Rust" src="https://img.shields.io/badge/language-Rust-c2532a">
  <img alt="UI toolkit: our own" src="https://img.shields.io/badge/UI%20toolkit-our%20own-4f8ef7">
  <img alt="license: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-3b82f6">
  <img alt="telemetry: none" src="https://img.shields.io/badge/telemetry-none-8b5cf6">
</p>

<p align="center"><a href="https://github.com/sbaruwal/orbvane/releases/latest"><b>Download for macOS</b></a>
— signed and notarized <code>.dmg</code> · Apple Silicon · macOS 14+</p>

<p align="center">or with Homebrew: <code>brew install --cask sbaruwal/tap/orbvane</code></p>

---

The UI toolkit is our own: layout, widgets, the GPU renderer, the glyph atlas, icons and theming.
Crates are only used for the low-level pieces: `winit` (window and events), `wgpu` (Metal),
`cosmic-text` (font shaping), `ropey` (text rope), `tree-sitter` + grammars (parsing), `vte` (terminal escape-sequence parser), `regex` (search matching), `muda` (native menu bar), `rfd` (native dialogs),
`arboard` (clipboard), and `objc2`/`dispatch2` (a few direct AppKit calls).

```bash
cargo run --release -- <folder> [files...]
```

## Crates

| Crate | Purpose |
|---|---|
| `acp` | Our own Agent Client Protocol client: an agent over stdio (JSON-RPC, one message per line), requests both ways, and the session updates the Assistant shows; `mcp`: the editor's tools as a Model Context Protocol server (the `orbvane` binary in a helper mode the agent starts, forwarding over a Unix socket to the editor) |
| `app` | Platform layer (`main.rs`) and the workbench: toolbar, sidebar with its view switcher, explorer, editor groups, panel, secondary side bar with the Assistant, status bar, command palette |
| `css` | Our own CSS, SCSS and Less language server, run inside the editor: a tolerant parser, the lint rules, and completion and hovers from the browser data (with MDN descriptions), selector specificity, outline, folding, colors, references and rename for variables, classes and ids, and formatting |
| `dap` | Our own Debug Adapter Protocol client: the adapter (lldb-dap, debugpy) over stdio, and the protocol types we use |
| `jsdebug` | Our own Node.js debugger: a Debug Adapter Protocol server running in the editor that drives `node --inspect-brk` over the inspector protocol (our own WebSocket client), with conditional/hit-count breakpoints and logpoints, values formatted like js-debug, `skipFiles`, and source maps |
| `extensions` | Installed extensions: `package.json` manifests (identity, activation events, the program to run, and `contributes`: commands, keybindings, color themes, snippets, languages, settings, JSON schemas), the extensions folder, installing from a `.vsix` or a folder, and the marketplace (`gallery`: the Open VSX registry's API through `curl`, SHA-256 checked downloads) |
| `extension-api` | `orbvane-extension`, the crate extension authors use: an `Extension` trait and a `Context` for the editor's API (messages, quick picks, input boxes, the status bar, output channels, documents, edits, settings, commands, tree views, decorations, hover/completion/definition providers, diagnostics) over JSON-RPC on stdio |
| `cargo-orbvane` | `cargo orbvane package`: builds an extension in release mode for each Mac architecture and packs it into a `.vsix`; `cargo orbvane registry`: what the extension registry's CI runs (builds submitted extensions from source, writes the index) |
| `fswatch` | Watches the open folder with macOS FSEvents through our own CoreServices bindings (the Explorer, open files and git status follow changes made outside the editor) |
| `html` | Our own HTML language server, run inside the editor: a scanner and tolerant parser, completion of tags, end tags, attributes, values and paths, hovers, linked editing of tag names, folding, outline, auto-closed tags and quotes; CSS in `<style>` and `style=""` goes to the `css` crate |
| `json` | Our own JSON language server, run inside the editor: a JSONC parser that keeps positions, JSON Schema validation, and schema-driven completion (keys, values, snippets), hovers, outline, folding and formatting |
| `language` | The language registry (`languages.json`: file patterns, comments, keywords, grammar, language server; extended by the user's file); incremental tree-sitter highlighting (36 grammars, with embedded languages: `<script>`/`<style>`, Markdown code fences, HTML in PHP) with a keyword lexer for the rest |
| `lsp` | Our own Language Server Protocol client: JSON-RPC over stdio (or with a server running in-process), position encodings, the protocol types we use; `Connection` is the same transport without LSP's handshake (extensions) |
| `render` | wgpu renderer: rounded/bordered quads, text, vector icons, layers and clipping |
| `scm` | Source control through the `git` CLI: the Source Control Graph's lane layout, porcelain v2 status, branches/tags/stashes/remotes, credential prompts over a Unix socket (askpass), and every operation (stage, commit, amend, checkout, branch, merge, rebase, fetch/pull/push/sync, stash, remotes, tags, clone) on a background worker; our own Myers line diff for gutter markers |
| `settings` | `settings.json` (user: `~/Library/Application Support/Orbvane/User`, workspace: `.orbvane/settings.json`): the settings schema, layered values, and JSONC edits that keep comments and formatting |
| `search` | Project-wide search: our own .gitignore/glob matching, parallel file walker and search workers (with cancellation and a result cap), replace with `$1` groups; `regex` does the matching |
| `terminal` | Integrated terminal: our own PTY layer (`forkpty`, foreground process name and shell cwd via libproc) and xterm-compatible emulator (grid, scrollback, colors, alternate screen); `vte` only tokenizes escape sequences |
| `text` | Rope buffer with selections, multi-edit transactions (one undo step for all cursors) and undo/redo that restores every cursor |
| `theme` | color themes: ships our own Orbvane Night (the default), Orbvane Day and Orbvane Dark, evaluates the color registry defaults (generated by `tools/gen_registry.py`), maps `tokenColors` to our tokens, and loads user themes from `~/Library/Application Support/Orbvane/themes` |

## The window

The toolbar holds the sidebar toggle, a pill with the project (click: Open Recent) and its branch
(click: Checkout to...), the search field (Go to File; type `>` for commands), run (Start
Debugging, or stop while debugging), the panel and secondary side bar toggles and the gear menu. The sidebar has a row of
view buttons at its top (Explorer, Search, Source Control, Run and Debug, Extensions, Testing and
extensions' views; the ones that don't fit are behind `...`), with badges for pending changes,
test results and extension updates; clicking the active view hides the sidebar. Tabs and the
panel's tabs are rounded chips. The status bar's left side is a row of pills: sync, the active file's language
server (a check when ready, its progress while busy; click for the Output), problems and Terminal. The default color theme is our own Orbvane Night, with Orbvane Day
as its light pair and Orbvane Dark, a deep, quiet graphite dark (Preferences: Color Theme). Everything, the
interface included, is set in SF Mono, the copy built into macOS (`editor.fontFamily`; Menlo is
the fallback). `workbench.interfaceFont`: `system` puts the interface back in the Mac's system
font.

The Explorer is headed by the project's name. The secondary side bar on the right (⌥⌘B, or its
toolbar button) holds the Assistant and the active file's Outline and Timeline; drag its edge to
resize it.

## Assistant

The Assistant is a chat with a coding agent you choose: any program that speaks the
[Agent Client Protocol](https://agentclientprotocol.com) over stdio. Set the command that starts
it in Settings (`assistant.agent.command`; it runs through your login shell, in the open
folder), then ask away (⇧⌘I goes to the message box, Enter sends, Esc stops the answer).

- The file you're in (and the selection) goes with each message; click its chip to leave it out
  (`assistant.sendActiveFile` turns this off).
- Replies stream in with the agent's thoughts, its plan and the tools it runs, each with its
  status.
- The agent reads files through the editor, so it sees unsaved changes. Its edits go into open
  documents as one undo step (saved, unless they had unsaved changes of their own); other files
  are written to disk.
- Before it changes something or runs a command, it asks: the choices are buttons in the chat,
  and Review Changes opens the proposed change in a diff tab.
- Agents that need a sign-in show their sign-in choices; New Chat (the + button) starts over.
  What the agent prints goes to the Output panel's Assistant channel.
- The editor gives the agent tools of its own (`assistant.editorTools`), as an MCP server it
  starts: the Problems the editor shows, and from the language servers Go to Definition, Find
  All References, Hover, a file's outline and workspace symbol search. So the agent sees what
  you see, unsaved changes included, without building the project itself.

## Keyboard

| Shortcut | Action |
|---|---|
| ⇧⌘P / ⌘P | Command palette / Go to file |
| ⌘, | Settings (search, User/Workspace, native dropdowns; the file icon opens `settings.json`) |
| ⌘K ⌘T | Color theme picker (↑/↓ previews, Esc restores) |
| ⌘B / ⌘J / ⌥⌘B | Toggle side bar / panel / secondary side bar |
| ⇧⌘I | Ask the agent (the Assistant's message box) |
| ⇧⌘X | Extensions (installed and popular extensions, marketplace search, their pages, Install from VSIX in the "..." menu) |
| ⌘K Z | Zen Mode (full screen, just the editor; Esc Esc leaves) |
| ⌘K M | Change Language Mode (also: click the language in the status bar) |
| ⌘\ , ⌘1–3 | Split editor / focus group |
| ⌥Z | Toggle word wrap |
| ⌥⌘[ / ⌥⌘], ⌘K ⌘L | Fold / unfold / toggle fold (also chevrons in the gutter) |
| ⌘K ⌘0 / ⌘K ⌘J | Fold all / unfold all |
| ⇧⌘\ | Go to bracket |
| ⌘K Enter | Keep editor (pin a preview tab) |
| ⌘S, ⌘W, ⌘N, ⌘O | Save, close editor, new file, open folder |
| ⇧⌘S / ⌥⌘S | Save as / save all (File > Revert File drops unsaved changes) |
| ⌥⌘T, ⌘K U, ⌘K W, ⌘K ⌘W | Close other editors / saved editors / the group's editors / all editors (right-click a tab for more) |
| Enter, ⌘⌫, ⌘↓ (in the Explorer) | Rename, move to Trash, open (also right-click: new file/folder, cut/copy/paste, copy path...) |
| ⌥⌘C / ⌥⇧⌘C / ⌥⌘R | Copy path / relative path, reveal in Finder |
| ⌘/ | Toggle line comment |
| ⌥-click, ⇧⌥-drag | Add/remove a cursor, column (box) selection |
| ⌥⌘↑ / ⌥⌘↓ | Add cursor above / below |
| ⌘D, ⌘K ⌘D, ⇧⌘L | Add next occurrence, move last selection to next occurrence, select all occurrences |
| ⌥⇧I | Add cursors to line ends |
| Esc | Back to a single cursor |
| F12 / ⌘-click | Go to definition (on a laptop keyboard F12 may need fn, or System Settings > Keyboard > "Use F1, F2, etc. keys as standard function keys") |
| Right-click (in the editor) | Go to Definition / References, Peek, Rename Symbol, Quick Fix, Format, Cut / Copy / Paste, Command Palette |
| F2 | Rename symbol |
| ⇧⌘F2 | Start linked editing (edit both tag names of an element at once) |
| ⇧F12 / ⌥F12 | Go to references / peek definition (several results open the peek view: ↑/↓, Enter, Esc) |
| ⌥⇧H | Peek call hierarchy (callers or, with the header's arrow, callees; →/← expand); in an open call or type hierarchy peek, switch its direction |
| (palette) | Peek Type Hierarchy: subtypes or supertypes of the type at the cursor |
| ⌘. | Quick fix / code actions |
| ⇧⌘O / ⌘T | Go to symbol in editor (`@`) / in workspace (`#`) |
| ⌃G | Go to line/column (`:`) |
| ⇧⌘Space | Trigger parameter hints |
| ⌃R | Open a recent folder or workspace (also File > Open Recent) |
| ⌘K ⌘S | Keyboard Shortcuts: pick a command and press its new key (saved to `keybindings.json`) |
| ⇧⌥F / ⌘K ⌘F | Format document / selection |
| ⌃Space | Trigger suggestions |
| ⌃\` / ⌃⇧\` | Toggle terminal / new terminal |
| ⌘\ (in terminal) | Split terminal |
| ⌥⌘← / ⌥⌘→ (in terminal) | Previous / next split terminal |
| ⌘K (in terminal) | Clear terminal |
| ⌘F / ⌥⌘F | Find / replace in the editor (Enter / ⇧Enter or ⌘G / ⇧⌘G to step, ⌘Enter replaces all) |
| ⇧⌘F / ⇧⌘H | Search / replace in files |
| ⌘Enter (in the Search view) | Open the results in a Search Editor (also "Search Editor: New Search Editor") |
| ⇧⌘R / ⌥⌘L / ⇧⌘⌫ (in a Search Editor) | Search again / toggle context lines / delete the file's results |
| ⌃⇧G, then ⌘⏎ | Source Control, commit |
| ⌘K ⌘⌥S / ⌘K ⌘N / ⌘K ⌘R | Stage / unstage / revert the selected lines' changes |
| Click the branch in the toolbar / the sync item in the status bar | Checkout to... / sync (or publish) |
| F7 / ⇧F7 | Next / previous difference (in a diff) |
| ⌥⌘C / ⌥⌘W / ⌥⌘R | Toggle match case / whole word / regex (in find and search) |
| F5 / ⌃F5 | Start debugging (continue when stopped) / run without debugging |
| ⇧F5 / ⇧⌘F5 / F6 | Stop / restart / pause |
| F10 / F11 / ⇧F11 | Step over / into / out |
| F9, click the glyph margin | Toggle breakpoint (right-click for conditions, hit counts and logpoints) |
| ⇧⌘V / ⌘K V | Markdown preview / preview to the side (updates as you type) |
| ⇧⌘D / ⇧⌘Y | Run and Debug view / Debug Console |
| ⇧⌘B | Run build task (Terminal > Run Task... for any task) |
| ⌘; A / ⌘; C / ⌘; F | Run all tests / the test at the cursor / the current file's tests |
| ⌘; ⌘C / ⌘; L / ⌘; E | Debug the test at the cursor / rerun the last run / rerun failed tests |
| ⌘; ⌘X / ⌘; ⌘R / ⌘; ⌘O | Cancel the test run / refresh tests / show Test Results |

## Languages

48 languages are built in (`crates/language/languages.json`): the file names and
extensions they cover, comment tokens (Toggle Line Comment), keywords for highlighting when there
is no tree-sitter grammar, and the language server to start. Add or change languages in
`~/Library/Application Support/Orbvane/User/languages.json` (read at startup), for example:

```jsonc
{
  "languages": [
    // Headers are C++ here.
    { "id": "cpp", "extensions": [".h", ".hpp", ".cpp"] },
    // A server of your own, with initialization options and workspace/configuration answers.
    { "id": "rust", "languageServer": { "command": "ra-multiplex", "args": ["client"] } },
    // A new language.
    { "id": "gleam", "name": "Gleam", "extensions": [".gleam"], "lineComment": "//",
      "keywords": ["fn", "pub", "let", "type"], "languageServer": { "command": "gleam", "args": ["lsp"] } }
  ]
}
```

Fields: `name`, `aliases`, `extensions`, `filenames`, `lineComment`, `blockComment`, `keywords`,
`controlKeywords`, `constants`, `grammar` (a built-in tree-sitter grammar, below), and `languageServer`
(`command`, `args`, `initializationOptions`, `settings`, `install`: the command that installs it,
offered when it's missing; `heavy`: keep it running when idle; `null` turns the server off). Change
Language Mode (⌘K M) switches a file's language by hand.

Built-in grammars: `rust`, `python`, `go`, `c`, `cpp`, `javascript`, `typescript`, `tsx`, `json`,
`toml`, `bash`, `html`, `css`, `scss`, `yaml`, `xml`, `java`, `c_sharp`, `ruby`, `swift`, `lua`,
`sql`, `make`, `php`, `kotlin`, `zig`, `scala`, `haskell`, `elixir`, `markdown`, `dockerfile`,
`dart`, `diff`, `ini`, `objc`, `r`. Languages embedded in others are highlighted with their own
grammar: scripts and styles in HTML, HTML in PHP and Markdown, and Markdown code fences (by the
fence's language name, alias or extension).

## JSON

JSON and JSON with Comments are handled by a built-in language server (`"command": "builtin:json"`
in `languages.json`; no install needed). The editor's own files have schemas: user and
workspace `settings.json`, `keybindings.json`, `languages.json`, `.orbvane/launch.json`,
`.orbvane/tasks.json` and `*.code-workspace`. Schemas give completion of keys and values (with
descriptions, defaults and snippets such as new launch configurations), hovers and warnings (unknown
settings, wrong types, values not allowed). A file's own `"$schema"` can name a schema file on disk.
Every JSON file gets syntax errors, duplicate key warnings, the outline, folding and Format Document.

## HTML and CSS

HTML, CSS, SCSS and Less have built-in language servers (`builtin:html`, `builtin:css`; no install
needed), our own. They use MIT-licensed browser data with descriptions from MDN Web Docs (CC-BY-SA
2.5; see `THIRD-PARTY-NOTICES.md`), regenerated by `crates/css/tools/update_data.py`.

- **HTML:** completion of tags after `<`, the open element's end tag after `</`, attributes (the
  element's own first, then global ones, then event handlers; values open right after), attribute
  values, and file paths in `src`/`href`. Typing `>` closes the tag (`html.autoClosingTags`) and `=`
  adds quotes (`html.autoCreateQuotes`). Also hovers with MDN links, linked editing of start and end
  tag names (`editor.linkedEditing`), folding (elements, comments, `<!-- #region -->`) and the outline
  (`div#id.class`). CSS in `<style>` and `style=""` gets the CSS features below.
- **CSS/SCSS/Less:** completion of properties (then their values), values, colors, units, at-rules,
  pseudo-classes and -elements, HTML tags in selectors and variables; hovers (with a selector's
  specificity); the lint warnings (unknown properties, empty rulesets, vendor prefixes, properties
  `display` ignores, ...); color swatches and the color picker; outline, folding, go to definition,
  references and rename for variables, classes and ids; Format Document.

## Language servers

A language's server starts when a file of that language is shown (one per workspace folder: files
outside it, like a library's source reached with Go to Definition, use the same server); tabs restored in the background
start nothing until you look at them. If the server isn't installed, a notification says so:
**Install** runs its install command in a terminal and starts the server when that succeeds (no
restart), or **Copy Install Command** to run it yourself. Go to Definition and Go to References
bring the notification back. Servers are looked up on the same PATH a terminal gets: started from
the Finder or the Dock, Orbvane asks your login shell for it (so a `PATH` set in `~/.zshrc`
counts), plus the usual Homebrew, Cargo, Go and Volta folders.

Servers hold memory while they run (rust-analyzer keeps about 2.5 GB for this repository), so
servers whose files haven't been shown for 10 minutes stop, and start again when one is shown
(`languageServers.stopWhenIdle`, `languageServers.idleMinutes`). Servers that take long to load
a project (rust-analyzer, jdtls, metals, kotlin-language-server, haskell-language-server; `"heavy":
true` in `languages.json`) keep running unless the setting is `all`. **Developer: Restart Language
Server** restarts the active file's server (one that crashed too), and **Developer: Stop Language
Servers** stops them all until restarted.

Started automatically when installed (from the language's
`languageServer`): `rust-analyzer`
(Rust), `gopls` (Go), `clangd` (C/C++/Objective-C), `pyright-langserver` (Python),
`typescript-language-server` (JS/TS/JSX/TSX), `sourcekit-lsp` (Swift),
`yaml-language-server`, `taplo` (TOML), `bash-language-server`, `jdtls` (Java), `csharp-ls`,
`ruby-lsp`, `lua-language-server`, `intelephense` (PHP), `kotlin-language-server`, `dart`, `zls`,
`docker-langserver`, `metals` (Scala), `elixir-ls`, `haskell-language-server`. Features: diagnostics
(squiggles, Problems panel, status bar), hover, go to definition, go to references, completion,
signature help (parameter hints while typing a call; `editor.parameterHints.enabled`), semantic
highlighting (`editor.semanticHighlighting.enabled`), inlay hints
(inline types and parameter names; `editor.inlayHints.enabled`), rename
, code
actions (quick fixes and refactorings in a native menu, from ⌘. or the gutter lightbulb that appears when
the cursor rests where the server has some; `editor.lightbulb.enabled`), formatting (also on save with
`editor.formatOnSave`) and the Outline (in the secondary side bar): the file's symbols as a tree that follows the
cursor, with Collapse All, Follow Cursor and Sort By (position, name, category) in its header. Click it
to use it from the keyboard: the arrows move through the symbols (the editor follows), ←/→ collapse and
expand, Enter goes to the symbol, and typing finds symbols by name. The Timeline tab next to it lists
the commits that changed the active file; clicking one shows what it changed. The breadcrumbs
show the symbols at the cursor after the file (`breadcrumbs.symbolPath`); clicking one lists the symbols
next to it. Server logs
appear in the Output panel.

## Settings

The Settings editor and `settings.json` understand these settings:
`workbench.colorTheme`, `editor.fontSize`, `editor.fontFamily`, `editor.lineHeight`,
`editor.tabSize`, `editor.insertSpaces`, `editor.lineNumbers`, `editor.renderLineHighlight`,
`editor.renderWhitespace`, `editor.autoClosingBrackets`, `editor.guides.indentation`,
`editor.scrollBeyondLastLine`, `editor.cursorBlinking`, `editor.cursorStyle`,
`editor.cursorWidth`, `editor.minimap.enabled`, `editor.minimap.showSlider`, `files.autoSave`,
`files.autoSaveDelay`, `files.insertFinalNewline`, `files.trimFinalNewlines`,
`editor.formatOnSave`, `editor.bracketPairColorization.enabled`, `editor.matchBrackets`, `editor.folding`, `editor.showFoldingControls`, `editor.wordWrap`, `editor.wordWrapColumn`, `editor.wrappingIndent`, `files.trimTrailingWhitespace`, `files.hotExit`, `files.refactoring.autoSave`, `window.restoreWindows`, `workbench.editor.enablePreview`, `workbench.editor.enablePreviewFromQuickOpen`, `workbench.tree.indent`, `terminal.integrated.fontSize`,
`terminal.integrated.lineHeight`, `scm.diffDecorations`, `git.autofetch`, `git.autofetchPeriod`,
`git.confirmSync`, `git.confirmForcePush`, `git.enableSmartCommit`, `git.mergeEditor`, `git.postCommitCommand`,
`git.pruneOnFetch`, `git.rebaseWhenSync`, `search.searchEditor.doubleClickBehaviour`,
`search.searchEditor.reusePriorSearchConfiguration`, `search.searchEditor.defaultNumberOfContextLines`,
`search.searchEditor.focusResultsOnSearch`, `testing.gutterEnabled`, `testing.defaultGutterClickAction`,
`testing.openTesting`, `testing.countBadge`, `html.autoClosingTags` and `html.autoCreateQuotes`.

## Editor

Images (PNG, JPEG, GIF, TIFF, BMP, WebP, HEIC, ICO) open in an image preview, decoded by macOS's
ImageIO: at 100% or shrunk to fit, click to zoom in and ⌥-click to zoom out; the status bar shows
the size.

Code lenses (`editor.codeLens`) show above their lines, like rust-analyzer's "▶ Run | Debug" over
`main` and tests and "N implementations" over types: Run runs it in a task terminal, Debug builds it
and debugs it with lldb-dap, and reference counts open the references.

Sticky scroll (`editor.stickyScroll.enabled`, up to `editor.stickyScroll.maxLineCount` lines) keeps
the lines that open the blocks around the top of the view pinned there; clicking
one goes to it.

## Debugging

Run and Debug (⇧⌘D) starts the configurations in `.orbvane/launch.json`, the standard format with its
`${workspaceFolder}`-style variables. `"type": "lldb-dap"` debugs native programs (Rust, C, C++,
Swift) with `lldb-dap`, found on the PATH or in Xcode, or set with `lldb-dap.executable-path`;
`"type": "debugpy"` debugs Python with `debugpy.adapter`, run by `python` or the workspace's
`.venv`/`venv`/`env` interpreter (else `python3`); `"type": "go"` debugs Go
with Delve (`dlv dap`; `mode` `auto`, `debug`, `test`, `exec`, or attach `local`/`remote`); and
`"type": "node"` debugs Node.js with our own adapter (launch with `program`, `args`,
`runtimeExecutable`, `runtimeArgs`, `env`, `stopOnEntry`, `skipFiles`, `outputCapture`, or attach
to `node --inspect` by `port`), source maps included, so breakpoints in TypeScript work for
compiled code. Without a launch.json, F5 offers to create one (for a Cargo package it builds with
the `rust: cargo build` task first, as its `preLaunchTask`, and launches the debug build; for
Node it runs package.json's `main`). While stopped: the Variables, Watch, Call Stack and Breakpoints
sections, the floating debug toolbar, values on hover, and the Debug Console, which shows the
program's output and evaluates expressions in the focused frame. Breakpoints can have a condition, a
hit count or a log message (logpoints), move with your edits, and are kept per folder. macOS asks
once for permission to debug other processes ("Developer Tools Access"); allow it, or run
`sudo DevToolsSecurity -enable`.

## Multi-root workspaces

A window can hold several folders, like the standard multi-root workspaces: File > Add Folder to
Workspace... (a window with one folder becomes an "Untitled (Workspace)"), Remove Folder from
Workspace on a folder's context menu, and Save Workspace As... / Open Workspace from File... for
`.code-workspace` files (folders with an optional `name`, plus workspace `settings`;
edits to the file made elsewhere are picked up). The Explorer shows each folder as a root; search,
Go to File, the file watcher, language servers and test providers cover every folder, and labels
say which folder a file is in. Source Control lists each folder's repository under Repositories
and shows the one the active editor belongs to.

## Search Editor

Search results in an editor tab, like the standard Search Editor ("Search Editor: New Search
Editor", or "Open in editor" / ⌘Enter in the Search view): the query with Match Case, Whole Word
and Regex, context lines around each match, and files to include/exclude, above results that are
an ordinary document, highlighted with each file's language. Double-clicking a result opens it;
⌘S saves the search as a `.code-search` file,
which opens as a search editor again.

## Extensions

Extensions are written in Rust. An extension is a folder with a `package.json`
(`name`, `publisher`, `version`, `activationEvents`, `contributes`) and, for code, a program named
by `main`. The editor starts the program when one of its activation events happens
(`onCommand:`, implicit for contributed commands; `onLanguage:`; `workspaceContains:`;
`onStartupFinished`; `*`). Each extension runs in its own process and talks to the editor over
JSON-RPC on stdin/stdout. The `orbvane-extension` crate (`crates/extension-api`) wraps that:
implement `Extension` and call `orbvane_extension::run`. Through its `Context` an extension can
show notifications (with buttons), quick picks and input boxes, set status bar items (with
`$(icon)`s), write to Output channels, read documents and the active editor, apply edits, read and
change settings, and run commands. It hears documents opening, changing, saving and closing, the
active editor and selection changing, and settings changing.
It can also fill tree views in the sidebar (its own view in the sidebar's switcher or Explorer sections,
with title and inline buttons and context menus from `menus`, shown by `when` clauses), decorate
editor text (backgrounds, borders, colors, underlines, text before/after, overview ruler marks,
hover messages), provide hovers, completions and definitions for documents matching a selector
(joined with the language server's answers), and report diagnostics to the Problems panel.
`examples/extensions/word-count` is an example (word count sample, in Rust), and
`examples/extensions/todo-tree` uses the views, decorations, providers and diagnostics (TODO and
FIXME comments of the workspace).

To publish one, `cargo install --path crates/cargo-orbvane` once, then run
`cargo orbvane package` in the extension's folder: it builds the program in release mode
(`--target arm64|x64|all`, default this Mac's) and writes `<name>-<version>.vsix` with the program in
`bin/<arch>/` (`orbvane.main` = `bin/${arch}/<program>`). Everything else in the folder goes in
except the sources, `target/`, Cargo's files and what `.orbvaneignore` leaves out.

What a `package.json` contributes works whether or not the extension has a program: commands
(palette), keybindings, color themes, snippets, languages (with our `grammar` and `languageServer`
fields, so a language server extension needs no code), settings (their own section in the Settings
editor) and `jsonValidation` schemas. Open VSX extensions whose code is JavaScript can be installed
for these too; their code doesn't run.

The Extensions view (⇧⌘X) lists the installed extensions, Orbvane's own registry (ORBVANE) and
Open VSX's most popular extensions. Extensions come from two places:

- **Orbvane's registry** ([sbaruwal/orbvane-extensions](https://github.com/sbaruwal/orbvane-extensions)):
  extensions written for Orbvane, submitted by pull request (a
  repository and a commit) and built from source for both Mac architectures by the registry's CI.
  The editor fetches its `index.json` once and searches it locally; installs are checked against
  the index's SHA-256.
- **[Open VSX](https://open-vsx.org)**, an open registry of editor extensions. Their `package.json`
  contributions (themes, snippets, languages, settings, schemas) work; their JavaScript doesn't
  run, so rows say "Open VSX extension" (installed: "Contributions only" when they have code).

Typing searches both, Orbvane's registry first, with the
`@category:"..."` and `@sort:installs|rating|updateDate` filters; `@installed`, `@enabled`,
`@disabled` and `@updates` filter the installed extensions instead. Rows show installs, rating and
verified publishers, with Install, Update or a gear menu (Enable, Disable, Uninstall, Copy
Extension ID). Clicking one opens its page: a header with what it contributes and its buttons,
above its README. Downloads take the build for this Mac when there is one and are checked against
the registry's SHA-256. Extensions live in `~/Library/Application Support/Orbvane/extensions`.

Extensions from the marketplace are checked for updates at startup and every 12 hours
(`extensions.autoCheckUpdates`) and updated automatically (`extensions.autoUpdate`); with
automatic updates off, the Extensions icon shows how many are outdated. Extensions: Check for
Extension Updates and Extensions: Update All Extensions do it by hand. Extensions: Install from
VSIX... unpacks a `.vsix`; Developer: Install Extension from Location... uses a folder where it is
(for developing one); Developer: Restart Extension Host stops every extension and reads them again
(after a rebuild). Languages an extension adds take effect after a restart.

Notifications appear like the standard toasts in the bottom right corner (Escape hides them); the Output
panel has a channel dropdown (Language Servers, then each extension's channels).

## Testing

The Testing view (the beaker in the sidebar's switcher) is Test Explorer: the folder's tests as
a tree with each one's state, a filter, a summary of the last run ("3/4 tests passed"), and Run,
Debug and Go to Test on each row. The editor's glyph margin shows a run icon (or the last result) on
each test; clicking it runs the test, right-clicking offers Debug. Failures show their message after
the failing line, the switcher counts them, and Test Results (a panel tab) has the run's output.

Tests come from test providers (`crates/app/src/testing`), like the standard test controllers: the
editor only draws what a provider reports. The first provider is rust-analyzer's test explorer, for
Cargo workspaces: it finds packages, modules and tests without building, runs them with cargo, and
debugs a test by building it and starting lldb-dap on it. Go modules get packages, files, tests,
benchmarks and fuzz tests read from `*_test.go` (subtests appear as they run), run with
`go test -json` and debugged with Delve; Go files also get "run test | debug test" code lenses.
Python folders with pytest tests get directories, files, classes, functions and parametrized
cases from `pytest --collect-only`, run with the workspace's interpreter (`.venv`, `venv`, `env`,
else `python3`) and debugged with debugpy.

## Tasks

Terminal > Run Task... and Run Build Task (⇧⌘B) run the tasks in `.orbvane/tasks.json` (`shell`, `process` and `cargo` tasks, `args`,
`options.cwd`/`env`, `group`, `osx` overrides) and detected tasks: `rust: cargo build`/`check`/`test`/`run`/`clean` for a Cargo
package, `go: build package`/`test package`/`build workspace`/`test workspace` for a Go module, and
`npm: install` plus one per `package.json` script (run with npm, yarn, pnpm or bun, after the lock file).
Each runs in a terminal of its own through your login shell, which stays open with the output until
a key is pressed. A launch configuration's `preLaunchTask` runs before debugging starts.

## Large files

Files over 20 MB or 300,000 lines open instantly and stay responsive: they're
highlighted by our line lexer, which only looks at the lines on screen, and folding, word wrap, the git gutter and language servers are off for them. A
keystroke in a million-line file costs well under a millisecond.

## Colors

Color values get a swatch in front of them, like the standard color decorators: from the language
server (`textDocument/documentColor`) and, when no server provides colors
(`editor.defaultColorDecorators`), from the text itself (`#rgb`, `#rrggbb`, `#rrggbbaa`,
`rgb()`/`rgba()`, `hsl()`/`hsla()`, in any file). Clicking a swatch opens the color picker:
drag in the saturation box, the opacity strip or the hue strip, click the color's text to switch
between rgb, hsl and hex, or the original color on the right to go back. `editor.colorDecorators`
turns them off, and `editor.colorDecoratorsLimit` caps them per file.

## Linked editing

With `editor.linkedEditing` on (or once, with Start Linked Editing, ⇧⌘F2), ranges that belong
together are edited together: rename an element's opening tag and its closing tag follows. The
ranges come from the language server (`textDocument/linkedEditingRange`), or for JSX from the
syntax tree. Typing something a name can't hold (a space), Escape, or moving away ends it; undo
takes back both edits at once.

## Emmet

In HTML, XML, JSX/TSX and CSS/SCSS/Less files, an Emmet abbreviation before the caret is offered
in the suggest widget, like the standard built-in Emmet: `ul>li.item$*3`, `div#main>(header+footer)`,
`a[href=/x]{Home}`, `!` for an HTML page, `lorem20`, and in a CSS rule `m10`, `p10-20`, `db`,
`pos:a`, `c#f.5`. Accepting it expands it with tab stops at the empty places. Emmet: Expand
Abbreviation (Edit menu) expands without the list, and Emmet: Wrap with Abbreviation asks for one
and wraps the selection (or the line) in it. `emmet.showExpandedAbbreviation` controls where it's
suggested, `emmet.triggerExpansionOnTab` expands on Tab, and `emmet.excludeLanguages` (in
settings.json) turns it off per language. The engine is a port of Emmet (MIT); its snippet and
lorem ipsum data are in `crates/app/src/emmet/data/`.

## Snippets

Completions from language servers can be snippets (`sum_of(${1:a}, ${2:b})`): Tab and ⇧Tab move
between the placeholders, a placeholder used twice is edited in both places, and Escape leaves the
snippet. Your own snippets go in `~/Library/Application Support/Orbvane/User/snippets/`, in VS
Code's format (`rust.json`, or `*.code-snippets` with a `scope`); they show up in completion and in
Snippets: Insert Snippet.

## Keybindings

Shortcuts can be changed in `~/Library/Application Support/Orbvane/User/keybindings.json`, in VS
Code's format: `{ "key": "cmd+k cmd+m", "command": "editor.action.toggleMinimap" }`, with
`"-command"` to remove a default. Later entries win; `when` clauses are ignored. Preferences: Open
Keyboard Shortcuts (⌘K ⌘S) records a key for a command, and ...(JSON) opens the file.

## Sessions

Orbvane reopens the last folder (or workspace) with its editors (selections and scroll
positions), layout, expanded explorer folders, the secondary side bar, breakpoints and watch expressions,
and restores the window size and position.
Quitting (⌘Q or closing the window) doesn't ask about unsaved changes: they are kept, untitled
files too, and come back unsaved next time (`files.hotExit`). Each folder or workspace remembers
its own editors. State lives in `~/Library/Application Support/Orbvane/State`.

## Git

The Source Control view, the status bar and the `Git:` commands cover the everyday
workflow: commit (also Amend, Commit & Push, Commit & Sync, Undo Last Commit), branches
(checkout picker, create, create from, rename, delete, publish), merge and rebase (with abort
and continue), fetch / pull / push / sync, stashes, remotes, tags and clone. The "..." menu in
the view has them all. Merge conflicts are highlighted in the editor with Accept Current /
Incoming / Both actions (and `Merge Conflict:` commands); stage the file to mark it resolved.
**Resolve in Merge Editor** (the button on a conflicted file, "Git: Resolve in Merge Editor", or
clicking a Merge Changes file with `git.mergeEditor` on) opens the three-way merge editor:
the incoming and current versions side by side with their commits, and the result below as the
editable file. Each conflict has Accept Incoming / Accept Current / Accept Combination / Ignore
actions and checkboxes; the result shows what it took (Remove Incoming...) and how many
conflicts remain. Changes only one side made are already merged. `Merge Editor:` commands
accept all from either side, reset the result and go to the next unhandled conflict. Complete
Merge saves and stages the file (with conflict markers around anything left unhandled, if you
choose to go ahead).
When git or ssh needs a username, password, passphrase or a host key confirmation, it asks in
the quick input (passwords are masked); git's credential helpers and ssh-agent are
used first. Automatic fetches never prompt.

## Updates

Orbvane keeps itself up to date. Shortly after starting and every 12 hours it asks GitHub for the
latest release; a newer one is downloaded in the background, checked against the checksum the
release lists, and checked to be signed by the same developer (and with the same bundle id) as
the copy that's running, so a damaged or tampered download is never installed. Then a notification
offers **Update Now** (Orbvane restarts into the new version, keeping your session), **Later**
(the update is installed when you quit) or **Release Notes**. **Orbvane > Check for Updates...**
checks right away, and so does the gear at the right of the toolbar (its menu also has the
Command Palette, Settings, Extensions, Keyboard Shortcuts and Themes); while an update waits for a
restart, the gear shows a badge and its menu offers **Restart to Update**.

`update.mode` (Settings > Application > Update) turns this down: `manual` checks only when you
ask, `none` never. A copy that can't replace itself (one run from the disk image or Downloads, or
built from source) offers the download page instead. If Orbvane's folder isn't writable, macOS
asks for an administrator's password to install the update.

## Privacy

Orbvane has no telemetry. It goes online only for extensions and its own updates: searching,
icons, READMEs, downloads and update checks (at startup and every 12 hours,
`extensions.autoCheckUpdates`) talk to [open-vsx.org](https://open-vsx.org) and to GitHub, where
Orbvane's extension registry lives, and the app's update check asks GitHub for the latest release
(`update.mode`: `none` turns it off). Everything else (git, language servers, debuggers, tasks)
runs on your Mac. The Assistant runs only the agent you set up; what that agent sends where is
up to it.

## License

Orbvane is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in Orbvane, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.

Third-party material and the licenses of the crates Orbvane is built from are listed in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) (regenerate it with
`python3 tools/third_party_notices.py` after changing dependencies).
