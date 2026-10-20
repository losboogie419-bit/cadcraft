# FA CAD: status and working rules

This fork of CADCraft is a fire alarm drafting tool. **Read this before planning or testing.**

## How we work (owner, Claude, Grok bots)
- **Only Claude pushes code to `main`.** Other bots don't push to `main`.
- **Talk in GitHub Issues**, using the forms: Test report, Bug, Idea / plan. Claude reads them when the owner asks it to check GitHub, and replies on each issue with what it did.
- **Code suggestions from another bot** go in a **pull request** from its own branch. Nothing is merged unless the owner or Claude accepts it.
- **Minor problems** (a wrong DIP switch, a label in the wrong spot) are marked *Minor*. The owner may fix them by hand, and Claude batches them into the next build instead of rebuilding for each one.
- **The repo is public.** Never put job drawings, addresses, panel names or other job details in code, issues or tests. Tests use made-up sample shapes.
- **Every push to `main` builds the Windows installer** (about 15–30 min). The newest one is always at **https://github.com/losboogie419-bit/cadcraft/raw/downloads/cadcraft-windows-x64.msi** (public, no login). Commits that only touch docs say `[skip ci]`.

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
| Loose wire ends | `WIRECHECK` | rings on FA_CHECK only where a wire stops near a terminal and misses it (about 8–10 on a panel sheet, 3 on a floor plan); arrowheads, labels and border lines are normal ends; `all: true` checks every end |
| Point list (#6, first build) | Tools → Fire Alarm → Point List: Drawing Inventory, Job Setup, Load Panel Report / Run Sheet, Find Devices and Rooms, Assign Room to Selected, Accept Selected, Accept All Greens on This Page, Show Only What Needs Me, Export | job JSON per job; devices from mapped blocks (DWG/DXF) or address text (PDF, split pieces merged); room by outline, else nearest room number (close call = amber); markers on FA-POINTLIST only; Excel export gap-filled 1–159 per node+loop with Node/Loop/Type/Number, duplicates on the full address; Workbench JSON |
| Point list round 2 (#13 bugs) | same menu, plus **Export Run Sheet...** | addresses kept as drawn (key for matching is separate); title-block Sheet # and Drawing No; Type SMOKE/MODULE; Function and Interface Equipment read from text near each device (job-editable word list; unsure = blank + comment); highlight = the address's own strokes/text recoloured on FA-POINTLIST (no rings); room names never a panel tag; Comments = owner note + `APP:` concerns, owner notes kept when re-exporting over an earlier file |
| Run sheet export | `FARUNSHEET` | owner's format: DEVICE ADDRESS (N05-L1-D-01) / DEVICE TYPE / FUNCTION / ROOM # (NAME (NUMBER)) / INTERFACE EQUIPMENT / COMMENTS; tab per loop, gap-filled 1–159 D then M; "No address" tab |
| Labels from a point list | `FALABELS` (pick a CSV with `from,to` columns) | never adds labels; NOT_FOUND and over-40 are flagged |
| Revision stamp + colour diff | `FAREV` (pick the previous revision's DXF) | stamp max 40 chars; added green, changed amber, areas ringed on FA_REV |
| Searchable PDF export | `EXPORTPDF` / PLOT | invisible text layer for stroke-font text |

## Next / open (owner's order)
1. ~~Bug build from #13~~ (done). WIRECHECK now only runs on sheets with terminal strips. PDF fills painted with B/b (fill+stroke) now come in filled (#3).
2. ~~Run sheet export, owner's format (replaces #13 item 14)~~ (done): DEVICE ADDRESS (`N1-L1-D-01` style) | DEVICE TYPE (Smoke / Module only) | FUNCTION (waterflow, fire/smoke damper, duct detector, area detector, heat detector, … only when the drawing shows it) | ROOM # (name and number) | INTERFACE EQUIPMENT | COMMENTS (app concerns + owner notes; owner notes are never overwritten). No DGP, loop, NAC zone, device input/output, key switch, install or P2P columns. Full information, no label shortening (labels are made later in the owner's separate abbreviations app). The owner's run sheets are a format reference only: never committed, never used as data or as an answer key.
3. ~~In-app review (#13 items 8–13, 24–29)~~ (done): **Point List → Review Panel** (docked under the drawing, can float): every device with ✓ accepted, address, type, function, equipment, room, sheet, status, comments; click a row to zoom and light up the device and its room; click a cell to edit (owner values win and are kept); ⌖ then click a room on the drawing to re-assign; status filter, "only rows with a concern", search; per-sheet counts with Accept greens; Show missed flashes unread address text; Show only what needs me is a toggle. Hovering a device on the drawing lights up its room. Rooms come from wall regions first (walls flood-filled from each room number), then the nearest number; an open area with several numbers is amber. Callout boxes ("58 - STATUS" under an equipment tag) fill a module's function and equipment. Not done: pop-out to a second OS window, riser counts, circuits-off filter.
4. **Record keeping (#13 items 15–18).**
5. **Field change markups** (moved / swapped / dual address / removed, into Comments and an as-built list) and **search the whole drawing set** (index every address, room and equipment tag across sheets).
- **On hold:** point-to-point testing mode (tablet limits; maybe its own app).
- **Later:** riser from the point list; revision compare; learn from fixes.
- **Held until they can be made job-generic:** program compare (#8), zone/CBE overlay, damper/fan pairing check, address check per loop, NAC circuit check.
- Device library with tagged terminals (#10, needs real cut sheets; every entry starts `verified:false`).

## Testing
Use `FA_CAD_Test_Script` v2 (kept by the owner, not in the repo). File results with the **Test report** issue form.
