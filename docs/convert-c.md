# A file of C written again as Zeno: `zeno convert`

`zeno convert` reads C as well as Python ([`convert.md`](convert.md)):

```sh
zeno convert meter.c
```

It writes `meter.zeno` beside it, and says what it did:

```
Wrote meter.zeno from meter.c.
  9 pieces: 8 written as Zeno, 1 kept as C.

  Kept as C, in the `inline c` block:
    line 37, function `tick`: `static count`, a variable inside a function that keeps its value from one call to the next (line 38)

  Run it with `zeno run meter.zeno`. It prints what the C program prints,
  but for the notes above.
```

The promise is the one it makes for Python: **what is written is never
silently wrong**. What it can say in Zeno it says in Zeno, with the
same numbers C would give, bit for bit. What it cannot, it keeps as C,
in an [`inline c`](inline.md) block that the Zeno calls. So what it
writes always runs, prints what the C program printed, and the work
left is to make the block smaller.

The decisions behind it are chapter 63 of [`decisions.md`](decisions.md).

## An example

`meter.c`, a program of 58 lines (it is in
[`tools/convert`](../tools/convert), and is checked on every push):

```c
/* A meter: readings, an average, and a checksum. */
#include <stdio.h>
#include <stdint.h>

#define COUNT 5

struct Reading {
    const char *name;
    int value;
};

uint8_t checksum(const uint8_t *bytes, int count) {
    uint8_t sum = 0;
    for (int i = 0; i < count; i++) {
        sum += bytes[i];
    }
    return (uint8_t)~sum;
}

int average(const struct Reading readings[], int count) {
    int total = 0;
    for (int i = 0; i < count; i++) {
        total += readings[i].value;
    }
    return total / count;
}

void limits(const struct Reading readings[], int count, int *low, int *high) {
    *low = readings[0].value;
    *high = readings[0].value;
    for (int i = 1; i < count; i++) {
        if (readings[i].value < *low) *low = readings[i].value;
        if (readings[i].value > *high) *high = readings[i].value;
    }
}

uint32_t tick(void) {
    static uint32_t count = 0;
    return ++count;
}

int main(void) {
    struct Reading readings[COUNT] = {
        {"volts", 230}, {"amps", 12}, {"hertz", 50}, {"watts", 2760}, {"temp", -4},
    };
    for (int i = 0; i < COUNT; i++) {
        printf("%-6s %5d\n", readings[i].name, readings[i].value);
    }
    int low, high;
    limits(readings, COUNT, &low, &high);
    printf("average %d, low %d, high %d\n", average(readings, COUNT), low, high);
    uint8_t frame[] = {0x10, 0x20, 0xF0};
    printf("checksum 0x%02X\n", checksum(frame, sizeof frame));
    uint32_t first = tick();
    printf("tick %u, tick %u\n", first, tick());
    return 0;
}
```

is written as:

```zeno
# A meter: readings, an average, and a checksum.

import c

let c_part = inline c {
    #include <stdio.h>
    #include <stdint.h>
    #define COUNT 5
    struct Reading {
        const char *name;
        int value;
    };

    uint32_t tick(void) {
        static uint32_t count = 0;
        return ++count;
    }
}

# C's `/` of two whole numbers takes the quotient toward zero, where Zeno's `//`
# takes it down: -7 / 2 is -3 in C, and -7 // 2 is -4.
function c_divide(a: int, b: int) -> int {
    let down = a // b
    if down < 0 and down * b != a {
        return down + 1
    }
    return down
}

let COUNT = 5

class Reading {
    name: str
    value: int
}

function checksum(bytes: list[u8], count: int) -> u8 {
    var sum_: u8 = 0
    for i in range(0, count) {
        sum_ = u8.wrap(sum_ + bytes[i])
    }
    return u8.wrap(~sum_)
}

function average(readings: list[Reading], count: int) -> int {
    var total = 0
    for i in range(0, count) {
        total += readings[i].value
    }
    return c_divide(total, count)
}

function limits(readings: list[Reading], count: int, low: list[int], high: list[int]) {
    low[0] = readings[0].value
    high[0] = readings[0].value
    for i in range(1, count) {
        if readings[i].value < low[0] {
            low[0] = readings[i].value
        }
        if readings[i].value > high[0] {
            high[0] = readings[i].value
        }
    }
}

# `tick` is kept as C, in the block above.

function main() {
    let readings = [Reading("volts", 230), Reading("amps", 12), Reading("hertz", 50), Reading("watts", 2760), Reading("temp", -4)]
    for i in range(0, COUNT) {
        print(f"{readings[i].name:<6} {readings[i].value:5}")
    }
    var low = 0
    var high = 0
    let low_box = [low]
    let high_box = [high]
    limits(readings, COUNT, low_box, high_box)
    low = low_box[0]
    high = high_box[0]
    print(f"average {average(readings, COUNT)}, low {low}, high {high}")
    let frame: list[u8] = [0x10, 0x20, 0xF0]
    print(f"checksum 0x{checksum(frame, 3):02X}")
    let first: u32 = u32(c_part.tick())
    print(f"tick {first}, tick {c_part.tick()}")
}

main()
```

Both print the same eight lines. `tick` keeps a count from one call to
the next in a `static` variable, which Zeno has no word for, so it
stays C and `main` calls it as `c_part.tick()`. The rest is Zeno: the
struct is a class, the arrays are lists, `sum += bytes[i]` on a
`uint8_t` goes round at 256 as it does in C (`u8.wrap`), `total /
count` takes the quotient toward zero as C does, and the two pointers
`limits` writes through are lists of one.

## Pieces, and what becomes of each

The file is cut where its declarations end: at each `;` outside braces,
at the `}` that ends a function, and at each line of the preprocessor.
Each piece is one of these:

| Piece of C | Written as |
|---|---|
| `#include <...>` | Nothing: what it gives is written where it is used. It is copied into the block |
| `#define NAME value` | `let NAME = value` |
| `#define` with arguments | Kept: a macro. It goes into the block, and what uses it is kept |
| `#if`, `#ifdef`, `#error` and the like | The whole file is kept: they change what the file is, and the converter would have to choose |
| `typedef` | Nothing: the name stands for its type wherever it is used |
| `struct Name { ... };` | `class Name { ... }`, a field for each field |
| `enum { A, B = 5, C };` | `let A = 0`, `let B = 5`, `let C = 6` |
| A global, `int count = 0;` | `var count = 0`; `const` gives `let` |
| A function | `function`, with its parameters and what it gives back |
| A prototype | Nothing: Zeno needs no declaration before a call |
| `main` | `function main()`, called at the end of the file. When it may give back other than 0, the program ends with what it gives |

Comments are kept where they were. A comment on the lines before a
function stays before it.

## Types

The converter reads each type as C does on a 64-bit Linux, macOS or
Windows machine: `char` 8 bits, `short` 16, `int` 32, `long` 64 (32 on
Windows, which it does not follow), pointers 64.

| C | Zeno | Why |
|---|---|---|
| `signed char`, `short`, `int`, `long`, `long long`, `int8_t` ... `int64_t` | `int` | Zeno's `int` holds all of them. Where C stores a value in a smaller type (`int x = big;`), the Zeno checks it fits: see the notes |
| `unsigned char`, `uint8_t` | `u8` | Goes round at 256, as in C |
| `unsigned short`, `uint16_t` | `u16` | Goes round at 65536 |
| `unsigned int`, `uint32_t` | `u32` | Goes round at 2**32 |
| `uint64_t`, `unsigned long`, `unsigned long long` | `u64` | Goes round at 2**64, written `u64.wrap(...)`. An `int` beside one is made a `u64` as C makes it: `u64(n)`, or `u64.wrap(n)` when it may be below zero |
| `size_t` | `int` | It counts bytes and items, never past 2**63, and stands beside `int` on nearly every line, which a `u64` meets only through `u64( )`. What could differ is kept: `-` or `~` of one, or one beside a number that may be below zero |
| `float` | `f32` | The same 32 bits, so the same sums |
| `double` | `float` | The same 64 bits |
| `long double` | Kept | More bits than any float of Zeno |
| `bool`, `_Bool` | `bool` | |
| `char *`, `const char *` | `str` | When it holds a text. A `char` array that C writes into is kept |
| `struct Name` | `Name`, a class | A copy where C copies (below) |
| `int values[5]` | `list[int]` | `[0] * 5` when it is not given values, as C gives a global zeros |
| `int grid[3][4]` | `list[list[int]]` | |

### Sums

Every sum gives what C gives:

- **Unsigned** (`u8`, `u16`, `u32`, `u64`): a sum that goes past the
  top goes round, written `u8.wrap(...)` ... `u64.wrap(...)`. It is
  written only where the value is looked at or stored, so `a + b + c`
  is one wrap. A hash of 64 bits reads as it does in C:
  `hash = u64.wrap(hash * FNV_PRIME)`, `h ^= u64.wrap(h << 13)`.
  A product of two `u32` that may need more than 62 bits goes through a
  helper `times_u32`, so it does not stop with `OverflowError` before
  it is cut.
- **Smaller types**: C works out a sum of two `char` or `short` as an
  `int`, and the Zeno does too.
- **Signed** sums that go past the top are undefined in C. Zeno stops
  with `OverflowError` past 64 bits, and checks a store into a smaller
  type (`i32(...)`): never silent.
- **`/` and `%`** of whole numbers that may be below zero go through
  `c_divide` and `c_remainder`, which take the quotient toward zero as C
  does. When both sides are never below zero, they are `//` and `%`.
- **`<<`** of a `u32` by a count written out is a mask (`(x <<
  3) & 0xFFFFFFFF`); by a count that is not, it is kept.
- **Floats**: `float` sums are `f32` sums, so they round where C's
  round. `(int)f` cuts toward zero, as `int(f)` does.
- `5 / 2` is 2 and `5.0 / 2` is 2.5, as in C.

### `printf`

A `printf` whose format is written out is an f-string:

| C | Zeno |
|---|---|
| `%d %i %u %ld %zu` | `{x}` |
| `%5d %-5d %05d` | `{x:5}` `{x:<5}` `{x:05}` |
| `%x %X %08X` | `{x:x}` `{x:X}` `{x:08X}` |
| `%f %.2f %8.3f` | `{x:.6f}` `{x:.2f}` `{x:8.3f}` |
| `%s %-10s %10s` | `{s}` `{s:<10}` `{s:>10}` |
| `%c` | `{letter(c)}`, a helper written once |
| `%%` | `%` |

`%e`, `%g`, `%o`, `%p`, `%+d`, `% d`, `%*d` and `%.3s` are kept: Zeno
does not show them so. A `printf` that ends with `\n` is `print`. When
any line of the program is written in parts (a `printf` without the
`\n`, `putchar`), everything it writes goes through one helper,
`write_text`, which holds a line until it ends, and the program ends
with `finish_text()`. `puts`, `putchar`, `fputs(s, stdout)` are written
the same way. `fprintf(stderr, ...)` of a whole line is
`program.warn(...)`, and `exit(n)` is `program.exit(n)`.

### Structs

A struct is a class, and a class is a reference in Zeno where a struct
is a value in C. So the converter writes a copy wherever C copies:

- `b = a;` of a struct, a struct given to a function that changes it,
  a struct put into an array: `a.copy()`, a method written once in the
  class, which copies the fields (and the structs and arrays inside).
- A struct given to a function that only reads it, or given back from
  where it was made, is not copied: nothing can tell.
- `p->field` where `p` is a parameter of a function is the object
  itself, which is what the pointer meant.

### Pointers

C's pointers are of a few kinds, and the converter writes each:

| A pointer parameter | Written as |
|---|---|
| `const int *values`, `int values[]`, with an index | `list[int]` |
| `int *out` to one number, written through with `*out` | `list[int]` of one. The caller writes `let x_box = [x]`, the call, and `x = x_box[0]` |
| `struct Point *p` | `Point`, the object |
| `const char *s` | `str` |

The address of a place in an array, `&values[j]`, is given the same
way. A call that gives the same variable twice, or a variable and a
place in it, is kept, since the two would be one in C and two in Zeno.
Anything else a pointer does is kept: a pointer variable, `p++`, `*p`
of a pointer that is not a parameter, `NULL`, `malloc`, a list made of
`next` pointers, a pointer to a function.

### Statements

| C | Zeno |
|---|---|
| `for (int i = 0; i < n; i++)`, with `i` and `n` not changed in it | `for i in range(0, n)` |
| Any other `for` | `var i = ...` and `while`, with the step at the end. A `continue` in it is kept, since it would pass by the step |
| `while`, `if`, `else if`, `break`, `return` | The same |
| `do { ... } while (t);` | `while true { ...; if not t { break } }` |
| `switch` | `if` and `else if`. A `case` that falls into the next one is kept |
| `x++`, `x += 2` | `x += 1`, `x += 2` |
| `a[i++] = v` | `a[i] = v` and then `i += 1`, when `i` is named once there |
| `c = cond ? a : b;` | `if` and `else`, as statements |
| `goto`, a label, `,` | Kept |

`int`, `char` and `bool` are kept apart, as Zeno keeps them: a test
`if (n)` of a number is `if n != 0`, `if (!p)` of a pointer is kept, a
comparison used as a number is kept.

## What is kept, and why

A piece is kept when one thing in it cannot be written as Zeno. The
report gives the line of that thing and why. The kept pieces go into
one block, `let c_part = inline c { ... }`, with every `#include`,
`#define`, struct, enum and typedef of the file copied in front of
them, so that they are compiled as they were. `static` is taken off a
kept function, so the Zeno can reach it.

Things the converter keeps, most often:

- A pointer variable, `malloc` and `free`, `NULL`, lists made of
  pointers, pointer sums.
- A `static` variable inside a function.
- A `char` array that C writes into (`char line[80]`, `strcpy`,
  `sprintf`, `strcat`).
- A macro with arguments, and everything that uses one.
- `%e`, `%g` and the other formats Zeno shows otherwise.
- A `switch` whose `case` falls into the next.
- `goto`, the operator `,`, `? :` inside an expression, an assignment
  used as a value.
- `main(int argc, char **argv)`. It is kept, and a few lines of C put
  at the end of the block give it the program's arguments: the Zeno
  passes the name of the file and then each word of
  `program.arguments()`, so `zeno run tool.zeno a b` gives `main` what
  `./tool a b` gives it, and the program ends with what `main` gives
  back.

### What crosses between Zeno and the block

C can call only C, and a variable lives on one side. So:

- A function the kept C calls is kept too.
- A global a kept function uses is kept, and a Zeno function that uses
  it is kept in turn.
- The Zeno calls a kept function through `c_part`, giving it numbers,
  floats, bools and texts it does not change (`const char *`). A struct,
  an array, a text C may change, or a pointer is not given: the Zeno
  function that would give one is kept.

### How a piece is chosen to be kept

The converter writes every piece it can. Each piece it cannot write is
kept, then what that drags in (above), until nothing changes. The
whole program is put together and read by Zeno's own checker, in the
same process; a piece the checker turns down is kept, and it goes
round again. If the checker turns down a line that no piece owns, the
whole file is kept, which still runs.

## The notes

Where Zeno does otherwise than C, the line has a `# convert:` comment
at its first place of each kind, and the report lists the lines:

- **A number stored in a smaller type that it does not fit**: `int x =
  big;` where `big` is past 32 bits stops the program with
  `OverflowError` in Zeno, where C keeps the bits that fit.
- **The last line written in parts is ended**: a program whose last
  `printf` leaves its line without `\n` gets one at the end.
- **`argv[0]` is the name of the file**: a `main` that reads its
  arguments is given `"tool"` for `tool.c` as `argv[0]`, where C gives
  the path it was started by, such as `./tool`.

## What it does not promise

- **What C leaves undefined**: a signed sum past the top, an index past
  the end of an array, a variable read before it has a value. Zeno
  stops on the first two. A variable C left without a value starts at
  0 in Zeno.
- **The order C works out the arguments of one call in**: `f(next(),
  next())` is left to the compiler in C; Zeno works them out left to
  right.
- **A `size_t` past 2\*\*63**, or a `uint64_t` past it stored in a
  signed `long`: both are `int` in Zeno, and stop the program with
  `OverflowError` where C would go on. Loud, never wrong.
- **`long` as 32 bits**, as on Windows: the converter reads it as 64.
- **The last digit of `sin`, `cos` and the like**, which comes from
  the C library of the machine.
- **What a kept piece does with what it shares**: a kept function is C,
  and does what C does.

## Checking it

```sh
python3 tools/convert.py             # every program in tools/convert
python3 tools/convert.py meter.c     # one
```

Each program of C in [`tools/convert`](../tools/convert) is compiled
with `cc -std=c11` and run, converted and run with `zeno run`, and what
the two print and the status they end with are compared. The first line
of each says how many pieces are to be kept (`// kept: 1`). Among them
are a ring buffer, a frame parser and a state machine as they are
written for a board (`firmware.c`), a hash of 64 bits and two random
generators (`hashes.c`), a sieve of primes, unsigned sums at
each size, structs copied and changed, pointers to one number and
arrays, `switch`, `printf` in parts, a `main` that reads its
arguments, and a file with `#ifdef` that is kept whole. The GitHub workflow `convert.yml` runs them on every push
that changes the converter, with `cargo test convert`.

## Where it is

| File | What it has |
|---|---|
| `src/convert.rs` | The command, the notes and the report, for both languages |
| `src/convert_c.rs` | Reading C: the tokens, the pieces, the declarations, the types |
| `src/convert_c_lines.rs` | Writing an expression, a statement and a function of C as Zeno, with its type; the helpers |
| `src/convert_c_write.rs` | What each piece calls and uses, which are kept, the block, putting the program together, the checker |
| `tools/convert.py`, `tools/convert/*.c` | The programs and the check |
