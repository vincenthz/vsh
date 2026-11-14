//! End-to-end tests: run the vsh binary on scripts and compare output.

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn vsh() -> Command {
    Command::new(env!("CARGO_BIN_EXE_vsh"))
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("vsh-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Run a script in a fresh temp dir; returns (status, stdout, stderr).
fn run(script: &str) -> (i32, String, String) {
    let tag: String = format!("{:x}", script.len() * 31 + script.bytes().map(|b| b as usize).sum::<usize>());
    let dir = tmpdir(&tag);
    let out = vsh()
        .arg("-c")
        .arg(script)
        .current_dir(&dir)
        .env("HOME", &dir)
        .env_remove("ENV")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn out(script: &str) -> String {
    let (_, o, e) = run(script);
    assert!(e.is_empty() || !e.contains("not implemented"), "stderr for {script:?}: {e}");
    o
}

#[track_caller]
fn check(script: &str, expected: &str) {
    let (_, o, e) = run(script);
    assert_eq!(o, expected, "script: {script}\nstderr: {e}");
}

#[test]
fn quoting_and_params() {
    check(r#"x='a  b'; echo $x "$x" '$x' \$x"#, "a  b a  b $x $x\n");
    check(r#"echo "${undefined:-def}" ${x:=set} $x"#, "def set set\n");
    check(r#"p=/a/b/c.tar.gz; echo ${p##*/} ${p%%.*} ${p#/} ${p%/*}"#, "c.tar.gz /a/b/c a/b/c.tar.gz /a/b\n");
    check(r#"s=hello; echo ${#s} ${s:1:3} ${s/l/L} ${s//l/L} ${(U)s} ${s:u}"#, "5 ell heLlo heLLo HELLO HELLO\n");
    check(r#"echo $'a\tb' "q\"uote" 'it'\''s'"#, "a\tb q\"uote it's\n");
    check(r#"e=; echo a $e b "$e" c | cat -A"#, "a b  c$\n");
    check(r#"set -- one "two three"; echo $# $1; for a in "$@"; do echo "[$a]"; done"#, "2 one\n[one]\n[two three]\n");
}

#[test]
fn zsh_param_flags() {
    check("x=a:b:c; for i in ${(s.:.)x}; do echo -n [$i]; done; echo", "[a][b][c]\n");
    check("a=(1 2 3); echo ${(j:,:)a} ${(U)${:-up}}", "1,2,3 UP\n");
    check("v='p q'; for w in ${=v}; do echo $w; done", "p\nq\n");
    check("o=$(printf 'l1\\nl2'); for l in \"${(f)o}\"; do echo \"<$l>\"; done", "<l1>\n<l2>\n");
}

#[test]
fn arrays() {
    check(r#"a=(x y z); echo ${#a} $a[1] $a[-1] ${a[2,3]}; a+=(w); echo $a"#, "3 x z y z\nx y z w\n");
    check(r#"a=(1 2 3); for i in $a; do echo -n $i; done; echo; echo "${a[@]}" | wc -w"#, "123\n3\n");
    check(r#"f() { local -a l=(p q); echo ${#l}; }; f"#, "2\n");
}

#[test]
fn arithmetic() {
    check("echo $((1 + 2 * 3)) $((10 / 3)) $((2 ** 8)) $(( (1<2) && 3 ))", "7 3 256 1\n");
    check("x=5; (( x += 2 )); echo $x; (( x > 100 )) || echo small", "7\nsmall\n");
    check("for ((i=0; i<3; i++)); do echo -n $i; done; echo", "012\n");
    check("i=0; while (( i < 3 )); do i=$((i+1)); done; echo $i", "3\n");
}

#[test]
fn control_flow() {
    check("if false; then echo a; elif true; then echo b; else echo c; fi", "b\n");
    check("for i in 1 2 3 4; do [ $i = 2 ] && continue; [ $i = 4 ] && break; echo $i; done", "1\n3\n");
    check("case hello.rs in *.c) echo c;; *.rs) echo rust;; esac", "rust\n");
    check("case x in x) echo one;& y) echo two;; z) echo three;; esac", "one\ntwo\n");
    check("repeat 3 echo hi", "hi\nhi\nhi\n");
    check("until [ -f f ]; do touch f; echo made; done", "made\n");
    check("for i (a b) echo $i", "a\nb\n");
    check("true && echo yes || echo no; false && echo yes || echo no; ! false && echo neg", "yes\nno\nneg\n");
}

#[test]
fn functions_and_scope() {
    check("f() { echo \"$0:$1:$#\"; return 4; }; f a b; echo $?", "f:a:2\n4\n");
    check("x=outer; f() { local x=inner; echo $x; }; f; echo $x", "inner\nouter\n");
    check("function g { echo g$1; }; g 1", "g1\n");
    check("fact() { if (( $1 <= 1 )); then echo 1; else echo $(( $1 * $(fact $(( $1 - 1 ))) )); fi; }; fact 5", "120\n");
}

#[test]
fn conditions() {
    check("[[ abc == a* ]] && echo glob; [[ abc == 'a*' ]] || echo literal", "glob\nliteral\n");
    check("[[ foo123 =~ ^[a-z]+([0-9]+)$ ]] && echo $MATCH $match[1]", "foo123 123\n");
    check("[[ -n x && ! -z x ]] && echo ok; [[ 3 -lt 10 ]] && echo num; [[ b > a ]] && echo str", "ok\nnum\nstr\n");
    check("touch f; [[ -f f && ! -d f ]] && echo file; [ -e nope ] || echo none", "file\nnone\n");
}

#[test]
fn pipelines_and_redirections() {
    check("echo hello | tr a-z A-Z | rev", "OLLEH\n");
    check("printf 'b\\na\\nc\\n' | sort | head -n 2", "a\nb\n");
    check("echo one > f; echo two >> f; cat f; wc -l < f", "one\ntwo\n2\n");
    check("{ echo out; echo err >&2; } 2>&1 | sort", "err\nout\n");
    check("{ echo out; echo err >&2; } > o 2> e; cat e o", "err\nout\n");
    check("{ echo a; echo b >&2; } &> both; sort both", "a\nb\n");
    check("echo x |& cat", "x\n");
    check("echo piped | read v; echo $v", "piped\n");
    check("cat <<EOF\nx=$((1+1))\n  tab\nEOF", "x=2\n  tab\n");
    check("cat <<'EOF'\n$HOME\nEOF", "$HOME\n");
    check("cat <<-EOF\n\tindented\n\tEOF", "indented\n");
    check("tr a-z A-Z <<< 'here string'", "HERE STRING\n");
    check("yes | head -n 3", "y\ny\ny\n");
    check("seq 1 100000 | tail -n 1", "100000\n");
    check("echo hidden > /dev/null; echo shown", "shown\n");
}

#[test]
fn command_substitution_and_splitting() {
    check("echo $(echo a; echo b)", "a b\n");
    check("echo \"$(printf 'a\\nb')\"", "a\nb\n");
    check("for w in $(echo 'x y  z'); do echo [$w]; done", "[x]\n[y]\n[z]\n");
    check("x=`echo back`; echo $x", "back\n");
    check("echo $(echo $(echo nested))", "nested\n");
}

#[test]
fn globbing_and_braces() {
    check("touch b.txt a.txt c.log .hid.txt; echo *.txt; echo *.{txt,log}", "a.txt b.txt\na.txt b.txt c.log\n");
    check("mkdir -p d/e; touch d/e/f.rs d/g.rs; echo **/*.rs", "d/e/f.rs d/g.rs\n");
    check("echo {1..5} {a..c} x{y,z}", "1 2 3 4 5 a b c xy xz\n");
    check("echo '*' \"*\" \\*", "* * *\n");
    let (st, _, e) = run("echo *.nothing");
    assert_eq!(st, 1);
    assert!(e.contains("no matches found"), "{e}");
    check("setopt nonomatch; echo *.nothing", "*.nothing\n");
    check("x='*'; touch q; echo $x", "*\n");
}

#[test]
fn cd_and_pwd() {
    let o = out("mkdir sub; cd sub; basename $PWD; cd ..; cd - > /dev/null; basename $(pwd); echo $OLDPWD | grep -c vsh-it");
    assert_eq!(o, "sub\nsub\n1\n");
    // Subshells and pipeline stages don't change the parent's directory.
    let o = out("mkdir s; (cd s); basename $PWD | grep -c s$; cd s | true; basename $PWD | grep -c '^s$'");
    assert_eq!(o, "0\n0\n");
}

#[test]
fn aliases_and_lookup() {
    check("alias hi='echo hello'\nhi there", "hello there\n");
    check("alias ls='ls -1'\nmkdir x y; ls", "x\ny\n");
    check("type cd; type echo; f() { :; }; type f", "cd is a shell builtin\necho is a vsh builtin utility\nf is a shell function\n");
    check("echo() { builtin echo wrapped $@; }; echo x", "wrapped x\n");
    let (st, _, e) = run("definitely-not-a-command-xyz");
    assert_eq!(st, 127);
    assert!(e.contains("command not found"), "{e}");
}

#[test]
fn exports_and_env() {
    check("export FOO=bar; env | grep '^FOO='", "FOO=bar\n");
    check("FOO=tmp env | grep '^FOO='; echo \"[$FOO]\"", "FOO=tmp\n[]\n");
    check("env -i A=1 env", "A=1\n");
    check("X=1; env | grep -c '^X='", "0\n");
    check("f() { echo $V; }; V=scoped f; echo \"[$V]\"", "scoped\n[]\n");
}

#[test]
fn read_builtin() {
    check("printf 'a b c\\n' | { read x y; echo \"$x|$y\"; }", "a|b c\n");
    check("printf 'l1\\nl2\\n' > f; while read line; do echo \"<$line>\"; done < f", "<l1>\n<l2>\n");
    check("echo 'a:b:c' | { IFS=: read -A arr; echo ${#arr} $arr[2]; }", "3 b\n");
    // Here-strings/docs on a loop share one read position.
    check("while read l; do echo \"<$l>\"; done <<< $'x\\ny'", "<x>\n<y>\n");
    check("{ read a; read b; echo $b$a; } <<EOF\n1\n2\nEOF", "21\n");
}

#[test]
fn getopts_builtin() {
    check(
        "f() { while getopts 'ab:' o; do case $o in a) echo A;; b) echo B=$OPTARG;; esac; done; shift $((OPTIND-1)); echo rest=$*; }; f -a -b val x y",
        "A\nB=val\nrest=x y\n",
    );
}

#[test]
fn xargs_and_find() {
    check("mkdir d; touch d/1 d/2; find d -type f | sort | xargs echo files:", "files: d/1 d/2\n");
    check("printf 'a\\nb\\n' | xargs -I{} echo item-{}", "item-a\nitem-b\n");
    check("seq 1 5 | xargs -n 2 echo", "1 2\n3 4\n5\n");
}

#[test]
fn errexit_and_status() {
    check("set -e; echo before; false; echo after", "before\n");
    check("set -e; if false; then :; fi; false || true; echo survived", "survived\n");
    let (st, _, _) = run("exit 42");
    assert_eq!(st, 42);
    check("false; echo $?; true; echo $?", "1\n0\n");
    check("trap 'echo bye' EXIT; echo hi", "hi\nbye\n");
    check("set -o pipefail; false | true; echo $?", "1\n");
}

#[test]
fn background_jobs() {
    check("{ sleep 0.1; echo bg; } & echo fg; wait; echo done", "fg\nbg\ndone\n");
}

#[test]
fn eval_and_source() {
    check("cmd='echo evaluated'; eval $cmd", "evaluated\n");
    check("echo 'sourced=yes; echo in-file $1' > s.sh; source s.sh arg; echo $sourced", "in-file arg\nyes\n");
    check("echo 'echo script $1 $#' > t.sh; vsh_bin=$SHELL; $vsh_bin t.sh a b", "script a 2\n");
}

#[test]
fn external_fallback() {
    // A program that only exists in PATH still runs.
    if cfg!(unix) {
        check("mkdir bin; printf '#!/bin/sh\\necho ext-$1\\n' > bin/myprog; chmod +x bin/myprog; PATH=$PWD/bin:$PATH myprog ok", "ext-ok\n");
        // Scripts without a shebang run in vsh.
        check("printf 'echo noshebang $1\\n' > ns; chmod +x ns; ./ns z", "noshebang z\n");
        // `command` prefers PATH over the builtin utility.
        check("mkdir bin; printf '#!/bin/sh\\necho external-ls\\n' > bin/ls; chmod +x bin/ls; PATH=$PWD/bin; command ls; ls -d bin", "external-ls\nbin\n");
    }
}

#[test]
fn stdin_script() {
    use std::io::Write;
    let mut child = vsh().stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"x=1\nif [ $x = 1 ]; then\n  echo from-stdin\nfi\n").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), "from-stdin\n");
}

#[test]
fn syntax_errors() {
    let (st, _, e) = run("echo )");
    assert_eq!(st, 1);
    assert!(e.contains("parse error"), "{e}");
    let (_, _, e) = run("if true; then");
    assert!(!e.is_empty());
}
