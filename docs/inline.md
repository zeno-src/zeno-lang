# Another language's code in a Zeno file: `inline` blocks

Some code is already written, and is in another language: a checksum
out of a board's firmware, a few lines of Python that use a library, a
function of JavaScript from a page. An `inline` block holds it in a
`.zeno` file as it was written, and the program calls it.

```zeno
import c

let bits = inline c {
    #include <stdint.h>

    /* How many bits of x are 1. */
    int ones(uint32_t x) {
        int count = 0;
        for (; x != 0; x &= x - 1) {
            count++;
        }
        return count;
    }
}

print(bits.ones(0xA5))
```

```
4
```

What is between the first line and the last `}` is C, and is not read
as Zeno. `bits` is a library of C, as [`c.library`](c.md) gives one,
and `ones` is a function of it.

There are three kinds of block:

| Written | Holds | Gives |
|---|---|---|
| `inline c { }` | C | A library of C: the functions the block writes out |
| `inline python { }` | Python | A module of Python: what the block defines |
| `inline node { }` | JavaScript | What the block puts in `exports` |

Nothing changes for a program that has no block. A block is asked for
twice: by the word `inline`, and by the `import` of its language at
the top of the file. The decisions behind it are chapter 61 of
[`decisions.md`](decisions.md).

```sh
cargo run -- run examples/bits.zeno
cargo run -- run examples/report.zeno
```

[`examples/bits.zeno`](../examples/bits.zeno) reverses bits, counts
them and hashes text in C. [`examples/report.zeno`](../examples/report.zeno)
has Python and JavaScript in one file.

## Why not make Zeno a superset of C, or of Python

A superset of a language has to take every program of that language,
for ever, mistakes and all, and a superset of two languages cannot
exist where they disagree: `a / b`, `{}` and `'x'` each mean something
else in C, Python and JavaScript. A block gets the same result with
none of the cost. The other language's code is used as it is, by that
language's own compiler, and Zeno stays one language with one set of
rules.

## What it needs

What the `import` of the language needs, and nothing more:

- `inline c`: a C compiler, as for [`import c`](c.md) and `zeno build`.
- `inline python`: Python 3, as for [`import python`](python.md).
- `inline node`: Node, as for [`import node`](node.md).

## Writing a block

```zeno
let name = inline c {
    ...the other language...
}
```

- **A block is the whole value of a `let`.** What it has is then used
  through that name. It may be at the top of a file, in a function, or
  in a test.
- **The first line ends with the `{`.** `inline`, the language, `{`,
  and the end of the line.
- **Everything inside is further in than the line that opens the
  block.** How far is up to you; `zeno format` sets it one step in.
- **The block ends at a line that holds only `}`**, as far in as the
  line that opened it.

That last rule is why a block needs no care. A `}` of C is always
further in than the block's own, so it ends nothing. Zeno does not
look for the other language's strings and comments, and so cannot
mistake one:

```zeno
let odd = inline c {
    /* A } in a comment, */
    const char *closing(void) { return "} and one in a text"; }
}
```

`inline` is not a reserved word. `let inline = 3` still means what it
meant: the word begins a block only when a name and a `{` that ends
the line come after it, which nothing else in Zeno can be.

The indentation that every line of a block shares is not part of the
block. Each line keeps how far in it is from the others, which in
Python is part of the program.

## A block of C

**Zeno sees the functions.** Each function that is written out in the
block, with its body, and is not `static`, is a function of the
library. Its types are read from its own first line, so nothing is
declared a second time:

```zeno
import c

let frame = inline c {
    #include <stdint.h>
    #include <stddef.h>

    #define POLYNOMIAL 0xA001

    static uint16_t step(uint16_t crc) {
        return (crc & 1) ? (crc >> 1) ^ POLYNOMIAL : crc >> 1;
    }

    uint16_t crc16(const uint8_t *data, size_t length) {
        uint16_t crc = 0xFFFF;
        for (size_t i = 0; i < length; i++) {
            crc ^= data[i];
            for (int bit = 0; bit < 8; bit++) {
                crc = step(crc);
            }
        }
        return crc;
    }
}

let request: list[u8] = [0x01, 0x03, 0x00, 0x6B, 0x00, 0x03]
print(frame.crc16(request, len(request)))
```

```
6004
```

`crc16` is a function of `frame`. `step` is `static`, so it is the
block's own, and so are `POLYNOMIAL`, a variable, a `typedef` and a
`struct`: they are there for the block's functions, and Zeno leaves
them alone.

**A call is checked before the program runs**, as a call of a function
of Zeno is:

```
error: the 1st parameter of `crc16` (`const uint8_t *`) takes a pointer to uint8_t that C gave, a list of whole numbers or none, and this is int
error: `crc16` takes 2 arguments, but 1 was given
error: this `inline c` block has no function `step` that Zeno can call. It has: crc16. A function marked `static` is the block's own
```

**Values go across as they do for `c.library`.** A whole number is
checked to fit the type the function takes, a list is lent for a
pointer and copied back unless the pointer is to `const`, text goes as
a `const char *`, and any other pointer C gives back is kept as it is.
The table is in [`c.md`](c.md#values-go-across), and it is one table
for both.

**A header is included as in C.** One of the system in `< >`, one of
the program in `" "`, found from the folder of the `.zeno` file.

**What the C compiler says names the `.zeno` file.** A mistake in the
block is the C compiler's to find, and it is told where each line of
the block was written:

```
error: C could not build this `inline c` block. The C compiler said:
sensor.zeno: In function 'twice':
sensor.zeno:4:31: error: 'y' undeclared (first use in this function)
    4 |     int twice(int x) { return 2 * y; }
      |                               ^
```

The line is the line of the `.zeno` file. The column is counted from
where the lines of the block begin, since the indentation they share
is not part of the block; the compiler shows the line and marks the
place.

A block that does not compile stops the program where the block is.
No `try` catches it: the program is wrong, and running it again will
not help.

### What `zeno build` does with it

A build that writes C writes the block into the program's C, as it
was written, before anything of the program's own. There is no
library beside the program and nothing between a call and the
function: `frame.crc16(request, len(request))` is a call of `crc16` in
the same file, which the C compiler may put in line. That is every
build but `--llvm`: `zeno build`, `--firmware`, `--no-collector`,
`--owned`, `--minimal`, `--release`, `--fast`, `--board`, and
`zeno test --build`.

The block is marked with where it came from:

```c
#line 4 "sensor.zeno"
#include <stdint.h>
...
#line 4498 "sensor.c"
```

so a mistake the C compiler finds, and a line a debugger stops at, is
a line of the `.zeno` file.

Under `zeno run` the block is built once into a small library, kept,
and called through the helper that `import c` uses; it is built again
when the block changes. A call goes through the helper there, as a
call of `c.library` does. In a built program it is a call of C.

### Assembly

Assembly is written inside a block of C, as the C compiler takes it:

```zeno
import c

let cpu = inline c {
    #include <stdint.h>

    /* Interrupts off, on a Cortex-M. What PRIMASK was is given back. */
    uint32_t interrupts_off(void) {
        uint32_t was;
        __asm__ volatile ("mrs %0, primask\n cpsid i" : "=r" (was));
        return was;
    }
}
```

There is no `inline asm`. Assembly has no types, so nothing in it says
what a routine takes or gives, and the C function around it is what
says so. It is also not one language: each processor has its own
instructions and each compiler its own way of writing them, and the C
compiler already knows both.

A whole file of assembly is given to `c.library`, beside the `.c`
files it takes: `c.library("startup.s", "void reset(void);")`. The C
compiler assembles it, a `.s` as it is and a `.S` after reading its
`#include` and `#define` lines.

### What a block of C cannot have

| | What Zeno says to do |
|---|---|
| A function Zeno cannot call that is not `static`: one that takes `...`, a function, or a whole struct | Mark it `static` if only the block calls it, or write a function beside it that Zeno can call |
| No function Zeno can call | Write one out, with its body, without `static` |
| C++ | Functions of C++ marked `extern "C"`, built into a library, go to `c.library` |

## A block of Python

```zeno
import python

let stats = inline python {
    import statistics

    def summary(readings):
        """The middle and the spread of some readings."""
        return [statistics.median(readings), statistics.pstdev(readings)]
}

print(stats.summary([21.5, 22.0, 23.5]))
```

```
[22.0, 0.8498365855987975]
```

The block is a module of Python: what `python.module` would give if
the lines were in a `.py` file. Everything it defines is reached with
a dot: a function, a class, a variable, and a module the block
imported (`stats.statistics`). Values go across as for
[`import python`](python.md).

The block runs when the program gets to it, once, top to bottom, as a
module does when it is imported. An error of Python in it stops the
program, and names the line of the `.zeno` file:

```
error: this `inline python` block could not be started. Python said: SyntaxError: invalid syntax (report.zeno, line 9)
```

## A block of JavaScript

```zeno
import node

let written = inline node {
    exports.number = (value, place) => new Intl.NumberFormat(place).format(value)
}

print(written.number(1234567.5, "en-IN"))
```

```
12,34,567.5
```

The block is a module of Node, as a `.js` file is: what it puts in
`exports`, or gives `module.exports`, is what the block gives, and
`require` finds the packages `npm install` put in the program's
folder. It is a module of the older kind, with `require` and
`exports`; `import` and `export` are not read there. Values go across
as for [`import node`](node.md).

## What each command does with a block

| | `inline c` | `inline python`, `inline node` |
|---|---|---|
| `zeno run`, `zeno run --vm`, `zeno test` | Built once, kept, and called through the C helper | Given to the Python or the Node that the `import` starts |
| `zeno check` | The functions and every call of them are checked | Checked when the program runs |
| `zeno build` and every build that writes C | Written into the program's C | Refused |
| `zeno check --strict` | As `zeno check` | Refused: what it gives is not known before the program runs |
| `zeno build --llvm` | Refused | Refused |
| `zeno build --target js` and the other targets | Refused | Refused |
| The playground on the website | Refused: a page has no C compiler | Refused |
| `zeno format` | The lines of the block are left as they are, one step in | The same |

A refusal is an error with the line of the block, when the program is
checked or built. A block is never skipped.

## What Zeno cannot check

The code in a block is the other language's, and Zeno checks none of
it. A pointer that is wrong in a block of C damages memory as it does
in C: a built program stops or goes wrong, and under `zeno run` the
helper stops and Zeno says which call it was waiting for. The blocks
of a program are the lines to read then, and `grep -n "inline c"`
finds them all, as `grep -n unsafe` finds the other lines of that
kind.

## What is not there yet

- **A library for a block.** A block of C that needs a library other
  than C's own (`-lz`) has nowhere to say so. Until it has, the code
  goes in a `.c` file given to `c.library` with the library.
- **A constant of a block.** A `#define` or a variable of a block is
  not seen by Zeno; a function that gives it is. `c.library` can
  declare a value ([`c.md`](c.md#values-a-header-names)).
- **Small types given back as themselves.** A `uint16_t` comes back
  as an `int` and a `float` as a `float`, as for `c.library`.
- **Blocks of Ruby, PHP, Lua and C#.** Each has an `import` already
  and can be given a block in the same way.
- **`zeno build --llvm`** with a block of C.

## How it is built

- [`src/lexer.rs`](../src/lexer.rs), `inline_block`: the one place
  the lexer looks further than two characters ahead, to the end of
  the line. A block is one token, `TokenKind::Inline`, that holds the
  language and the text.
- [`src/parser.rs`](../src/parser.rs): `ExprKind::Inline`, allowed
  only as the value of a `let`.
- [`src/checker.rs`](../src/checker.rs), `inline`: asks for the
  import, and types a block of C with
  `bridge_c::library_of_block`.
- [`src/bridge_c.rs`](../src/bridge_c.rs), `block_declarations`:
  finds the functions of a block of C, and makes of them the lines
  `c.library` would have been given. From there a block is a library:
  the checker, `zeno run` and `zeno build` use what `import c` built.
- [`src/inline.rs`](../src/inline.rs): what `zeno run` does when it
  gets to a block. A block of Python is compiled by Python's own
  `compile`, with as many empty lines before it as the `.zeno` file
  has above it, so that Python's line numbers are the file's.
- [`src/emit_c.rs`](../src/emit_c.rs), `c_libraries` and
  `with_blocks_placed`: the block in the program's C, and its two
  `#line`s.
- [`src/format.rs`](../src/format.rs), `block`: the formatter's part.

## Things to try

1. Run `examples/bits.zeno`, then build it and run what was built.
   Open `bits.c` and find the block. What is on the line before it,
   and on the line after?

2. In the first program of this page, write `count+++;` in the block
   and run it. Which file and line does the C compiler name?

3. Mark `ones` as `static` and run `zeno check`. Read the message.
   Then write a second function that is not `static` and calls it.

4. Write `let m = inline python {` in a file with no `import python`,
   and read what Zeno says.

5. Write a block of Python with a class in it, make an object of the
   class from Zeno, and call a method. Then do the same with a
   JavaScript class and `node.new`.

6. Put a `}` on a line by itself in the middle of a block of C, as far
   in as the rest of the block. Does it end the block? Move it out to
   where the `let` begins and run `zeno check` again.
