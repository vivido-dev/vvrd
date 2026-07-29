# Office test fixtures

Minimal hand-written DOCX, PPTX, ODT, and ODP packages used by `src/office.rs` tests. Each one
contains two short paragraphs ("vvrd office fixture" / "second paragraph") and only the parts the
format requires, so the tracked binaries stay in the 1–5 KB range. Real LibreOffice opens all four,
which keeps them usable for manual `VVRD_OFFICE_BACKEND=soffice` checks.

They exist to prove the conversion path produces a fixed-layout PDF MuPDF can page through — not to
measure fidelity. Rendering quality is only observable against real documents.

Regenerate with:

```sh
python3 tests/fixtures/make_fixtures.py tests/fixtures
```
