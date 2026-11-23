# The Zeno Book

The whole of Zeno 0.8, and of 0.9 as far as it has come, from a first
line to the inside of the compiler: 42 chapters in five parts, and
seven appendices.

- `chapters/` holds the source, one LaTeX file for each chapter or
  appendix. `zeno-book.tex` puts them together, and `zeno-book.sty`
  sets how the printed book looks.
- `dist/zeno-book.pdf` is the printable book.
- `dist/html/` is the book for the web, one page for each chapter.
  It needs no server: open `index.html`.

## Every sample is run

The code in the book is not copied in by hand. Each `zeno` block is a
program, and the build runs it with the `zeno` command and writes what
it printed underneath, so the book cannot show output the compiler
does not give. A sample marked `fails=yes` must fail, and its message
is shown the same way. If any sample does something other than what
the book says, the build stops and names it.

## Building it

You need:

- `zeno` on the path (`cargo build --release` in the repository, then
  `target/release/zeno`), a C compiler and `clang` for the chapters
  that compile, and `zeno.wasm` for the web chapter
  (`cargo build --release --target wasm32-unknown-unknown`)
- Python 3
- the languages whose libraries the two chapters on other languages
  use: Python 3, Node 22, Java, Ruby 3, PHP 8, Lua and the .NET SDK,
  and Go and `rustc` for the libraries the chapter on C builds
- XeLaTeX and `latexmk` (TeX Live: `texlive-xetex`,
  `texlive-latex-extra`, `latexmk`)
- the fonts TeX Gyre Pagella and Heros (`tex-gyre`) and DejaVu Sans
  Mono (`fonts-dejavu`)
- pandoc 3 or later, for the HTML

Then, from the top of the repository:

```
python3 book/tools/build.py              # samples, PDF and HTML
python3 book/tools/build.py --samples    # only run the samples
python3 book/tools/build.py --pdf        # samples and PDF
python3 book/tools/build.py --html       # samples and HTML
python3 book/tools/build.py --chapter=12 # only the samples of the chapter whose file starts with 12
```

Work files go in `book/build/`, which git does not keep.

`images/` holds pictures of pages that `zeno build --web` made, as a
browser shows them. `node book/tools/pictures.js` makes them again,
with Playwright, when a page changes.

## Writing in it

`tools/build.py` describes every option of the three sample
environments, `zeno`, `zenofrag` and `session`, at its top. Two rules
keep the PDF and the HTML apart: a label is `ch-` or `app-` and a name
no other chapter has, because the HTML page is named after it; and
code inside a table cell's macro argument is `\texttt`, not `\verb`.
