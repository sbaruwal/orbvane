# Word Count

An example Orbvane extension written in Rust, after the classic word count sample. It shows
how an extension's program talks to the editor through the `orbvane-extension` crate.

## Features

- The status bar counts the words (or characters, or lines) of the active Markdown or text file,
  or of the selection.
- **Word Count: Show Word Count** (⌘⌥W) shows the counts in a notification.
- **Word Count: Choose What to Count...** picks words, characters or lines (a quick pick) and saves
  it as the `wordCount.mode` setting.
- **Word Count: Count Occurrences...** asks for a word (an input box) and counts it, with a button
  that lists the lines in the Output panel.
- **Word Count: Insert Summary at Cursor** inserts the counts into the document (a workspace edit).
- A `wcnote` snippet for Markdown.

## Settings

| Setting | Default | |
|---|---|---|
| `wordCount.mode` | `words` | What the status bar counts |
| `wordCount.showInStatusBar` | `true` | Show the count in the status bar |
| `wordCount.ignoredWords` | `[]` | Words not counted |

## Trying it

Build it, then install the folder with **Developer: Install Extension from Location...**:

```sh
cargo build -p word-count
```

`main` in `package.json` points at the debug build, so after rebuilding, run **Developer: Restart
Extension Host**. A published extension would ship its binary in the folder (`"main": "bin/word-count"`).
