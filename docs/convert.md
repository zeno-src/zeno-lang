# A file of Python written again as Zeno: `zeno convert`

A program already written in Python can be had as Zeno with one
command:

```sh
zeno convert shop.py
```

It writes `shop.zeno` beside it, and says what it did:

```
Wrote shop.zeno from shop.py.
  11 pieces: 10 written as Zeno, 1 kept as Python.

  Where Zeno does otherwise than Python (2 lines; each kind has a `# convert:` comment at its first):
    a value is shown as Zeno shows it: line 29
    `upper` and `lower` change a to z only: line 26

  Kept as Python, in the `inline python` block:
    line 2, `import textwrap`: `textwrap` is a module of Python, and what uses it reaches it through the block

  Run it with `zeno run shop.zeno`. It prints what `python3 shop.py` prints,
  but for the notes above.
```

The one promise is that **what is written is never silently wrong**.
What the converter can say in Zeno it says in Zeno. What it cannot, it
leaves as Python, in an [`inline python`](inline.md) block that the
lines of Zeno call. So what it writes always runs, prints what the
Python printed, and the work left is to make the block smaller.

The decisions behind it are chapter 62 of [`decisions.md`](decisions.md).

## An example

`shop.py`:

```python
# A till: prices, a basket, and a receipt.
import textwrap

PRICES = {"tea": 2.5, "cake": 3.0, "jam": 1.25}


def line_total(item, count):
    return PRICES[item] * count


def receipt(basket):
    lines = []
    total = 0.0
    for item, count in basket.items():
        cost = line_total(item, count)
        total += cost
        lines.append(f"{item:<6} x{count}  {cost:6.2f}")
    lines.append(f"{'total':<9} {total:6.2f}")
    return lines


basket = {"tea": 2, "jam": 3}
basket["cake"] = 1
for line in receipt(basket):
    print(line)
words = sorted(word.upper() for word in basket)
print(", ".join(words))
note = textwrap.fill("Thank you for shopping with us, and come back soon.", width=24)
print(note)
```

`shop.zeno`, as the converter writes it:

```zeno
# Written from shop.py by `zeno convert`.

import python

let py = inline python {
    import textwrap
}

# convert: Python's `sorted`, a copy of a list with the smallest first.
function sorted[T is Ordered](items: list[T]) -> list[T] {
    let copy = items.slice(0)
    copy.sort()
    return copy
}

# A till: prices, a basket, and a receipt.

let PRICES = ["tea": 2.5, "cake": 3.0, "jam": 1.25]

function line_total(item: str, count: int) -> float {
    return PRICES[item] * count
}

function receipt(basket: dict[str, int]) -> list[str] {
    let lines: list[str] = []
    var total = 0.0
    for item, count in basket {
        let cost = line_total(item, count)
        total += cost
        lines.append(f"{item:<6} x{count}  {cost:6.2f}")
    }
    lines.append(f"{'total':<9} {total:6.2f}")
    return lines
}

let basket = ["tea": 2, "jam": 3]
basket["cake"] = 1
for line in receipt(basket) {
    print(line)
}
# convert: `upper` and `lower` change a to z only. Python changes every letter
# that has a capital.
let words = sorted([word.upper() for word in basket])
print(words.join(", "))
let note = py.textwrap.fill("Thank you for shopping with us, and come back soon.", width=24)
# convert: Zeno shows a value its own way: `true`, `false` and `none` in small
# letters, a text inside a list, a dict or an object in double quotes, and a
# dict between `[` and `]`.
print(note)
```

The types came from the program itself: `line_total` is given a text
and a whole number at its one call, and gives back a float. `textwrap`
is a library of Python that Zeno has no words for, so its `import`
stays in the block and `fill` is called through it, as `py.textwrap`.

## Pieces, and what becomes of each

The file is cut into **pieces**: the statements that begin at its left
margin. A `def` with its body is one piece, a `class` is one, a `for`
loop at the top level is one, and so is each line such as
`basket["cake"] = 1`. Each piece ends in one of three states:

| State | What is written | In the report |
|---|---|---|
| **Zeno** | Lines of Zeno that mean what the piece meant | Counted as written as Zeno |
| **Zeno with a note** | The same, with a `# convert:` comment where Zeno does something a reader of Python would not expect | Under "Where Zeno does otherwise", with every line it applies to |
| **Kept** | The piece as it was written, in the `inline python` block | Under "Kept as Python", with the reason and its line |

A comment is written once for each kind of note, above the first line
it applies to. The report lists every line.

A kept `def`, `class` or `import` sits at the top of the block, and
the Zeno reaches it through the block: `py.fill(...)`, `py.Meter(...)`.
A kept line that does something, such as a loop, becomes a function of
the block, `def convert_step_1():`, that the Zeno calls in its place,
so it runs exactly where it ran in the Python:

```zeno
let py = inline python {
    def convert_step_1():
        global total
        for row in rows:
            total += parse(row)
}

py.convert_step_1()
print(py.total)
```

The block is named `py`. If the file has a name `py` already, it is
named `kept`, or `python_part`.

## What is written as Zeno

| Python | Zeno |
|---|---|
| `x = 1`, given again later | `var x = 1`; given once, `let x = 1` |
| A name first given a value inside a block and used after it | Declared before the block, with the first value of its type: `var name: str = ""` |
| `def f(a, b=2):` | `function f(a: int, b: int = 2) -> int {`: the types from what the program gives it and what it gives back |
| A parameter given a new value in the function | `function f(n_given: int)` and `var n = n_given`, since a parameter of Zeno cannot change |
| `class` whose `__init__` only puts its parameters and fixed values into fields, and its methods | `class` with fields and methods |
| `@dataclass` | `class`; a field with a default is still given a value by `Point(1, 2)` |
| `True`, `False`, `None` | `true`, `false`, `none` |
| `if x:` | The question asked out: `x != 0`, `x != ""`, `len(x) > 0`, `x is not none` |
| `x if c else y` | `if c { ... } else { ... }`, or `y` and then `x` if `c` where that changes nothing |
| `a and b` as a value, between bools | `a and b` |
| f-strings, with `:<6`, `:6.2f`, `:x`, `:08b` | f-strings of Zeno, which take the same |
| `xs[-1]` | `xs[len(xs) - 1]` |
| `xs[1:10]`, `s[-3:]` | `xs.slice(min(1, len(xs)), min(10, len(xs)))`, `s.slice(max(len(s) - 3, 0))`: an end past the end is cut as Python cuts it |
| `for k, v in d.items():`, `for i, x in enumerate(xs):` | `for k, v in d {`, `for i, x in xs {` |
| `for name, age in [("ann", 30), ("bob", 41)]:` | `for row_1, name in ["ann", "bob"] {` and `let age = [30, 41][row_1]` |
| A loop's variable read after the loop | The loop counts with `word_item`, and its first line gives `word` its value |
| `[x * 2 for x in xs if x > 0]` | The same in Zeno |
| `sum(x for x in xs)`, and in `min`, `max`, `sorted`, `list`, `join` | A list made from a loop: these take every item before they give anything |
| `try:` / `except ValueError as e:` / `raise KeyError(k)` | `try { } except ValueError as e { }`, `raise KeyError(k)` |
| `if __name__ == "__main__":` | Its block, at the top level |
| `round(x)` | `math.round(x)`, with a note |
| `chr`, `ord(c)`, `s.startswith`, `s.splitlines()`, `sep.join(xs)` | `char`, `c.code()`, `s.starts_with`, `s.lines()`, `xs.join(sep)` |
| `xs.copy()`, `list(xs)`, `list(d.keys())` | `xs.slice(0)`, `d.keys()` |
| `del d[k]` | `d.pop(k)` |
| `import math`, `from math import sqrt, pi` | `import math`, `math.sqrt`, `math.pi` |
| `sys.argv[1:]`, `sys.exit(n)` | `program.arguments()`, `program.exit(n)` |
| Comments | Where they were |

### What Python has and Zeno has not

A few things Python has are not in Zeno. Where a file uses one, it gets
a small function of Zeno that does it, once, at the top, with a
comment that says what it is:

| Python | The function written for it |
|---|---|
| `sorted(xs)`, `sorted(d)` | `sorted[T is Ordered](items: list[T]) -> list[T]` |
| `xs.remove(x)` | `remove_item[T](items: list[T], item: T) raises ValueError` |
| `text.split(by, most)` | `split_at_most(text: str, by: str, most: int) -> list[str]` |
| `"-" * 20` | `repeated(text: str, count: int) -> str` |

A file that has a name of its own the same as one of these keeps the
line that would need it. (`sorted` is the exception: a file that gives
the name `sorted` a meaning of its own uses that one.)

## What is kept, and why

A piece is kept for one reason, which the report gives with the line
it is on. The usual ones:

- **What Zeno does not have yet**: `lambda`, a function inside a
  function, a generator (`yield`), a decorator other than `@dataclass`,
  `*args`, a class with a base, a method with two underscores at each
  end, a tuple kept as a value, `%` formatting, `print(..., end="")`,
  `with` and `open`, `input`.
- **What Zeno would do otherwise**: `==` between objects that are not
  dataclasses, printing an object that is not a dataclass, a default
  that is a list or a dict (Python makes it once, Zeno each call),
  `sort(key=...)`, `find`, a bare `except:`, a loop whose block, or a
  function it calls, changes the list it goes over.
- **A name that is a word of Zeno**: a variable called `match`, `len`
  given another meaning, and the like.
- **A type the converter cannot tell**: a value that only Python knows
  (one that came from the block) used in arithmetic, a dict whose keys
  are not texts or whole numbers.
- **What the rest needs**: a piece that gives a value a kept piece
  reads, and the rules below.

### What crosses between Zeno and the block

The lines of Zeno and the block are two sides, and a value crosses
from one to the other as the bridge to Python ([`python.md`](python.md))
takes it. A number, a text, a bool and `none` cross exactly. A value
of Python's own, such as an object a kept class made, stays in Python,
and Zeno holds it as a handle. A list or a dict of Zeno given to
Python is copied, and what Python did to its copy would not be seen
by the Zeno. So:

- A call into the block may be given numbers, texts, bools, `none`
  and values of Python's own; and a list or a dict written right there
  in the call, which nothing else holds. Otherwise the piece is kept.
- A piece that gives a value to a name that a kept piece reads is kept
  too, if the kept piece can run after it: it is further down the file,
  or it is a function or a class. Python can only call Python.
- A name that a kept piece and a piece of Zeno both give values is a
  variable on each side. A line of Zeno after the kept piece must read
  a value Zeno gave the name there, and if it may not, it is kept, with
  the reason "`n` may still hold here the value that the piece at line
  3 gave it".
- The variable of a kept loop that no other piece reads after the loop
  is that loop's own, and a loop of Zeno with a variable of the same
  name has nothing to do with it.
- An `import` of a module that Zeno has words for, and a name given one
  fixed value once in the whole file (`LIMIT = 10`), are written on both
  sides.

### How a piece is chosen to be kept

The types come from rounds over the whole file: what a function is
given at its calls, what it gives back, what goes into a list, a dict,
a field. A round may learn something the round before did not know,
so there are rounds until nothing more is learned (twelve at most).

Then the last round writes every piece. If some cannot be written,
**one** is kept: the first whose reason is its own, and not "the type
is not known". All is then done again. One piece at a time, because
what one piece cannot be written for is often what another became: a
name holds a function because the line that gives it one is kept, a
type is not known because a value came from the block.

Last, what was written is read by Zeno's own checker, in the same
program. A piece that it does not take is kept, with the checker's
words as the reason. Nothing that Zeno would refuse is given out.

## The notes

Each note is a place where the Zeno runs and does what the Python did,
but prints or raises something a little otherwise.

| Note | What it says |
|---|---|
| A value is shown as Zeno shows it | `print` and f-strings show `true`, `false`, `none` in small letters, a text inside a list in double quotes, a dict between `[` and `]`. Written where such a value is printed, and where a value of Python's own is printed, which may be one |
| A float is shown, and a very large or very small one is written otherwise | Python's `1e+16` is `1e16` in Zeno, and `1e-07` is `1e-7`. Every other float is shown as Python shows it |
| The message of an error is Zeno's own | Where Zeno raised the error itself, its message is in Zeno's words. An error out of the kept Python has its kind in front of its message |
| An `except` also catches errors only Zeno raises | Zeno raises an error where Python raises none: a whole number past 64 bits (`OverflowError`), an item at a place below zero that a variable holds (`IndexError`), a whole number to a power below zero (`ValueError`). An `except` of that kind catches these too |
| `upper` and `lower` change a to z only | Python changes every letter that has a capital |
| `math.round` takes a half away from zero | 2.5 to 3, where Python's `round` takes it to the even number, 2 |
| A variable is a float from its first value | It holds a float later, and a variable of Zeno has one type. In Python it was a whole number until then, and is printed as one |

The test programs in [`tools/convert`](../tools/convert) that differ
in this way have a file `x.expect` with what Zeno prints, and the check
holds the report to having a note that says why.

## What it does not promise

- **A program that stops with an error that Python finds while it
  runs**: a name used before it has a value (`NameError`,
  `UnboundLocalError`). A name the converter declares before a block
  has the first value of its type, where Python would stop.
- **Whole numbers past 64 bits.** Python's have no end. Zeno stops
  with `OverflowError` where a sum goes past, so this is never silent
  but for one thing: `int()` of a float past 64 bits gives the largest
  whole number Zeno has.
- **The types a kept function says it gives.** `def f() -> int:` in
  the block is believed: what crosses is checked by the bridge.
- **The same list reached by two names** while a loop goes over it
  under one of them.
- **`strip`, `split` and `splitlines`** on spaces outside ASCII, such as
  a non-breaking space, which Python counts as space and Zeno does not.
- **What an `import` of the block does when it is run**: it runs when
  the block is made, at the top of the program, and not where the
  `import` was.
- **The last digit of `sin`, `cos` and the like**, which comes from
  the C library of the machine.

## Checking it

```sh
python3 tools/convert.py           # every program in tools/convert
python3 tools/convert.py lists     # one
```

Each program of Python in [`tools/convert`](../tools/convert) is
converted, run with `python3` and with `zeno run`, and what the two
print is compared. The first line of each says how many pieces are to
be kept (`# kept: 2`), so a rule that stops working is seen. One of
them, `library.py`, is a program of 240 lines with classes, a
dataclass, dicts of objects, exceptions, f-strings and a report, and is
written as Zeno but for its `import json`. The GitHub workflow
`convert.yml` runs this on every push that changes the converter, with
`cargo test convert`.

## Where it is

| File | What it has |
|---|---|
| `src/convert.rs` | The command, the notes and the report |
| `src/convert_python.rs` | Reading Python: the tokens, the pieces, the tree of each piece |
| `src/convert_python_write.rs` | The pieces, what they define and use, which are kept, the rounds, the plan of where each name is declared, the block |
| `src/convert_python_values.rs` | Writing an expression of Python as Zeno, with its type |
| `src/convert_python_lines.rs` | Writing a statement, a function and a class |
| `tools/convert.py`, `tools/convert/` | The programs and the check |

C is the next language it is to read: a file of C written as Zeno,
with what it cannot read left in an `inline c` block.
