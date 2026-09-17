# commentlint

Improve the comments that AI agents add through agent-based static analysis.

`commentlint` extracts every comment from your source files with
[tree-sitter](https://tree-sitter.github.io/), asks
[TypeSafe.ai](https://typesafe.ai) a set of yes/no questions about each one, and
fails when any comment does not meet the bar. Out of the box it checks that
comments are written in the active voice. You can add your own rules in a
per-directory config file.

## Install

### Shell installer (macOS and Linux)

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ari-becker/commentlint/releases/latest/download/commentlint-installer.sh | sh
```

### Homebrew (macOS and Linux)

```sh
brew install ari-becker/commentlint/commentlint
```

### Prebuilt binaries

Every [release](https://github.com/ari-becker/commentlint/releases) ships
archives for macOS (Apple Silicon and Intel) and Linux (x86_64 and arm64),
with checksums. Unpack one and put `commentlint` on your `PATH`.

### From source

```sh
cargo install --git https://github.com/ari-becker/commentlint
```

A C compiler is needed to build the tree-sitter grammars.

## Set up the API key

`commentlint` reads your TypeSafe.ai API key from:

- `$XDG_CONFIG_HOME/commentlint/key.txt` when `XDG_CONFIG_HOME` is set
- `$HOME/.config/commentlint/key.txt` otherwise

```sh
mkdir -p ~/.config/commentlint
echo 'your-typesafe-api-key' > ~/.config/commentlint/key.txt
chmod 600 ~/.config/commentlint/key.txt
```

The tool exits with an error if the key is missing. In CI you can set the
`COMMENTLINT_API_KEY` environment variable instead of writing the file.

## Default rules

The built-in rules and settings are defined in
[`data/defaults.yaml`](data/defaults.yaml). The file is compiled into the
binary with `include_str!` and parsed at startup, so it is the single place to
edit when changing what ships by default. Anything in it can be overridden per
repository (see [Configuration](#configuration)).

## Usage

```sh
commentlint                    # check every file tracked by Git
commentlint .                  # walk the current directory (honours .gitignore)
commentlint src/main.rs lib.py # check specific files (what pre-commit does)
commentlint --only-changed     # only files Git reports as changed
echo "Some text." | commentlint --from-stdin
```

Each failing comment is printed to stdout as `path:line:column`, pointing at
the first character of the comment, followed by the rule that failed and the
probability TypeSafe.ai returned:

```
src/parser.rs:42:5: error: comment fails rule `active_voice` (p=0.09, threshold 0.50)
    The buffer is drained by the caller before the next read.
commentlint: 1 issue(s) found
```

Exit codes: `0` when every comment passes, `1` when at least one comment fails
a rule, `2` for configuration, API, or usage errors.

Other flags:

| Flag | Meaning |
| --- | --- |
| `--only-changed` | Restrict to files that `git status` reports as modified, added, or untracked. Combines with explicit paths. |
| `--from-stdin` | Skip file parsing and evaluate the rules against the text on stdin. |
| `--list-comments` | Print the comments that would be evaluated and exit without calling the API. Useful for checking extraction. |
| `-j`, `--jobs N` | Concurrent API requests (default 8). |
| `-v`, `--verbose` | Report skipped files and per-file comment counts on stderr. |

### How comments are extracted

Consecutive single-line comments that start at the same column and use the same
delimiter are merged into one block, so a paragraph of `//` lines is judged as a
whole. Comment delimiters and leading `*` decorations are stripped before the
text is sent.

Comments are skipped when they have fewer than `min_words` words (default 3) or
start with a known tool directive such as `noqa`, `eslint-`, `type:`, or
`SPDX-`. Both lists are configurable.

Supported languages: Bash, C, C++, C#, CMake, CSS, Dart, Elixir, Elm, Erlang,
F#, Gleam, Go, Haskell, HCL (including Terraform), HTML, Java, JavaScript,
Julia, Kotlin, Lua, Nix, Objective-C, OCaml, PHP, Python, R, Ruby, Rust, Scala,
Swift, TOML, TypeScript, TSX, YAML, and Zig. Files with other extensions are
ignored.

## Configuration

Put a `.commentlint.toml` in any directory. Configuration is layered: the
built-in defaults are the base, and every `.commentlint.toml` found between
the working directory and the file's own directory is applied on top,
outermost first. The configuration closest to the file being evaluated has the final
say, so a nested directory can add, override, disable, or re-enable rules and
change thresholds for its subtree.

`.commentlint.toml` and `data/defaults.yaml` share the same keys.

```toml
# Rule ids to turn off for this directory and below.
disable = ["active_voice"]

# Re-enable rules that an outer directory disabled.
enable = []

# Default pass threshold: a rule fails when TypeSafe.ai's probability of
# "yes" is below this value. Rules can override it individually.
threshold = 0.5

# TypeSafe.ai model name.
model = "jev-latest"

# Comments with fewer words than this are not evaluated.
min_words = 3

# Comments starting with any of these are treated as tool directives and
# skipped. Setting this replaces the built-in list.
# ignore_prefixes = ["noqa", "eslint-"]

# Add a rule. Each rule is a TypeSafe.ai Noul question: `instructions` asks
# the yes/no question, `criteria.true` and `criteria.false` describe what
# each answer means.
[rules.explains_why]
instructions = "Does the comment explain why the code exists or behaves as it does, rather than restating what the code does?"
criteria = { true = "the comment gives intent, reasoning, or a constraint", false = "the comment only paraphrases the code" }
threshold = 0.6
```

### Default rules

Defined in [`data/defaults.yaml`](data/defaults.yaml):

| Id | Question | Yes means | No means |
| --- | --- | --- | --- |
| `active_voice` | Is the text written in the active voice? | the grammatical subject of the sentence is the person or thing performing the action | the grammatical subject of the sentence is the person or thing being acted upon |

Overriding a default rule is just defining a rule with the same id in a
`.commentlint.toml`.

## pre-commit

Add to your `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/ari-becker/commentlint
    rev: v0.1.0
    hooks:
      - id: commentlint
```

The `commentlint` hook builds the tool with cargo inside pre-commit's managed
environment. If you would rather use a binary you installed with Homebrew, use
`id: commentlint-system` instead. Both hooks pass the staged file names to the
tool, so only the files in the commit are checked.

## GitHub Actions

```yaml
- uses: actions/checkout@v4
- run: brew install ari-becker/commentlint/commentlint
- run: commentlint
  env:
    COMMENTLINT_API_KEY: ${{ secrets.TYPESAFE_API_KEY }}
```

## Releasing

Releases are built and published by [dist](https://github.com/axodotdev/cargo-dist).
The configuration lives in `dist-workspace.toml`; `.github/workflows/release.yml`
is generated from it, so change the config and run `dist generate` rather than
editing the workflow by hand.

Pushing a tag such as `v0.2.0` builds the binary on a native runner for each
target, creates a GitHub release with the archives, checksums, and shell
installer, and pushes the Homebrew formula to
[`ari-becker/homebrew-commentlint`](https://github.com/ari-becker/homebrew-commentlint).
That tap repository must exist, and this repository needs a
`HOMEBREW_TAP_TOKEN` secret holding a token that can write to it.

To try a release build locally:

```sh
dist plan
dist build
```

## License

MIT. See [LICENSE](LICENSE).
