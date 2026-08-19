# Third-party notices

xless was designed by studying [`jless`](https://github.com/PaulJuliusMartinez/jless)
(a command-line JSON viewer) and, for several modules, directly porting or
closely adapting its source. `jless` is MIT licensed:

```
Copyright (c) 2021 Paul Julius Martinez

Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

xless is also MIT licensed (see `LICENSE`), so this reuse is permitted;
this notice exists to satisfy the "include the above copyright notice"
condition and to be transparent about provenance. Modules that are a
close/near-verbatim port of a specific jless source file say so in their
own doc comment (e.g. `src/terminal.rs`, `src/input.rs`); modules that
port jless's *algorithms* onto a new XML-shaped data model (e.g.
`src/flatxml.rs`'s traversal methods, `src/viewer.rs`'s movement/scrolling
logic) say so as well. See `docs/ARCHITECTURE.md` for the full account of
what was studied and how each part of jless's design was reused, adapted,
or deliberately diverged from for xless's requirements (large-file
performance, standalone editing) — see docs/ARCHITECTURE.md §§1, 6, 8, 9.
