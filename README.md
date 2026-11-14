# vsh

A small zsh-flavoured shell written in Rust, with the common coreutils
built in. It runs natively on macOS, Linux and Windows. On Windows there is no
MSYS or Cygwin layer: `ls`, `grep`, `cat` and the rest run inside the shell
process.

```
cargo build --release
./target/release/vsh                 # interactive
./target/release/vsh script.sh a b   # run a script
./target/release/vsh -c 'ls | wc -l'
```

## Command lookup

A command name is resolved in this order:

1. aliases (expanded while parsing)
2. shell functions
3. shell builtins (`cd`, `export`, `read`, `source`, ...)
4. builtin utilities (`ls`, `grep`, `sort`, ...)
5. `$PATH`, as a last resort (with `%PATHEXT%` on Windows)

Ways around the order:

- A name with a `/` in it (`/bin/ls`, `./ls`) always runs that file.
- `command ls` skips functions and the builtins, and runs the `$PATH` program.
  If there is none, it falls back to the builtin utility.
- `builtin ls` forces the builtin.

`type`, `which`, `whence` and `where` show what a name resolves to.

## Language

The syntax is POSIX sh plus the zsh features that scripts use most:

- Pipelines (`|`, `|&`), lists (`;`, `&&`, `||`, `&`), `!`, `{ }`, `( )`.
- `if`, `while`, `until`, `for x in`, `for x (a b)`, `for ((;;))`, `repeat N`,
  `case` (with `;&`), and functions (`f() {}` and `function f {}`) with `local`.
- `[[ ]]` with glob `==`, regex `=~` (sets `$MATCH`, `$match`, `$BASH_REMATCH`),
  and file and number tests. `(( ))`, `$(( ))`, `let`.
- Quoting: `'...'`, `"..."`, `$'...'`, and backslash escapes.
- `$(...)` and backticks, here-documents (`<<`, `<<-`, quoted delimiters), and
  here-strings (`<<<`).
- Redirections: `<`, `>`, `>>`, `>|`, `<>`, `2>&1`, `>&2`, `&>`, `&>>`, `n>&-`.
- Parameter expansion:
  - `${x:-y}`, `${x:=y}`, `${x:?msg}`, `${x:+y}`
  - `${#x}`, `${x#p}`, `${x##p}`, `${x%p}`, `${x%%p}`, `${x/p/r}`, `${x//p/r}`,
    `${x:off:len}`
  - zsh flags and modifiers: `${(U)x}`, `${(L)x}`, `${(s:,:)x}`, `${(j:,:)a}`,
    `${(f)x}`, `${(F)a}`, `${(@)a}`, `${=x}`, `${x:u}`, `${x:l}`
  - nested substitutions such as `${${x#a}%b}`
- Arrays: `a=(x y)`, `a+=(z)`, `$a[1]`, `$a[-1]`, `$a[2,3]`, `${#a}`,
  `"${a[@]}"`.
- Brace expansion (`{a,b}`, `{1..10..2}`, `{01..10}`), tilde expansion, and
  globbing with `*`, `?`, `[...]` and recursive `**/`.

zsh semantics that differ from sh:

- An unquoted `$var` is not word-split (use `${=var}` to split).
- An array expands to one word per element.
- An unquoted empty parameter disappears.
- A glob that matches nothing is an error (`setopt nonomatch` keeps the
  pattern instead).
- The last stage of a pipeline runs in the current shell, so
  `echo x | read v` sets `v`.

Options: `set -e -u -x -f`, `set -o pipefail`, `setopt`/`unsetopt` (`errexit`,
`xtrace`, `nounset`, `noglob`, `pipefail`, `nomatch`, `globdots`).

## Builtins

**Shell builtins:**
`: . alias builtin cd command continue break declare dirs env eval exec exit
export functions getopts hash history integer jobs let local popd pushd read
readonly rehash return set setopt shift source time trap type typeset umask
unalias unfunction unset unsetopt wait whence where which xargs`

**Utilities:**
`[ basename cat chmod clear cp cut date dirname du echo env false find grep
head hostname ln ls mkdir mktemp mv nl nproc paste printenv printf pwd
readlink realpath rev rm rmdir seq sleep sort stat tac tail tee test touch tr
true uname uniq wc whoami yes`

Each utility supports the commonly used GNU flags (for example `ls -lah`,
`grep -rniE`, `sort -k2 -t, -n`, `find . -name '*.rs' -type f`,
`tail -f`). `chmod` exists on unix only.

`env` and `xargs` are shell builtins because they run other commands. They
use the same lookup without functions, so `find . -name '*.txt' | xargs wc -l`
uses the builtin `wc`.

## Interactive use

- Line editing and history (`~/.vsh_history`, or `$HISTFILE`), with history
  hints.
- Tab completion for commands, files and `$variables`.
- Commands that span several lines continue on the next line.
- Startup file: `~/.vshrc`.
- The prompt comes from `$PROMPT` (or `$PS1`) and uses zsh escapes: `%n %m %~
  %1~ %d %# %? %j %T %* %B %b %F{color} %f %(?.ok.fail)`. The default is
  `%n@%m %~ %# ` in color, with the exit status when it isn't 0.
- Ctrl-C interrupts the running command, whether it is an external program
  or a builtin (`cat`, `yes`, `sleep`, a `while` loop).
- `cmd &` runs in the background. `jobs` and `wait` handle background jobs.

## Windows notes

- Paths are shown with forward slashes (`C:/Users/me`). `~` is `%USERPROFILE%`
  when `HOME` is not set.
- Backslash is the escape character, as in zsh, so type `C:/dir` or
  `'C:\dir'`.
- Programs are found through `PATH` and `PATHEXT`; `.bat` and `.cmd` files
  work.
- A script with a `#!...sh` line runs through vsh.
- `/dev/null` maps to `NUL`.
- Ctrl-C is handled through the console control handler. ANSI colors and the
  UTF-8 code page are turned on at startup.

## Limitations

- No job control: there is no `fg`, `bg` or Ctrl-Z. Background jobs are
  threads.
- Only file descriptors 0–2 can be redirected. `exec` with only redirections
  (`exec >log`) is not supported.
- `trap` only handles `EXIT`.
- No associative arrays, no glob qualifiers or extended globs, and no `select`
  or coprocesses.
- `grep` uses the Rust regex engine, so there are no back-references.
- `find` has no `-exec`; pipe into `xargs` instead.
- Windows support is checked at compile time (`cargo check`/`clippy --target
  x86_64-pc-windows-msvc`). The test suite has only run on macOS.

## Layout

| File | Contents |
| --- | --- |
| `src/parser.rs`, `src/ast.rs` | Combined lexer and recursive-descent parser. Yields one complete command at a time, so aliases apply to later lines. |
| `src/expand.rs`, `src/glob.rs`, `src/arith.rs` | Word expansion, pattern matching and globbing, `$(( ))`. |
| `src/exec.rs` | Executor: lookup, pipelines (one thread per stage on a copy of the shell state), redirections, command substitution, jobs. |
| `src/io.rs` | Reference-counted stdin/stdout/stderr descriptions shared by builtins and child processes. |
| `src/shell.rs` | Shell state. The working directory is part of the state, not the process, so concurrent pipeline stages never race on it. |
| `src/builtins.rs`, `src/cond.rs` | Shell builtins and `[[ ]]`. |
| `src/utils/` | The coreutils: plain functions over a `Ctx` (streams, cwd, env). |
| `src/repl.rs`, `src/term.rs` | Line editor, prompt, signals and console setup. |

Tests: `cargo test` runs the unit tests and `tests/shell.rs` (end-to-end
scripts).
