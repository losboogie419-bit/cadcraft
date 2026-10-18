# FA CAD: status and working rules

This fork of CADCraft is a fire alarm drafting tool. **Read this before planning or testing.**

## How we work (owner, Claude, Grok bots)
- **Only Claude pushes code to `main`.** Other bots don't push to `main`.
- **Talk in GitHub Issues**, using the forms: Test report, Bug, Idea / plan. Claude reads them when the owner asks it to check GitHub, and replies on each issue with what it did.
- **Code suggestions from another bot** go in a **pull request** from its own branch. Nothing is merged unless the owner or Claude accepts it.
- **Minor problems** (a wrong DIP switch, a label in the wrong spot) are marked *Minor*. The owner may fix them by hand, and Claude batches them into the next build instead of rebuilding for each one.
- **The repo is public.** Never put job drawings, addresses, panel names or other job details in code, issues or tests. Tests use made-up sample shapes.
- **Every push to `main` builds the Windows installer** (about 15–30 min). It lands on the Releases page as **Latest Windows build**. Commits that only touch docs say `[skip ci]`.

## The owner's rules (apply to everything)
- Black background, amber/yellow (#FFB000) accent.
- Flag anything that doesn't match a rule: NEEDS_REVIEW. Never silently fix it, and never invent a number.
- Generous undo, and a way to clear or reset.
- Lists for pasting into VeriFire Tools are gap-filled: every ID in order, with a blank row for each unused number.
- Labels and stamps are 40 characters at most.
- The owner's own panel program (node exports) is the source of truth over run sheets.
- Strobe power supplies: DIP 5 ON.
- XP6-C card address = the first circuit address label above the card.

## Done (on `main`)
| Feature | Command / menu | Notes |
|---|---|---|
| Black/amber theme | View → Interface Theme → Dark | default |
| Windows installer build | GitHub Actions → Releases | unsigned (SmartScreen: More info → Run anyway) |
| Import PDF | `PDFIMPORT`, File → Import PDF…, File → Open | vectors, circles/arcs, real text; SHX labels rebuilt as text when clean, kept as strokes (layer PDF_SHX_GLYPHS) with a hidden searchable copy otherwise |
| XP6-C addresses, T11–T15 jumpers, Class A, HPFF12 DIP 5 | `FAPANEL`, Tools → Fire Alarm → Read Panel Build | tag blocks FA_XP6C / FA_HPFF12 on layer FA_READ; the owner's edits win on rerun |
| Trace a wire | `FATRACE` (select a wire first) | screws end a trace; crossings don't join |
| Loose wire ends | `WIRECHECK` | rings on FA_CHECK; still flags drawing details (about 120 on a panel sheet), needs tuning from test reports |
| Labels from a point list | `FALABELS` (pick a CSV with `from,to` columns) | never adds labels; NOT_FOUND and over-40 are flagged |
| Revision stamp + colour diff | `FAREV` (pick the previous revision's DXF) | stamp max 40 chars; added green, changed amber, areas ringed on FA_REV |
| Searchable PDF export | `EXPORTPDF` / PLOT | invisible text layer for stroke-font text |

## Next / open
- Tune WIRECHECK using real-vs-noise examples from test reports.
- Device library with tagged terminals (needs real cut sheets; every entry starts `verified:false`).
- Rule checks that need terminal roles (relay common on SLC, jumper onto a card).
- Gap-filled VeriFire export of card/circuit addresses.

## Testing
Use `FA_CAD_Test_Script` v2 (kept by the owner, not in the repo). File results with the **Test report** issue form.
