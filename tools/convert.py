#!/usr/bin/env python3
"""Check `zeno convert` on the programs of Python in tools/convert.

Each program is converted, and then both are run: `python3 x.py` and
`zeno run x.zeno`. What the two print must be the same. A program whose
notes say that Zeno shows something otherwise has a file `x.expect`
beside it with what Zeno prints, and that is what is compared.

The first line of each program says how many of its pieces are kept as
Python (`# kept: 2`), so that a rule that stops working is seen.

    python3 tools/convert.py                 every program
    python3 tools/convert.py lists texts     these ones
    python3 tools/convert.py --write-expect  write the `.expect` files anew
"""

import os
import re
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
PROGRAMS = os.path.join(HERE, "convert")
ZENO = os.environ.get("ZENO", os.path.join(HERE, "..", "target", "release", "zeno"))


def run(command, folder):
    done = subprocess.run(command, cwd=folder, capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=120)
    return done.returncode, done.stdout, done.stderr


def check(name, write_expect):
    source = os.path.join(PROGRAMS, name + ".py")
    with open(source, encoding="utf-8") as file:
        text = file.read()
    with tempfile.TemporaryDirectory() as folder:
        with open(os.path.join(folder, name + ".py"), "w", encoding="utf-8") as file:
            file.write(text)
        code, report, errors = run([ZENO, "convert", name + ".py"], folder)
        if code != 0:
            return f"`zeno convert` failed:\n{report}{errors}"
        wanted = re.match(r"# kept: (\d+|all)", text)
        found = re.search(r"(\d+) kept as Python", report)
        kept = "all" if "The whole file is kept" in report else (found.group(1) if found else "?")
        _, python_out, python_err = run([sys.executable, name + ".py"], folder)
        code, zeno_out, zeno_err = run([ZENO, "run", name + ".zeno"], folder)
        with open(os.path.join(folder, name + ".zeno"), encoding="utf-8") as file:
            zeno_text = file.read()
        expect = os.path.join(PROGRAMS, name + ".expect")
        if write_expect:
            if zeno_out != python_out:
                with open(expect, "w", encoding="utf-8") as file:
                    file.write(zeno_out)
            elif os.path.exists(expect):
                os.remove(expect)
        if os.path.exists(expect):
            with open(expect, encoding="utf-8") as file:
                python_out = file.read()
            if "Where Zeno does otherwise" not in report:
                return "it has a `.expect` file, and the report has no note that says why"
        problems = []
        if wanted and wanted.group(1) != kept:
            problems.append(f"{kept} pieces kept, and its first line says {wanted.group(1)}\n{report}")
        if not wanted:
            problems.append("its first line does not say `# kept: N`")
        if code != 0 or zeno_out != python_out:
            problems.append(
                "the two do not print the same\n--- python3\n"
                + python_out + python_err[-600:] + "--- zeno run\n" + zeno_out + zeno_err[-1200:] + "--- the Zeno\n" + zeno_text
            )
        return "\n".join(problems)


def main():
    args = [arg for arg in sys.argv[1:] if not arg.startswith("--")]
    write_expect = "--write-expect" in sys.argv
    names = args or sorted(name[:-3] for name in os.listdir(PROGRAMS) if name.endswith(".py"))
    failed = 0
    for name in names:
        problem = check(name, write_expect)
        if problem:
            failed += 1
            print(f"FAILED {name}: {problem}")
        else:
            print(f"ok     {name}")
    print(f"{len(names) - failed} of {len(names)} programs convert and print the same")
    sys.exit(1 if failed else 0)


main()
