# Zeno

> Every journey is halfway done. Forever.

Zeno is a small programming language, built from scratch as a learning project.
It is named after Zeno of Elea, whose paradoxes say Achilles can never catch the
tortoise, and after the quantum Zeno effect, where a system watched closely
enough never changes. A fitting name for a compiler that will always be almost finished.

## The idea

Zeno should read like plain English and still be checked like Rust. Two rules shape the syntax:

1. **Words over symbols.** Keywords are whole words you can read aloud: `function`,
   `and`, `or`, `not`, `match`, `case`, `raise`. No `fn`, `&&`, `=>` or `?`.
2. **Punctuation has to earn its place.**

| Kept | Why |
|---|---|
| `{ }` around blocks | You can always see where a block ends, and a stray space can't change what the code does |
| `( )` for calls, `[ ]` for lists and dicts, `,` between items | Everyone already reads these |
| `:` before a type, `->` before a return type | Separates a name from its type at a glance |
| `#` for comments | One character, hard to miss |
| `&`, `|`, `^`, `~`, `<<`, `>>` on whole numbers | How every datasheet writes a register |

| Dropped | Why |
|---|---|
| `;` at the end of a line | The line break already says it |
| `:` after `if`, `for`, `function` | The brace already says it |
| `( )` around conditions | `if x > 0 {` needs no help |
| `&&`, `||`, `!` | `and`, `or`, `not` read better |

Variables say once whether they can change: `let` for a value that is set once,
`var` for one that changes, and plain `x = 2` to update. A misspelt name is an
error, not a new variable.

Zeno does not make you pick a style:

- **Object-oriented and functional.** Classes with methods, and also functions
  as values, closures, `map` / `filter` / `reduce` and comprehensions.
- **Types are optional.** Write `function area(w: float, h: float) -> float`
  or just `function area(w, h)`. Start loose, add types as the program grows.

Underneath:

- **Types, where written, are checked before the program runs.** Where they are left out, they are worked out when possible.
- **Errors are checked.** A function says what it `raises`, and the compiler makes sure someone handles it.
- **No surprise `none`.** A value can be `none` only if its type says `or none`, and you must check before using it.
- **Enums carry data**, and `match` must cover every case.
- **Generics.** `class Ring[T]` is written once and used for bytes, voltages and text. Each use is checked, and compiled as if written by hand for that type.
- **Whole numbers have a size where it matters.** A `u8` field is one byte, a number that does not fit is an error, and wrapping is something you ask for.
- **Memory by hand, where you ask for it.** `allocate`, raw pointers, `free` and registers at fixed addresses, as in C. The lines Zeno cannot check go inside `unsafe { }`, and `zeno run` checks them anyway.
- **Compiled, not interpreted**, so it is fast and small enough for a microcontroller.

Samples, none of them final:

- [`examples/hello.zeno`](examples/hello.zeno): one line.
- [`examples/tour.zeno`](examples/tour.zeno): the main features.
- [`examples/styles.zeno`](examples/styles.zeno): one job written typed, untyped, object-oriented and functional.
- [`examples/primes.zeno`](examples/primes.zeno): no types written, and it compiles to C.
- [`examples/shapes.zeno`](examples/shapes.zeno): classes, enums, lists and `match`, compiled to C.
- [`examples/sensor.zeno`](examples/sensor.zeno): values that may be `none`, errors, and functions as values.
- [`examples/memory.zeno`](examples/memory.zeno): a program that makes far more memory than it keeps, and the collector that gives it back.
- [`examples/buffer.zeno`](examples/buffer.zeno): owned values, which are freed at a line you can point to. It also builds with no collector at all.
- [`examples/labels.zeno`](examples/labels.zeno): text that is built and kept, in a build with no collector.
- [`examples/words.zeno`](examples/words.zeno): a line split into words, with lists that nobody keeps and the statement frees.
- [`examples/scheduler.zeno`](examples/scheduler.zeno): tasks in a list, with no collector and not one `owned`: each list frees its items.
- [`examples/commands.zeno`](examples/commands.zeno): commands read from text, as an enum whose cases carry values, with no collector. A line that is no command is `none`.
- [`examples/handlers.zeno`](examples/handlers.zeno): steps kept as function values and faults kept as errors, with no collector. Each has one owner, which frees it.
- [`examples/driver.zeno`](examples/driver.zeno): a driver for a serial port, with a queue of bytes that is allocated and freed by hand and three registers, inside `unsafe { }`.
- [`examples/sentence.zeno`](examples/sentence.zeno): lines from a sensor, checked and taken apart with `find`, `slice`, `sort`, `join` and `code`, the methods of text and lists.
- [`examples/logbook.zeno`](examples/logbook.zeno): readings typed at the keyboard, kept in a file named on the command line, and read back: `input`, `files` and `program.arguments`.
- [`examples/download.zeno`](examples/download.zeno): a video from YouTube saved in a folder, for videos you have the right to keep. The program starts [yt-dlp](https://github.com/yt-dlp/yt-dlp) with `program.run`, which gives another program its words as a list, and says how it went. Run it with `zeno run`.
- [`examples/video.zeno`](examples/video.zeno): the same job with yt-dlp as a library. `import python` lets a Zeno program use any package of Python that is installed: `python.module("yt_dlp")` is the module, and its answers come back as values of Zeno, so the program can say what a video is called and how long it is before it saves anything. Run it with `zeno run`.
- [`examples/checksum.zeno`](examples/checksum.zeno): the SHA-256 of a file, the number a download page shows beside it. Zeno has none of its own, and `import node` lets a program use the `crypto` that comes with Node, or any package that `npm install` put beside the program. Run it with `zeno run`.
- [`examples/zipped.zeno`](examples/zipped.zeno): what is in a zip file, with each file's size and how much smaller the zip made it. `import java` lets a program use every class that comes with Java, here `java.util.zip`, and any library that is a jar. Run it with `zeno run`.
- [`examples/noise.zeno`](examples/noise.zeno): a converter's reading with noise on it, kept in range and smoothed, and how far a joystick is pushed. `random`, `math.clamp` and `math.hypot` are written in Zeno itself, so every engine and every target gives the same numbers.
- [`examples/settings.zeno`](examples/settings.zeno): a settings file written in YAML, printed as JSON. Zeno reads JSON and not YAML, and `import ruby` lets a program use the `yaml` that comes with Ruby, or any gem that is installed. Run it with `zeno run`.
- [`examples/find.zeno`](examples/find.zeno): every line of a file that a pattern is found in, and what was found. Zeno has no patterns for text, and `import php` lets a program use PHP's `preg_match_all`, or any package that `composer require` put beside the program. Run it with `zeno run`.
- [`examples/records.zeno`](examples/records.zeno): the records of a binary file, field by field, as a logger or a radio wrote them. Zeno reads text, and `import lua` lets a program use Lua's `string.unpack`, or any module that `luarocks install` put on the computer. Run it with `zeno run`.
- [`examples/clock.zeno`](examples/clock.zeno): what time it is now in each place named, with how far ahead of UTC it is there, for a log stamped in another country or a board that keeps UTC. Zeno has no time zones, and `import csharp` lets a program use the ones .NET has, or any package that `dotnet add package` gave the project beside it. Run it with `zeno run`.
- [`examples/modbus.zeno`](examples/modbus.zeno): a request of Modbus RTU, and the frames that come back checked, with the checksum written in C as a board's firmware already has it ([`examples/crc.c`](examples/crc.c)). `import c` builds the file of C beside the program and calls it as it is. Run it with `zeno run`.
- [`examples/bits.zeno`](examples/bits.zeno): the bits of a byte in the other order, how many are 1, and a hash that wraps round at 32 bits, each a few lines of C written in the `.zeno` file itself, in an `inline c` block. Under `zeno run` the block is built once and called; `zeno build` writes it into the program's C, so the built program calls it as C calls C.
- [`examples/report.zeno`](examples/report.zeno): one file, three languages. An `inline python` block works out the middle and the spread of some readings with Python's `statistics`, an `inline node` block writes the numbers as India, the United States and Germany write them, and Zeno is the program around both. Run it with `zeno run`.
- [`examples/instruments.zeno`](examples/instruments.zeno): a compass, a thermistor, a level meter, a charging capacitor and a sine table, with the `math` module and angles that carry their unit.
- [`examples/faults.zeno`](examples/faults.zeno): fault codes counted and looked up in a dict: `["mode": "auto"]`, `get`, `in`, `pop` and a loop over the keys.
- [`examples/channels.zeno`](examples/channels.zeno): a dict that holds lists and objects. The readings of each channel are a list under its name, and `for channel, values in readings` is given each key and what is under it.
- [`examples/station`](examples/station/main.zeno): one program in four files. A weather station whose sensors, report and serial port are each a module that `main.zeno` imports.
- [`examples/thermostat`](examples/thermostat/main.zeno): a program that needs a package. Its `zeno.package` says `needs filters in "../filters"`, and [`examples/filters`](examples/filters/main.zeno) is that package: a folder of modules with a `zeno.package` of its own.
- [`examples/ring`](examples/ring/main.zeno): a module that keeps its insides to itself. A queue of a fixed size whose slots and count are `private`, so that only `ring.zeno` can change them. The module has its tests at the end of its file: `zeno test examples/ring` runs them.
- [`examples/ports.zeno`](examples/ports.zeno): an `interface`. A UART and a log in memory are both a `Port`, and the code that sends a report takes either.
- [`examples/todo.zeno`](examples/todo.zeno): a page in a browser. A list of things to do, with [`todo.html`](examples/todo.html) as the page and `import web` to reach it. `zeno build --web` makes the two into one file that a browser opens.
- [`examples/sketch.zeno`](examples/sketch.zeno): a sheet to draw on with the mouse, with [`sketch.html`](examples/sketch.html) as the page. A function asks for the `Event` to learn where the mouse is, draws on a `<canvas>`, sets the style of one element, and keeps the chosen colour for the next visit. Its tests press the mouse with `fire`.
- [`examples/blink.zeno`](examples/blink.zeno): the same language on a microcontroller. It blinks the LED of a simulated board.
- [`examples/signal.zeno`](examples/signal.zeno): a traffic light on three pins, as a state machine. It builds for firmware with no collector.
- [`examples/doorbell.zeno`](examples/doorbell.zeno): a button on an input pin with a pull-up rings a bell, once for each press.
- [`examples/console.zeno`](examples/console.zeno): a command line over a serial port that drives the LED.
- [`examples/battery.zeno`](examples/battery.zeno): a battery read through a divider on an analog pin, in volts, with the LED on when it is low.
- [`examples/dimmer.zeno`](examples/dimmer.zeno): a lamp faded up and down and a servo swept, with a PWM signal on two pins.
- [`examples/steady.zeno`](examples/steady.zeno): a loop that runs at a steady rate on a ticker, and notices the round that took too long.
- [`examples/tachometer.zeno`](examples/tachometer.zeno): the turns of a wheel counted by an interrupt, read once in each stretch of time.
- [`examples/packet.zeno`](examples/packet.zeno): sensor readings packed into six-byte frames and unpacked again, with `u8`, `u16` and the bit operators.
- [`examples/protocol.zeno`](examples/protocol.zeno): a small serial protocol whose commands and status codes are numbers on the wire and names in the program: an enum whose cases stand for numbers, `int(Ok)` and `Command(byte)`.
- [`examples/generics.zeno`](examples/generics.zeno): a ring buffer written once with `class Ring[T]`, and used for bytes, voltages and text.

## One language, every job

The goal is that anything you would reach for C, C++, Python, JavaScript, Java or Rust
to do, you can do in Zeno. That is a long road, and each use case arrives with a
piece of the compiler:

| Use case | What gets it there |
|---|---|
| Scripts, automation, a REPL | Interpreter and bytecode VM (`zeno run`) |
| Microcontrollers and firmware | Compile to C, no heap required |
| Command-line tools, desktop, servers | Native code through C or LLVM |
| Calling existing C libraries | C interop, in both directions |
| Using the libraries of other languages | `import python` starts the Python that is installed beside the program, and any package of it is used as it is ([`docs/python.md`](docs/python.md)). `import node` does the same for JavaScript: the library of Node and the packages of npm ([`docs/node.md`](docs/node.md)). `import java` does it for Java: its own classes and any jar ([`docs/java.md`](docs/java.md)). `import ruby` does it for Ruby: its own library and any gem ([`docs/ruby.md`](docs/ruby.md)). `import php` does it for PHP: its own functions and classes and any Composer package ([`docs/php.md`](docs/php.md)). `import lua` does it for Lua: its own library and any module of LuaRocks ([`docs/lua.md`](docs/lua.md)). `import csharp` does it for C# and .NET: its own types and any package of NuGet ([`docs/csharp.md`](docs/csharp.md)). `import c` calls a library of C, or of Go or Rust built with their C interface, from the declarations of the functions the program uses ([`docs/c.md`](docs/c.md)). A few lines of C, Python or JavaScript can be written in the `.zeno` file itself, in an `inline` block ([`docs/inline.md`](docs/inline.md)). A whole file of Python or of C can be written again as Zeno with `zeno convert`, which keeps what it cannot rewrite in such a block, so that what it writes always runs ([`docs/convert.md`](docs/convert.md), [`docs/convert-c.md`](docs/convert-c.md)) |
| Web front ends | Zeno itself built for WebAssembly, and the `web` module (`zeno build --web`); the program compiled to JavaScript (`--target js`, `--target html`) |
| Web back ends, networking | `async` / `await` and a standard library |
| Data and numerics | Fast arrays and operator overloading |
| Mobile apps, games | Native code plus platform bindings |

## Install

**Windows.** Download `zeno-0.8.0-windows-setup.exe` from the
[releases page](https://github.com/zeno-src/zeno-lang/releases) and
run it. It installs Zeno for you, puts it on the PATH, and offers to
install a C compiler and Git if the machine has none. Windows warns
about it the first time, because it is not signed: "More info", then
"Run anyway".

**macOS and Linux.** One line in a terminal:

```sh
curl -fsSL https://github.com/zeno-src/zeno-lang/releases/latest/download/install.sh | sh
```

It finds the `zeno` for your machine, checks the download, installs it
in `~/.zeno`, puts it on the PATH, and offers to install a C compiler
and git if there is none. [`docs/install.md`](docs/install.md) says
what both do, line by line, and how to take Zeno off again.

**With a package manager.** Each release has a package for apt,
pacman, snap and flatpak, installed from the file:

```sh
sudo apt install ./zeno_0.8.0_amd64.deb                         # Debian, Ubuntu, Raspberry Pi OS
sudo pacman -U zeno-lang-bin-0.8.0-1-x86_64.pkg.tar.zst         # Arch
sudo snap install --classic --dangerous zeno_0.8.0_amd64.snap
flatpak install --user zeno-0.8.0-x86_64.flatpak
```

The `.deb` is also there for `arm64`, `armhf` (every Raspberry Pi with
a 32-bit system) and `i386`, the snap for `arm64` and `armhf`, and the
flatpak for `aarch64`. apt installs a C compiler and git along with
Zeno if the machine has none.

Homebrew, on macOS and Linux:

```sh
brew tap zeno-src/zeno-lang https://github.com/zeno-src/zeno-lang
brew install zeno-src/zeno-lang/zeno
```

For winget there are three files in
[packaging/winget](packaging/winget),
which are what Microsoft's collection of packages takes.
[docs/packages.md](docs/packages.md)
says what each package is, and
[docs/stores.md](docs/stores.md)
what is still to be done before `apt install zeno` and the rest work
with no file: for apt a signed list in the release itself, for pacman
the AUR, for snap the Snap Store, and for winget a pull request.

**By hand.** Each release also has a `zeno` that is already built, as
an archive, for ten kinds of machine:

| Machine | File |
|---|---|
| Linux, x86-64: most PCs and servers | `zeno-0.8.0-linux-x86_64.tar.gz` |
| Linux, ARM64: a Raspberry Pi 3, 4 or 5 with a 64-bit system, ARM servers | `zeno-0.8.0-linux-arm64.tar.gz` |
| Linux, 32-bit ARMv7: a Raspberry Pi 2 or later with a 32-bit system | `zeno-0.8.0-linux-armv7.tar.gz` |
| Linux, 32-bit ARMv6: a Raspberry Pi 1 or Zero | `zeno-0.8.0-linux-armv6.tar.gz` |
| Linux, 32-bit x86 | `zeno-0.8.0-linux-x86.tar.gz` |
| macOS, Apple silicon | `zeno-0.8.0-macos-arm64.tar.gz` |
| macOS, Intel | `zeno-0.8.0-macos-x86_64.tar.gz` |
| Windows, x86-64 | `zeno-0.8.0-windows-x86_64.zip` |
| Windows, 32-bit x86 | `zeno-0.8.0-windows-x86.zip` |
| Windows, ARM64 | `zeno-0.8.0-windows-arm64.zip` |

Unpack it and put `zeno` (`zeno.exe` on Windows) somewhere on your
`PATH`. It is one file and needs nothing else installed. To build web
pages, put `zeno.wasm` from the same folder beside it.

```sh
tar -xzf zeno-0.8.0-linux-x86_64.tar.gz
cd zeno-0.8.0-linux-x86_64
./zeno --version                 # zeno 0.8.0
./zeno run examples/hello.zeno   # Hello, Zeno!
```

On macOS, a program downloaded with a browser is held back because
Apple has not signed it: `xattr -d com.apple.quarantine zeno` lets it
run.

`zeno build` hands the C it writes to a C compiler, so it needs one:
`cc`, `gcc` or `clang`, whichever it finds first, or the one `CC`
names. `zeno build --llvm` needs clang. `zeno fetch`, `zeno search` and
`zeno publish` need git.
Everything else needs nothing. The installers offer to install these.

Or build it from this source, which needs only
[Rust](https://rustup.rs): `cargo build --release` puts `zeno` in
`target/release/`. For web pages, `cargo build --release --target
wasm32-unknown-unknown` builds `zeno.wasm` as well
([`docs/web.md`](docs/web.md)). How a release is made, and what is checked before
one goes out, is in [`docs/release.md`](docs/release.md).

## Status

**Zeno is at version 0.2.** The language is complete as it was set out
for 0.1: everything on the roadmap below is done, and every decision
that was once marked provisional has been read again and settled
([`docs/version-0.1.md`](docs/version-0.1.md), which also lists what
0.1 leaves out). Version 0.2 adds a program in a web page
([`docs/web.md`](docs/web.md)) and installers
([`docs/install.md`](docs/install.md)), and changes nothing that ran
before.

Zeno checks and runs programs, and compiles nearly all of the language to C:
numbers, strings, functions, control flow, classes, lists, enums and `match`,
values that may be `none`, errors (`try` / `except` / `raise`), and functions
used as values. A compiled program prints what `zeno run` prints;
[`docs/compiler.md`](docs/compiler.md) lists the two small exceptions.
It also frees the memory it no longer uses, with a small garbage collector
that is compiled in ([`docs/memory.md`](docs/memory.md)). A value marked
`owned` has one owner and is freed the moment its owner's block ends, without
waiting for the collector ([`docs/ownership.md`](docs/ownership.md)).
`zeno build --firmware` writes C with no `setjmp`, where errors are return
values that every caller tests ([`docs/firmware.md`](docs/firmware.md)).
`zeno build --no-collector` goes one step further and leaves the
collector out of the C altogether
([`docs/no-collector.md`](docs/no-collector.md)). In that build every
list and object has one owner, whoever received it, with no word to
write ([`docs/one-owner.md`](docs/one-owner.md)), and so has an enum
value that carries something and a value that may be `none`
([`docs/enums-and-none.md`](docs/enums-and-none.md)), and so has a
function value and an error
([`docs/functions-and-errors.md`](docs/functions-and-errors.md)). Such a program does
not use the heap either: its lists and objects live in a pool of a
fixed size that is part of the program ([`docs/pool.md`](docs/pool.md)).
Text that such a program keeps is a copy that its keeper frees
([`docs/text.md`](docs/text.md)), and a list that is made and given to
nobody is freed by the statement that made it
([`docs/temporaries.md`](docs/temporaries.md)).

Beside both, a program can look after memory by hand, as C does:
`allocate`, a raw `pointer[u8]`, `free`, and a `Register[u32]` at a
fixed address. The lines that Zeno cannot check go inside `unsafe { }`.
`zeno run` checks every one of them anyway, and a compiled program
does what C does ([`docs/unsafe.md`](docs/unsafe.md)).

`zeno run` has two engines. The interpreter walks the syntax tree. With
`--vm` the program is compiled to bytecode and run by a small virtual
machine, which is two to three times as fast and prints exactly the same
([`docs/vm.md`](docs/vm.md)). Every test program is run on both.

`zeno build --llvm` compiles a second way: to LLVM IR, the input of the
compiler library behind Clang and Rust, which clang turns into a
program ([`docs/llvm.md`](docs/llvm.md)). It compiles what the C backend
compiles, and the tests build every program both ways. It takes the
same flags: `--firmware` (errors as values, no `setjmp` in the IR),
`--no-collector` (owned values freed where their owner ends, no
collector in the program) and `--memory`.

`zeno build --minimal` writes the least C that runs the program, close
to what a person would write by hand: no check of any kind, no line
numbers carried along, only the parts of Zeno's runtime the program
uses, and `print` as C's `printf`. `examples/hello.zeno` is 23 lines of
C that way and `examples/primes.zeno` is 100, from about 4,500 each. A
mistake in such a build does what it would do in C, so it is for a
program that already runs right ([`docs/minimal.md`](docs/minimal.md)).
Every other build writes the C it always wrote.

`zeno build --target <language>` writes the program in another language,
by fixed rules, as a file that runs with that language's own tools:
`js` (Node), `ts` (TypeScript), `html` (a page that runs it), `py`
(Python), `php`, `lua`, `ruby`, `dart`, `kotlin`, `go`, `java`, `cs`
(C#), `cpp` (C++), `rust`, and `wasm` (WebAssembly, built with
`rustc`)
([`docs/targets.md`](docs/targets.md)). The written program
prints what `zeno run` prints, overflows and errors included, and
`tools/targets.py` checks that for every example and book sample.

```sh
cargo run -- run examples/tour.zeno             # check a program, then run it
cargo run -- run --vm examples/tour.zeno        # the same, compiled to bytecode and run by the VM
cargo run -- bytecode examples/hello.zeno       # the bytecode, one instruction per line
cargo run -- check examples/tour.zeno           # only check it for mistakes
cargo run -- check --strict examples/tour.zeno  # every type must be known, as firmware needs
cargo run -- check --strict examples/styles.zeno  # passes with no types written: they are worked out
cargo run -- build examples/primes.zeno         # compile to primes.c, then to ./primes
cargo run -- build examples/shapes.zeno         # classes, enums, lists and match, compiled
cargo run -- build examples/sensor.zeno         # none, errors and functions as values, compiled
cargo run -- build --memory examples/memory.zeno  # the program reports its memory use as it ends
cargo run -- build --memory examples/buffer.zeno  # owned values: most blocks are freed without the collector
cargo run -- build --firmware examples/sensor.zeno  # errors as values, no setjmp, for firmware
cargo run -- build --no-collector --memory examples/buffer.zeno  # no garbage collector and no malloc: memory is a fixed pool
cargo run -- build --no-collector --memory examples/labels.zeno  # text that is kept, with no collector: every copy is freed
cargo run -- build --no-collector --memory examples/words.zeno   # split, map, filter and + with no collector: each statement frees what it made
cargo run -- build --no-collector --memory examples/scheduler.zeno  # lists of objects with no collector and no `owned`: each list frees its items
cargo run -- build --no-collector --memory examples/commands.zeno   # enum cases that carry values, and values that may be none, with no collector
cargo run -- build --no-collector --memory examples/handlers.zeno   # function values and kept errors with no collector: each has one owner
cargo run -- run examples/sentence.zeno         # the methods of text and lists: find, slice, sort, join, code
cargo run -- run examples/logbook.zeno bench.txt  # the keyboard, a file and the command line: type readings, then Ctrl-D
cargo run -- run examples/instruments.zeno      # math: atan2, log, exp, round, and `sin(30 deg)`
cargo run -- run examples/faults.zeno           # a dict: fault codes counted and looked up
cargo run -- run examples/station/main.zeno     # a program in four files: `import sensors`, `import drivers.uart`
cargo run -- join examples/station/main.zeno    # the same four files as the one program the compiler sees
cargo run -- run examples/thermostat/main.zeno  # a program that needs a package: `import filters.lowpass`
cargo run -- run examples/ring/main.zeno        # a module with `private` fields and functions
cargo run -- run examples/ports.zeno            # an interface: `class Uart is Port`, and a list of ports
cargo run -- run examples/channels.zeno         # a dict of lists and of objects, and `for key, value in ...`
cargo run -- fetch examples/thermostat          # get the packages a program needs from git (this one needs none fetched)
cargo run -- test examples/ring                 # run the tests written in the files of a folder: `test "name" { assert ... }`
cargo run -- test --build --firmware examples/ring  # the same tests, in the program a firmware build makes
cargo run -- new /tmp/blinker                   # make a new program: a `main.zeno`, a module with a test and a `zeno.package`
cargo run -- new --package /tmp/filters         # make a new package: a folder of modules for other programs to need
cargo run -- format --check examples            # is every example laid out the way `zeno format` lays programs out?
python3 editors/talk.py target/debug/zeno examples/shapes.zeno  # play the editor's part to `zeno lsp`, and print both sides
cargo run -- run examples/driver.zeno           # memory by hand and registers: `zeno run` checks every use of a pointer
cargo run -- build --memory examples/driver.zeno  # the same, compiled: the report counts what `allocate` gave and what was never freed
cargo run -- check --no-collector examples/scheduler.zeno  # check as that build would, without building
cargo run -- build --minimal examples/primes.zeno  # the least C that runs it: 100 lines, no checks, print as printf
cargo run -- build --minimal --no-collector examples/ring/main.zeno  # the same with no collector: the smallest C for a program with a list
cargo run -- build --llvm examples/shapes.zeno  # compile to LLVM IR (shapes.ll), then to ./shapes with clang
cargo run -- build --llvm --firmware examples/sensor.zeno  # the same through LLVM, with errors as values
cargo run -- build --llvm --no-collector --memory examples/buffer.zeno  # through LLVM with no collector: the same memory report as the C build
cargo run -- run examples/blink.zeno            # blink the LED of the simulated board (Ctrl-C stops it)
cargo run -- run examples/todo.zeno             # a program for a web page, on a simulated page
cargo build --release --target wasm32-unknown-unknown   # Zeno itself, built for a browser (once)
cargo run -- build --web examples/todo.zeno     # todo.web.html: the page and the program in one file. Open it in a browser
cargo run -- test examples/sketch.zeno          # a page that draws: its tests press the mouse on the simulated page
cargo run -- playground                         # playground.html: write Zeno programs and run them in a browser, with nothing installed
cargo run -- build --firmware --no-collector examples/signal.zeno  # pins and a clock, built for firmware
cargo run -- build --board boards/qemu-cortex-m3 examples/signal.zeno  # the same for an ARM Cortex-M3, in one step: signal.elf, which QEMU runs
cargo run -- build --board boards/lm3s6965evb examples/hello.zeno      # the same chip on its own UART, with no printf: hello.elf is 936 bytes of flash
cargo run -- build examples/packet.zeno         # u8 and u16 fields, stored in as many bytes in the C struct
cargo run -- build examples/generics.zeno       # a generic class: one C struct for each type it is used with
cargo run -- parse examples/tour.zeno           # the syntax tree the parser builds
cargo run -- tokens examples/hello.zeno         # the tokens the lexer finds
cargo test
```

`from board import led` and `from time import sleep` give a program pins and a
clock. On a computer the board is simulated and prints each pin as it changes.
A compiled program meets the board through four C functions, so a real one
can be put underneath ([`docs/board.md`](docs/board.md)).
`zeno build --board <folder>` builds a program for a board in one step,
with the board's own compiler, flags, start-up file and linker script,
which the folder names
([`docs/board-build.md`](docs/board-build.md)). A board whose header
brings four more functions prints, reads and stops by itself, with no
`printf` and no `exit` under the program.

`import web` gives a program a web page: `web.element("#count").text = "3"`,
`button.on("click", pressed)`. `zeno build --web` writes one file that a
browser opens, and `zeno run` runs the same program on a simulated page that
prints each change ([`docs/web.md`](docs/web.md)).

`u8`, `u16`, `u32`, `i8`, `i16` and `i32` are whole numbers with a size, and
`&`, `|`, `^`, `~`, `<<` and `>>` work on their bits. Arithmetic always gives
a full `int`, so nothing wraps by accident: `u8(n)` checks that `n` fits, and
`u8.wrap(n)` keeps the low eight bits on purpose
([`docs/sized-ints.md`](docs/sized-ints.md)).

`function first[T](items: list[T]) -> T` and `class Stack[T]` are generic:
written once, with `T` standing for a type. The call does not say what `T`
is. The checker works it out, and makes a copy of the function or class for
each type it is used with, so a `Stack[u8]` is as small and as fast as one
written for bytes by hand ([`docs/generics.md`](docs/generics.md)).

- **Why Zeno is the way it is:** [`docs/decisions.md`](docs/decisions.md) gives the reason for every design choice.
- **How Zeno was built:** [`HISTORY.md`](HISTORY.md) lists every commit, oldest first, under its release, with a plain line on what each one changed.
- **How each stage works:** [`docs/lexer.md`](docs/lexer.md), [`docs/parser.md`](docs/parser.md),
  [`docs/checker.md`](docs/checker.md), [`docs/interpreter.md`](docs/interpreter.md),
  [`docs/compiler.md`](docs/compiler.md), [`docs/memory.md`](docs/memory.md),
  [`docs/firmware.md`](docs/firmware.md), [`docs/ownership.md`](docs/ownership.md),
  [`docs/no-collector.md`](docs/no-collector.md), [`docs/pool.md`](docs/pool.md),
  [`docs/text.md`](docs/text.md), [`docs/temporaries.md`](docs/temporaries.md),
  [`docs/one-owner.md`](docs/one-owner.md), [`docs/enums-and-none.md`](docs/enums-and-none.md),
  [`docs/functions-and-errors.md`](docs/functions-and-errors.md),
  [`docs/unsafe.md`](docs/unsafe.md), [`docs/audit.md`](docs/audit.md),
  [`docs/library.md`](docs/library.md),
  [`docs/reading-and-writing.md`](docs/reading-and-writing.md),
  [`docs/folders.md`](docs/folders.md), [`docs/resource.md`](docs/resource.md),
  [`docs/files-in-pieces.md`](docs/files-in-pieces.md),
  [`docs/running-programs.md`](docs/running-programs.md),
  [`docs/python.md`](docs/python.md), [`docs/node.md`](docs/node.md),
  [`docs/java.md`](docs/java.md), [`docs/ruby.md`](docs/ruby.md),
  [`docs/php.md`](docs/php.md), [`docs/lua.md`](docs/lua.md), [`docs/csharp.md`](docs/csharp.md),
  [`docs/c.md`](docs/c.md), [`docs/inline.md`](docs/inline.md), [`docs/convert.md`](docs/convert.md), [`docs/convert-c.md`](docs/convert-c.md),
  [`docs/math.md`](docs/math.md), [`docs/dict.md`](docs/dict.md),
  [`docs/modules.md`](docs/modules.md), [`docs/using-packages.md`](docs/using-packages.md),
  [`docs/registry.md`](docs/registry.md), [`docs/packages.md`](docs/packages.md),
  [`docs/format.md`](docs/format.md), [`docs/lsp.md`](docs/lsp.md),
  [`docs/private.md`](docs/private.md), [`docs/interfaces.md`](docs/interfaces.md),
  [`docs/testing.md`](docs/testing.md), [`docs/new.md`](docs/new.md),
  [`docs/gaps.md`](docs/gaps.md), [`docs/version-0.1.md`](docs/version-0.1.md),
  [`docs/release.md`](docs/release.md), [`docs/found-by-the-book.md`](docs/found-by-the-book.md),
  [`docs/vm.md`](docs/vm.md),
  [`docs/llvm.md`](docs/llvm.md), [`docs/board.md`](docs/board.md),
  [`docs/board-build.md`](docs/board-build.md), [`docs/plain-c.md`](docs/plain-c.md),
  [`docs/web.md`](docs/web.md), [`docs/playground.md`](docs/playground.md),
  [`docs/json.md`](docs/json.md), [`docs/install.md`](docs/install.md),
  [`docs/sized-ints.md`](docs/sized-ints.md), [`docs/generics.md`](docs/generics.md).

## Roadmap

1. Lexer (done)
2. Parser and syntax tree (done)
3. Tree-walking interpreter, `zeno run` (done)
4. Type checker, `zeno check` (done)
5. Compile to C, `zeno build` (done: numbers, strings, functions, control flow, classes, lists, enums, `match`)
6. Compile the rest (done: errors with `try` / `raise`, `none`, functions as values)
7. Memory management (done: a mark-and-sweep garbage collector in the compiled program)
8. Errors as values, for firmware and MISRA builds (done: `zeno build --firmware`
   compiles `try` / `raise` to status returns and `if`, with no `setjmp`)
9. Owned values and looks, beside the collector (done: `owned` values have
   one owner and are freed where its block ends, and a parameter only
   looks at one unless it says `take`)
10. A firmware build with no collector (done: `zeno build --no-collector`
    gives every list and object one owner, and leaves the collector out
    of the C)
11. Bytecode VM (done: `zeno run --vm` compiles to bytecode and runs it on a
    stack machine; `zeno bytecode` shows the instructions)
12. LLVM backend (done: `zeno build --llvm` writes LLVM IR for the whole
    language the C backend compiles, and builds it with clang)
13. Extras:
    - The `board` and `time` modules (done: pins, `sleep` and `now`, on a
      simulated board, with a real one put underneath by a header)
    - Whole numbers with a size, and bit operators (done: `u8` to `i32`,
      checked and wrapping conversions, `& | ^ ~ << >>`, and small fields
      in compiled objects)
    - Generics (done: `function first[T]` and `class Stack[T]`, with the
      types worked out at each call, and one compiled copy for each set of
      types)
    - `--llvm` together with `--firmware` (done: errors as values in the
      IR, with no `setjmp`, and the same refusals as the C firmware build)
    - `--llvm` together with `--no-collector` and `--memory` (done: owned
      values are freed where their owner ends, text is given back at the
      end of its statement, and the memory report matches the C build's
      line for line)
    - A fixed memory pool for `--no-collector` builds (done: no `malloc`,
      `calloc` or `free` in the program; blocks in sizes that double,
      taken and given back in constant time, from one array whose size
      is chosen when the C is compiled)
    - Values owned by whoever receives them, in `--no-collector` builds
      (done. Text that is built can be kept, returned and
      stored in objects and lists, as a copy its keeper frees.
      A list made by `split`, `map`, `filter` or `+`, or written out,
      and an owned value that is only looked at, belong to the
      statement that made them, which frees them when it is over.
      Plain lists and classes have one owner without the word
      `owned`, and a list owns its items, so lists of objects and lists
      of lists work. An enum value that carries something has
      one owner, a list, object or enum value that may be `none` has
      one owner, and `match` looks at it without taking it. A function
      value has one owner, and so has an error, which can be kept and
      raised again)
    - Memory by hand in `unsafe { }` blocks (done: `pointer[T]`,
      `allocate` and `free`, and `Register[T]` at a fixed address, in
      every build and through both backends. `zeno run` checks every
      use of a pointer, and a compiled program checks none, as in C)
    - A pass over the whole language for "unambiguous and intuitive"
      (done: a plain parameter looks and `take` takes, the words of the
      language are never names, the cases after `case none` know the
      value is there, every function can read a `let` at the top of the
      file, a method can take out what its object holds, and a firmware
      build refuses what it cannot do instead of doing something else.
      See [`docs/audit.md`](docs/audit.md))
14. A standard library (in progress)
    - Text and lists (done: `find`, `slice`, `code` and `char` for text;
      `insert`, `pop(at)`, `reverse`, `sort`, `clear`, `find`, `slice`
      and `join` for lists, in every engine and every kind of build.
      See [`docs/library.md`](docs/library.md))
    - Reading and writing (done: `files.read`, `write`, `append`,
      `exists` and `remove`, with `FileError` as a value in every build;
      `text.lines()`; `input()`, which gives a line or `none`; and
      `program.arguments()`.
      See [`docs/reading-and-writing.md`](docs/reading-and-writing.md))
    - More of `math` (done: `tan`, `asin`, `acos`, `atan`, `atan2`,
      `exp`, `log`, `log2`, `log10`, `round` and `e`. `sin(30 deg)`
      takes an angle with its unit, `atan2` gives one in `rad`, and
      `.deg` and `.rad` change between them.
      See [`docs/math.md`](docs/math.md))
    - A dictionary (done: `["Ada": 36]`, `[:]` and the type
      `dict[str, int]`; reading, storing, `in`, `len`, a loop over the
      keys in the order they were put in, `get`, `pop`, `keys`,
      `values`, `clear` and `==`. A key that is not there is a
      `KeyError`. Compiled programs keep it in a hash table, and a
      build with no collector frees it with its owner.
      See [`docs/dict.md`](docs/dict.md))
15. Programs in several files (done: `import shapes` reads
    `shapes.zeno` from the program's folder and `import drivers.uart`
    reads `drivers/uart.zeno`; `shapes.area`, or `from shapes import
    area`. Each file runs once, before the file that imports it, and
    files that import each other are refused. An error names the file
    it is in. The files are joined into one program before it is
    checked, so every engine and every kind of build runs them the
    same. See [`docs/modules.md`](docs/modules.md))
16. A package manager (done: a folder with a `zeno.package` is a
    package. A program's own `zeno.package` says what it needs, in
    plain words: `needs filters in "../filters"` for a folder, or
    `needs units from "git address" at "v1.2.0"` for a repository.
    `import filters.lowpass` then reads `lowpass.zeno` of that
    package. `zeno fetch` gets the repositories with git into
    `packages/` and pins each commit in `zeno.lock`; running a
    program never uses the network. Since 0.5.2 there is a registry,
    so that `needs units at "v1.2.0"` finds a package by its name. It
    is a git repository,
    [zeno-src/packages](https://github.com/zeno-src/packages), with
    one small file for each package and no server. `zeno search`
    lists it, `zeno publish` checks a package and gives the address
    of the form that asks the registry to take it, and
    `zeno registry accept` is the registry's own side.
    See [`docs/using-packages.md`](docs/using-packages.md) and
    [`docs/registry.md`](docs/registry.md))
17. Editor support
    - A formatter (done: `zeno format file.zeno` lays a file out the
      one way Zeno lays programs out, with no options. It changes
      white space and nothing else: every token and every line break
      stays, so a program cannot come to mean something else, and the
      formatter checks that on every run. `--check` only looks.
      See [`docs/format.md`](docs/format.md))
    - A language server (done: `zeno lsp` speaks the Language Server
      Protocol, so VS Code, Neovim, Helix and others show the mistakes
      in a file as it is typed, from the text in the editor, and lay
      it out on save. They also jump to where a name was declared
      and show its type, from what the checker noted about each name.
      It is the `zeno` program itself, running the checker
      `zeno check` runs. Colours and setup for each editor
      are in [`editors/`](editors/README.md).
      See [`docs/lsp.md`](docs/lsp.md))
18. What real programs turn out to need (done). This was the finish
    line for the language: with this list done, Zeno is version 0.1.
    - `private` (done: `private` in front of a declaration at the top
      of a file, or of a field or a function of a class, keeps it to
      the file it is written in.
      See [`docs/private.md`](docs/private.md))
    - Interfaces (done: `interface Shape { ... }` lists functions, and
      `class Circle is Shape` says that the class has them, which is
      checked at the class. A `Circle` then goes wherever a `Shape`
      is wanted, so one function serves every class that is one. In
      a compiled program it is a small number in the object and a
      `switch`, with no pointer to a function.
      See [`docs/interfaces.md`](docs/interfaces.md))
    - A dict that holds any type of value, and a loop over its pairs
      (done: a value is anything a list can hold, such as a list, an
      object, a value of an interface or another dict, and never
      `none`, which is what `get` says for a key that is not there.
      `for key, value in ages` is given each key and what is under
      it, and over a list the first name is the position. With no
      collector a dict still holds plain values only.
      See [`docs/dict.md`](docs/dict.md))
    - `zeno test`: the tests of a Zeno program, written in Zeno
      (done: `test "what it checks" { ... }` at the top of a file,
      with `assert total == 6` and `assert raises ValueError { ... }`
      inside. A failed `assert` shows what was on each side. `zeno
      test` runs the tests of a file or of every file in a folder,
      `zeno run` goes past them, and a build leaves them out.
      `assert total == 6, "the total"` prints the text when it
      fails, and `zeno test --build` runs the same tests in the
      program a build makes, on a board too.
      See [`docs/testing.md`](docs/testing.md))
    - `zeno new`: a new program or package in one command (done:
      `zeno new blinker` makes a folder with a `main.zeno`, a module
      with a test and a `zeno.package`. It runs, its test passes,
      and it is laid out as `zeno format` lays programs out. `zeno
      new --package filters` makes a package. Nothing is written
      into a folder that is there.
      See [`docs/new.md`](docs/new.md))
    - The gaps programs have run into (done: `count += 1` on a `u8`
      is `count = u8(count + 1)`, the sum checked as it is stored.
      `f"{angle:.1f}"` is `30.5 deg`: a quantity takes the formats
      of a float and keeps its unit. `//=`, `%=` and `**=` are
      there, so every operator has an `=` form. Two mistakes were
      found and mended on the way: `x += f()` now reads `x` first
      in every build, and a call in the place of a `+=`, which ran
      twice, is refused.
      See [`docs/gaps.md`](docs/gaps.md))
    - A last pass over every decision marked provisional (done: each
      of them, about eighty, is now settled, a stated limit of 0.1,
      or changed, with the reason. They are all in chapter 11 of
      [`docs/decisions.md`](docs/decisions.md).
      See [`docs/version-0.1.md`](docs/version-0.1.md))
19. The Zeno Book (done: 38 chapters and seven appendices, from the
    first program to the inside of the compiler, with every sample run
    by the real compiler as the book is built. A printable PDF in
    [`book/dist/zeno-book.pdf`](book/dist/zeno-book.pdf) and pages for
    the web in `book/dist/html/`. See [`book/README.md`](book/README.md))
20. The first release (done: `zeno` 0.1.0, already built, for ten kinds
    of machine: Linux on x86-64, ARM64, 32-bit x86, ARMv7 and ARMv6,
    macOS on Apple silicon and Intel, Windows on x86-64, 32-bit x86
    and ARM64. A tag starts the build, and each build is run before
    it is released. Making it found that a compiled program rounded
    `c * 1.8 + 32` differently from `zeno run` on ARM64 and on 32-bit
    x86, which is mended.
    See [`docs/release.md`](docs/release.md))
21. What writing the book found (done: `zeno check --no-collector`
    says what the build would say. `none` is tested for with `is`
    only, and only where a value can be `none`. A field of an enum
    case is taken out with `match`. A `const` is written from what
    the compiler can work out. A parameter with a default comes last.
    `None`, `null`, `True` and `elif` each get a message that says
    what Zeno writes.
    See [`docs/found-by-the-book.md`](docs/found-by-the-book.md))
22. Windows as a whole (done: `zeno build --llvm` works with the clang
    that comes with Visual Studio. A compiled program waits, reads
    the clock and takes its arguments the Windows way. Every path
    Zeno makes is written with `/`. Every test of Zeno passes on
    Windows, tried there by a small workflow for one machine.
    See [`docs/release.md`](docs/release.md))
23. A program in a web page (done, and what version 0.2 is:
    `import web` gives a program the elements of a page, their text
    and classes, clicks, keys and timers. `zeno build --web` writes
    one `.html` file that a browser opens, with the program and Zeno
    itself in it, built for WebAssembly. Under `zeno run` and `zeno
    test` the page is a simulated one, so what a page does is tested
    on a desk.
    See [`docs/web.md`](docs/web.md))
24. Installers (done, in 0.2: a setup program for Windows and one
    line for macOS and Linux. Each picks the right `zeno`, puts it on
    the PATH, offers the C compiler and git that `zeno build` and
    `zeno fetch` hand work to, and can take it all off again.
    See [`docs/install.md`](docs/install.md))
25. A page that does more (done, in 0.2: a function given to `on` may
    ask for the `Event`, which says where the mouse was and what was
    clicked. The style of one element, drawing on a canvas, a timer
    that repeats, text kept between visits, and `fetch` and `post`
    for the network. `fire` presses a button in a test. `zeno run`
    reads the page that was written, so an id spelt wrong is found on
    a desk. A built page is 0.4 MB where it was 1.5 MB. And `zeno
    playground` writes a page in which programs are written and run
    with nothing installed.
    See [`docs/web.md`](docs/web.md) and
    [`docs/playground.md`](docs/playground.md))
26. What 0.1 left, taken up (done, in 0.2: `min` and `max` take
    quantities in one unit. A mistake in `let x: str = first([1])`
    is said at `x`. An editor says the type of a field. `zeno test
    --only "text"` chooses tests by name.
    See chapter 17 of [`docs/decisions.md`](docs/decisions.md))
27. JSON (done, in 0.2: `import json` has `json.parse(text)`, which
    gives the value the text describes, and `json.write(value)`,
    which gives the text of a value. A page reads what a server
    answers with it, and a program its settings.
    See [`docs/json.md`](docs/json.md))
28. Packages (done, in 0.2: each release has a `.deb` for apt, a
    package for pacman, a snap and a flatpak, and recipes for Homebrew
    and winget, all made from the release's own files and each
    installed, run and removed before a release. What is left is the
    place each manager looks in. Homebrew works, and for apt, the AUR,
    winget and the Snap Store everything is ready up to the account
    each needs.
    See [`docs/packages.md`](docs/packages.md) and
    [`docs/stores.md`](docs/stores.md))
29. A timer that can be stopped (done, in 0.2: `web.after` and
    `web.every` give a `Timer`, and `timer.stop()` calls off what
    it was waiting to do.
    See chapter 19 of [`docs/decisions.md`](docs/decisions.md))
30. `sort` with a function (done, in 0.3: `people.sort(age_of)` puts
    a list of anything in the order of what the function gives for
    each item, and items with the same stay as they were. In every
    engine.
    See [`docs/library.md`](docs/library.md) and chapter 21 of
    [`docs/decisions.md`](docs/decisions.md))
31. As fast as C (done, in 0.4: `zeno build --fast` leaves out the
    checks a correct program never fails, and the C compiler is given
    whole-number arithmetic as C's own. Benchmarks are counted in
    instructions as well as timed.
    See [`docs/memory-modes.md`](docs/memory-modes.md) and chapter 23
    of [`docs/decisions.md`](docs/decisions.md))
32. The program in another language (done, in 0.5: `zeno build
    --target` writes it in JavaScript, TypeScript, Python, PHP, Go,
    Java, C#, C++ or Rust, as a page, or as WebAssembly. Each file
    carries Zeno's rules and prints what `zeno run` prints, which
    `tools/targets.py` checks on 338 programs. Lua, Ruby, Dart and
    Kotlin came in 0.5.2.
    See [`docs/targets.md`](docs/targets.md) and chapter 24 of
    [`docs/decisions.md`](docs/decisions.md))
33. The least C (done, in 0.5.2: `zeno build --minimal` writes only
    the C the program needs, with no checks, for a program that is
    already correct. The other builds are
    unchanged, byte for byte. `tools/minimal.py` holds it to the
    checked build on 363 programs.
    See [`docs/minimal.md`](docs/minimal.md) and chapter 30 of
    [`docs/decisions.md`](docs/decisions.md))

34. A library written in Zeno (done, in 0.5.2: a module that comes
    with Zeno may have a part that is a file of Zeno, kept inside
    `zeno` and joined to a program as a file of the program is. Only
    the functions a program reaches are put into it, and a program
    that uses none is unchanged. The first ones: `math.hypot`, `sinh`,
    `cosh`, `tanh`, `gcd` and `clamp`, and the module `random`, with
    `between`, `number`, `pick` and `seed`. Each is written once, and
    is the same in every engine, every build and every target. See
    [`docs/library-in-zeno.md`](docs/library-in-zeno.md) and chapter
    33 of [`docs/decisions.md`](docs/decisions.md))
35. Ending a program, its environment, and the error stream (done, in
    0.6.0: `program.exit(code)` ends the program with a number for the
    shell, which no `try` stops; `program.environment(name)` gives
    `str or none`; `program.warn(text)` writes a line on the error
    stream. In every engine, every build and all fifteen target
    languages, and `tools/targets.py` now holds each target to the
    number a program ends with and to what it warned. See
    [`docs/ending-a-program.md`](docs/ending-a-program.md) and chapter
    34 of [`docs/decisions.md`](docs/decisions.md)) 36. A case of an
    enum can stand for a number (done, in 0.6.0: `Ack = 6` in the
    enum, for every case or for none; `int(Ack)` and `u8(Ack)` give
    the number, `Reply(6)` gives `Reply or none`. The checker writes
    two plain functions beside the enum, so it is the same in every
    engine, build and target. See
    [`docs/enum-numbers.md`](docs/enum-numbers.md) and chapter 35 of
    [`docs/decisions.md`](docs/decisions.md)) 37. Bytes from and to a
    file (done, in 0.6.0: `files.read_bytes(path)` gives a `list[u8]`,
    whatever the file holds, and `files.write_bytes(path, bytes)`
    writes one. In every engine, every build and all fifteen target
    languages. See [`docs/bytes.md`](docs/bytes.md) and chapter 36 of
    [`docs/decisions.md`](docs/decisions.md)) 38. A `list[u8]` is
    stored one byte to an item (done, in 0.6.0: in C and LLVM, every
    `list[u8]`, with nothing to write. An 8 MB file read and written
    back takes 13 MB where it took 64. Decision 36.2) 39. A list of a
    fixed length (done, in 0.6.0: `list[u8, 64]` always has 64 items.
    The count is checked before the program runs, `append`, `pop`,
    `insert` and `clear` are refused, and a function that never
    changes a list's length can be given one. `[0] * 64` repeats a
    list, for any list. In every engine, every build and all fifteen
    target languages. See [`docs/fixed-lists.md`](docs/fixed-lists.md)
    and decisions 36.3 and 36.4 in
    [`docs/decisions.md`](docs/decisions.md)) 40. `f32`, a float of 32
    bits (done, in 0.6.0: `let gain: f32 = 0.1`, sums done in 32 bits,
    as by a board's float unit, an f32 wherever a float is wanted, and
    an f32 beside a float refused with a message that says which to
    write. C writes `float`. In every engine, every build and all
    fifteen target languages. See [`docs/f32.md`](docs/f32.md) and
    chapter 41 of [`docs/decisions.md`](docs/decisions.md)) 41. `u64`,
    a whole number of 64 bits with no sign (done, in 0.6.0: `let h:
    u64 = 0xcbf29ce484222325`, sums that stay `u64` and are checked,
    `u64.wrap( )` to cut them to 64 bits as a hash does, and a `u64`
    beside an `int` value refused with a message that says which to
    write. `u8.max`, `int.min` and the ends of every whole type. C
    writes `uint64_t`. In every engine, every build and all fifteen
    target languages. See [`docs/u64.md`](docs/u64.md) and chapter 42
    of [`docs/decisions.md`](docs/decisions.md)) 42. Units made of
    units (done, in 0.6.0: `3.3 V * 20 mA` is `0.066 W`, `5 V / 2 mA`
    is `2500 ohm`, `5 W * 2 h` is `36000 J` and `1 / 2 ms` is `500
    Hz`, from a closed list of the sums firmware does; any other pair
    is refused with the list. In every engine, every build and all
    fifteen target languages. See
    [`docs/units-made.md`](docs/units-made.md) and chapter 43 of
    [`docs/decisions.md`](docs/decisions.md)) 43. Inputs on a board
    (done, in 0.6.0: `input_pin(2, pull_up)` gives an `InputPin`, read
    with `is_high()` and `is_low()`, and driving one is refused before
    the program runs. A board header gains a fifth function,
    `zeno_board_input`, which only a program that reads a pin calls.
    In every engine and every C and LLVM build. See
    [`docs/inputs.md`](docs/inputs.md) and chapter 45 of
    [`docs/decisions.md`](docs/decisions.md)) 44. A serial port on a
    board (done, in 0.6.0: `serial(1, 115200 baud)` gives a `Serial`
    that writes text and bytes and reads a byte at a time; the speed
    has a unit, `baud`. On a computer every port is the terminal. A
    board header gains four functions, which only a program with a
    port calls. In every engine and every C and LLVM build. See
    [`docs/serial.md`](docs/serial.md) and chapter 46 of
    [`docs/decisions.md`](docs/decisions.md)) 45. Reading a voltage on
    a board (done, in 0.6.0: `analog_pin(0)` gives an `AnalogPin`;
    `voltage()` is a quantity in `V`, so `level < 1200 mV` works and
    `level < 1200` is refused, and `count()` with `max_count` is the
    converter's own number. A board header says its bits and reference
    and gains two functions. In every engine and every C and LLVM
    build. See [`docs/analog.md`](docs/analog.md) and chapter 47 of
    [`docs/decisions.md`](docs/decisions.md))
46. A PWM signal on a board (done, in 0.7.0:
    `pwm_pin(9, 1 kHz)` gives a `PwmPin`, made with its frequency;
    `duty(0.25)` is the share of the time on, from 0.0 to 1.0, and
    `pulse(1500 us)` the time on in each period, as a servo wants. A
    share outside the range is an error, never clamped. A board
    header gains one function. In every engine and every C and LLVM
    build. See [`docs/pwm.md`](docs/pwm.md) and chapter 48 of
    [`docs/decisions.md`](docs/decisions.md))
47. A steady beat on a board (done, in 0.7.0:
    `ticker(10 ms)` gives a `Ticker`, whose ticks are 10 ms apart
    whatever the work between them takes. `wait()` waits for the next
    tick and `ticks()` asks without waiting; each gives how many ticks
    have come, so a loop knows when it ran over and a clock loses
    nothing. `restart()` makes it a timeout. It asks a board header
    for nothing new. In every engine and every C and LLVM build.
    See [`docs/ticker.md`](docs/ticker.md) and chapter 49 of
    [`docs/decisions.md`](docs/decisions.md))
48. Interrupts on a board (done, in 0.7.0:
    `button.interrupt(falling)` gives an `Interrupt`: the board counts
    the pin's edges, `wait()` waits for one and `edges()` asks without
    waiting, so no pulse is lost and the chip can sleep. The program's
    own code never runs inside an interrupt, so there is no shared
    variable to get wrong. `rising`, `falling` and `either` are values
    of the type `Edge`. A board header gains three functions. In every
    engine and every C and LLVM build.
    See [`docs/interrupts.md`](docs/interrupts.md) and chapter 50 of
    [`docs/decisions.md`](docs/decisions.md))
49. One step to a board (done, in 0.7.0:
    `zeno build --board boards/qemu-cortex-m3 blink.zeno` writes the
    program's C and has the board's own compiler make `blink.elf` of
    it. A board is a folder whose `zeno.board` names the compiler, the
    flags, the start-up files, the linker script and the command that
    runs the result. The board that comes with Zeno is an ARM
    Cortex-M3 that QEMU emulates, so every example is tried on a
    32-bit chip with 64 KB of RAM and no hardware. A build for a board
    has no collector.
    See [`docs/board-build.md`](docs/board-build.md) and chapter 51 of
    [`docs/decisions.md`](docs/decisions.md))
50. `print` and the end of a program through the board (done, in
    0.7.0: a board's header says
    `#define ZENO_BOARD_PRINTS` and brings four functions, for what is
    printed, what is said beside it, what is typed, and stopping. Then
    nothing of the program goes through the C library's streams or its
    `exit`. A second board, `boards/lm3s6965evb`, is the emulated
    Cortex-M3 on its own pin, timer and UART registers: `hello` takes
    936 bytes of flash there, and 9,472 on the board with a C library
    under it.
    See [`docs/board.md`](docs/board.md) and chapter 52 of
    [`docs/decisions.md`](docs/decisions.md))
51. A float into text, and text into a float, with no C library (done,
    in 0.7.0: the runtime works out the digits
    of a float, `{x:.2f}` and `float("2.5")` itself, with whole
    numbers and exactly, where it called `snprintf` and `strtod`. On a
    Cortex-M3 that is 4 KB of flash where the C library took 28:
    `primes` takes 7,884 bytes there, and took 25,360. Writing a float
    is five times faster on a computer. A build now prints
    `5.684341886080802e-14` for `2.0 ** -44.0` and `NaN` for a NaN
    with decimals, as `zeno run` always did.
    See chapter 53 of [`docs/decisions.md`](docs/decisions.md))
52. Checks in plain C, for a C compiler that is neither GCC nor Clang
    (done, in 0.7.0: the few things the C
    asked of GCC and Clang by name, the overflow of a sum, a
    difference and a product first among them, are each written a
    second time in plain C, and a compiler gets the one it can read.
    tcc builds every example, and each does what it does built by gcc.
    On a Cortex-M3 the plain C is as small as the other. No program
    has been built by IAR's, Keil's or a chip maker's compiler yet.
    See [`docs/plain-c.md`](docs/plain-c.md) and chapter 55 of
    [`docs/decisions.md`](docs/decisions.md))
53. `raises` in a function type (done, in 0.8.0:
    `function(str) -> int raises ValueError` is a function that may
    raise `ValueError`, and a call through it is checked as a call of
    one by its name. A firmware build and a build with no collector now
    take `texts.map(parse)` where `parse` raises, which they refused.
    A function that returns a raising function writes it in brackets.
    See [`docs/raises-in-types.md`](docs/raises-in-types.md) and
    chapter 56 of [`docs/decisions.md`](docs/decisions.md))
54. A function that is kept takes what it uses (done, in 0.8.0:
    a function written where a value goes, and returned, kept or given
    to a parameter that says `take`, takes each value with one owner
    that it uses and frees it when it is freed. So `compose(f, g)`
    builds with no collector. See
    [`docs/kept-functions.md`](docs/kept-functions.md) and 56.8 of
    [`docs/decisions.md`](docs/decisions.md))
55. A method used as a value (done, in 0.8.0: `let m =
    counter.bump` builds every way, as the function that calls the
    method. Passed along it looks at `counter`, and kept it takes it.
    See [`docs/method-values.md`](docs/method-values.md) and 56.9 of
    [`docs/decisions.md`](docs/decisions.md))
56. `take` in front of a place (done, in 0.8.0: `let task = take
    slot.task` gives what a field or an item holds and leaves `none`
    there, in every build, so a state machine with no collector can
    hand on what it holds. See [`docs/take.md`](docs/take.md) and 56.10
    of [`docs/decisions.md`](docs/decisions.md))
57. A value with one owner in a generic (done, in 0.8.0:
    `Stack[Frame]`, `Box([1, 2])` and `same(frame)` build with no
    collector, and `function same[T](x: take T) -> T` is right for a
    number and for a list. See
    [`docs/generic-owned.md`](docs/generic-owned.md) and 56.11 of
    [`docs/decisions.md`](docs/decisions.md))
58. A dict that owns its values (done, in 0.8.0: with no
    collector `dict[str, Channel]` and `dict[str, list[int]]` build,
    and the dict owns its values as a list owns its items: `d[key]`
    looks, a store frees the old value, `pop` hands it over. See
    [`docs/dict-owned.md`](docs/dict-owned.md) and 56.12 of
    [`docs/decisions.md`](docs/decisions.md))
59. A list of numbers that may be `none` with no collector (done,
    in 0.8.0: `list[float or none]` builds with
    `--no-collector`, and the list owns a small block of the pool for
    each number, as it owns its copy of each text; `none` takes none.
    See [`docs/none-list.md`](docs/none-list.md) and 56.14 of
    [`docs/decisions.md`](docs/decisions.md))
60. The size of the pool on the command line (done, in 0.8.0:
    `zeno build --no-collector --pool 4096` and `--text-room 2048`, and
    with `--memory` a write to a block after it was given back stops
    the program. See [`docs/pool-size.md`](docs/pool-size.md) and
    56.15 of [`docs/decisions.md`](docs/decisions.md))
61. A function inside a function, and a default that is worked out,
    in every build (done, in 0.8.0: a function with a name
    written inside a function and `function f(a: int, b: int = a * 2)`
    build through C and LLVM, with or without a collector. See
    [`docs/inner-functions.md`](docs/inner-functions.md) and 56.16 of
    [`docs/decisions.md`](docs/decisions.md))
62. Several values from one function (done, in 0.8.0: `function
    divide(a: int, b: int) -> (int, int)`, `return a // b, a % b`,
    `let q, r = divide(7, 2)`, and `low, high = high, low`, in every
    engine, every build and every target; several values are never
    one value, and a class keeps them together. See
    [`docs/several-values.md`](docs/several-values.md) and chapter 57
    of [`docs/decisions.md`](docs/decisions.md))
63. Cleaning up when a block ends (done, in 0.8.0: `owned class
    Selected is Resource` with a `function release(self)`, which runs
    when the owner of the object ends, on every way out of its block,
    in every engine, every build and every target. See
    [`docs/resource.md`](docs/resource.md) and 59.1 to 59.5 of
    [`docs/decisions.md`](docs/decisions.md))
64. A file in pieces (done, in 0.8.0: `files.read_from(path)`
    and `for line in reader`, `read_bytes(count)`, `files.write_to` and
    `files.append_to` with `write(text)`; the file closes when its
    owner ends. See [`docs/files-in-pieces.md`](docs/files-in-pieces.md)
    and 59.6 and 59.7 of [`docs/decisions.md`](docs/decisions.md))
65. Generic enums, bounds and generic interfaces (done, in 0.8.0:
    `enum Maybe[T]` and `Result[T, E]`, `function largest[T is
    Ordered]` and `[S is Shape]`, and `interface Source[T]` with `class
    Countdown is Source[int]`, a copy for each set of types, in every
    engine, every build and every target. A method with types of its
    own stays refused, and the message says what to write. See
    [`docs/generics.md`](docs/generics.md) and 58.1 to 58.8 of
    [`docs/decisions.md`](docs/decisions.md))
66. What interfaces left out (done, in 0.8.0: a default on a parameter
    of an interface's function, a function that takes out what its
    object owns standing for one of an interface, and `owned interface
    Shape`. See [`docs/interfaces.md`](docs/interfaces.md) and 58.20 to
    58.23 of [`docs/decisions.md`](docs/decisions.md))
67. Folders (done, in 0.8.0: `files.list(folder)`, sorted,
    `files.make_folder(path)` and `files.join(first, second)`. See
    [`docs/folders.md`](docs/folders.md) and 59.20 to 59.23 of
    [`docs/decisions.md`](docs/decisions.md))
68. A message on `assert`, and tests of a compiled program (done, in
    0.8.0: `assert c, "text"` prints the text above what was false, and
    `zeno test --build` runs the tests in the program C or LLVM makes,
    with `--firmware`, `--no-collector` or `--board`. See
    [`docs/testing.md`](docs/testing.md) and chapter 60 of
    [`docs/decisions.md`](docs/decisions.md))

What Zeno still does not have, in the order it will be built, with
the reason for each: [`docs/missing.md`](docs/missing.md).

## Layout

```
src/main.rs         the `zeno` command
src/token.rs        the kinds of token
src/lexer.rs        source text -> tokens
src/ast.rs          the shapes of the syntax tree
src/parser.rs       tokens -> syntax tree
src/printer.rs      syntax tree -> text, for `zeno parse`
src/modules.rs      the files of a program -> one syntax tree
src/package.rs      reads `zeno.package` and `zeno.lock`, and is `zeno fetch` and `zeno new`
src/registry.rs     the registry: a package by its name, `zeno search`, `zeno publish`, `zeno registry`
src/format.rs       `zeno format`: one layout for every file
src/json.rs         reads and writes JSON, for the language server and the `json` module
src/lsp.rs          `zeno lsp`: the language server an editor starts
src/convert*.rs     `zeno convert`: a file of Python or C written again as Zeno
editors/            colours and setup for VS Code, Vim, Neovim and Helix
src/checker.rs      finds type mistakes before the program runs
src/value.rs        what a value is while a program runs
src/ops.rs          what each operator and step does, for both engines
src/interpreter.rs  runs a syntax tree
src/bytecode.rs     the instructions of the VM, and a printer for them
src/emit_bytecode.rs  syntax tree -> bytecode
src/vm.rs           runs bytecode, for `zeno run --vm`
src/builtins.rs     print, len, methods, modules, formatting
src/board.rs        the simulated board that `zeno run` uses
src/board_build.rs  reads `zeno.board`, and says what a board's compiler is asked (`zeno build --board`)
src/page.rs         the page of the `web` module: the browser's, or a simulated one
src/web_host.rs     where `main` would be, when Zeno is built for a browser
src/web.js          starts a program in a page, and does what it asks of the page
src/html.rs         reads a page of HTML, so that the simulated page has its elements
src/deflate.rs      packs `zeno.wasm` into a built page, the way a `.zip` is packed
src/playground.html the playground, before `zeno playground` writes Zeno into it
src/ints.rs         the sized whole numbers: what a `u8` holds
src/units.rs        the units a number can carry
src/emit_c.rs       syntax tree -> C source, for `zeno build`
src/emit_llvm.rs    syntax tree -> LLVM IR, for `zeno build --llvm`
src/runtime.c       the C library every compiled program carries
src/minimal.rs      puts the C of `zeno build --minimal` together: only what the program uses
src/lower.rs        syntax tree -> a plain program any language can say, for `zeno build --target`
src/emit_js.rs      that program -> JavaScript and TypeScript; emit_py, emit_php, emit_lua, emit_ruby, emit_dart, emit_kotlin, emit_go, emit_java, emit_cs, emit_cpp and emit_rust are the others
src/runtime.js      Zeno's rules in JavaScript, put into each written program; runtime.ts, .py, .php, .lua, .rb, .dart, .kt, .go, .java, .cs, .cpp and .rs are the others
tools/targets.py    builds every example for every target and compares it with `zeno run`
tools/minimal.py    builds every example with and without `--minimal` and compares the two
tools/board.py      builds every example for an emulated ARM Cortex-M3 and for the computer and compares the two
tools/plain.py      builds every example as it is, as plain C, and with a C compiler that is neither GCC nor Clang, and compares the three
boards/             a folder for each board `zeno build --board` can build for: today two, both the Cortex-M3 that QEMU emulates, one on a C library and one on its own registers
src/error.rs        the error type every stage shares
docs/               the design decisions, and how each stage works
examples/           sample .zeno programs
tools/release-check.sh        runs a built `zeno` before it is released
tools/install.sh              installs Zeno on macOS and Linux
tools/try-install.sh          tries install.sh, with no release to download
tools/installer/              the setup program for Windows (Inno Setup)
tools/package.sh              makes the packages: .deb, snap, flatpak, pacman, Homebrew, winget
tools/try-packages.sh         installs each package, runs it, and removes it
tools/keep-recipes.sh         puts the recipes of a release into the repository
Formula/                      the Homebrew formula, written at each release
packaging/                    the PKGBUILD and the winget files, written at each release
.github/workflows/release.yml builds and releases `zeno` when a tag is pushed
.github/workflows/trial.yml   tries Zeno on one kind of machine, and releases nothing
.github/workflows/installer.yml  tries the installers on one kind of machine
.github/workflows/add-installers.yml  makes the installers for a version already released
.github/workflows/packages.yml  makes and tries the packages of a version already released
.cargo/config.toml  how `zeno` is linked for each kind of machine
```

## Help and contact

| For | Write to |
|---|---|
| Help with installing or using Zeno | help@zeno-lang.org |
| A bug in Zeno | the [issues](https://github.com/zeno-src/zeno-lang/issues) on GitHub, where others can see it and its answer. With no GitHub account, issues@zeno-lang.org |
| A security problem | security@zeno-lang.org, in private. [`SECURITY.md`](SECURITY.md) says what to send |
| Anything else: an idea, the website, the book | contact@zeno-lang.org |

## Licence

Zeno is under the MIT licence: anyone may use it, change it and pass it
on, as long as the notice in [`LICENSE`](LICENSE) goes with it. Every
archive, installer and package of a release carries that file, from
0.2.0 on.
