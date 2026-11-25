# flurry icon spec

The rules behind flurry's icons (`ui/icons.rs`), written so that anyone (person or model) drawing a new
icon produces one that is indistinguishable in style from the existing set. Every rule is mandatory.
"MUST" and "NEVER" are absolute; there is no "roughly". Where two layouts both satisfy every rule,
section 10 decides between them, so the same brief always yields the same icon.

All numbers are grid units unless they say px. Coordinates are (x, y), y pointing down.
Constants: √2 = 1.4142, 1/√2 = 0.7071, tan 22.5° = √2 − 1 = 0.4142. Write non-integer coordinates
with exactly 4 decimals.

## 1. Grid and canvas

1. The canvas is 16×16 units: `viewBox="0 0 16 16"`.
2. Content MUST lie inside the box from (1, 1) to (15, 15). The 1-unit margin stays empty. The only
   things allowed outside it are listed in section 11; NEVER add another.
3. Every horizontal and every vertical edge MUST sit on a whole unit (x or y an integer).
4. 45° edges sit wherever the exact 2s of section 2 put them, so their coordinates may be irrational
   (√2 multiples). That is expected, not a flaw.
5. Icons are drawn at multiples of 16 px (16, 32, 48, 64). Design for 16 px: if a detail only reads
   at 64 px, it is wrong.

## 2. Measurements (the core of the style)

6. Every line, band, wall, ray, dot and bar MUST be exactly 2 thick. Thickness is measured
   perpendicular to the edge: a 45° bar is 2 across its faces (2.8284 horizontally), not 2
   horizontally.
7. NOTHING may be narrower than 2: no part, no fragment left by a cut, no hole, no slit, no wall, no
   neck inside a shape. Sharp tips at corners (section 4) are the only narrow points allowed.
8. Every separate part MUST be exactly 2 from its nearest neighbor, and never closer than 2 to any
   part. 2.05 is as wrong as 1.95; 3 is as wrong as 1.
9. Every cutout (hole) MUST have exactly 2 of solid between it and its nearest neighbor (another hole
   or the icon's outside), and at least 2 everywhere else.
10. Open notches in an outline (like the space between a tail and a stem) are not gaps; rules 8 and 9
    apply to separate parts and to holes.
11. Exact means within 0.0001 for geometry you compute; on a render at 16 px per unit, within 1.5 px
    for widths and gaps and 1 px for bounds.

## 3. One style

12. Objects (things with a silhouette: phone, battery, lock, keyboard, bell, mic, speaker, mouse,
    plug, folder, ...) MUST be solid filled silhouettes. Details are cut OUT of the solid as holes or
    slits. NEVER draw an object as an outline. A part inside a hole (the speaker's cone in its woofer
    ring) is allowed only when every 2 holds; otherwise fill solid (battery-full has no level block).
13. Glyphs with no silhouette (signal arcs, sound waves, refresh arrows, slashes, an X, a shackle, a
    headphone band, a rune's tails, a network tree's links) are 2-thick lines and bands, joined to or
    spaced from the rest by exact 2s.
14. If a hollow region inside a glyph (an arrowhead, a loop) cannot be at least 2 wide with 2-thick
    walls, make that region solid. NEVER thin the walls to make the hole fit.
15. Prefer the fewest details that still read. One defining cutout beats several: the phone is a slab
    with one 4×2 earpiece slot, no button; the lock body has one 2×4 keyhole slot.
16. Flat, one layer, one color. NEVER: perspective, 3D, shading, gradients, shadows, highlights, text,
    letters, numbers, outlines around solids, decorative lines.

## 4. Geometry

17. Only three directions: horizontal, vertical, 45°. NEVER any other slope (frozen exceptions in
    section 11).
18. NEVER curves, arcs, circles or rounded corners. A circle becomes a regular octagon (rule 26). A
    rounded corner becomes a square corner, or a 45° chamfer when the shape needs the softening.
19. Polygon corners MUST be 45°, 90° or 135° (or their reflex 225°, 270°, 315°). 45° tips are allowed
    (speaker cone, bell flare, Wi-Fi band ends); nothing sharper.
20. Line ends MUST be square caps: cut flat, perpendicular to the line, extending 1 past the centerline
    end. NEVER angled or rounded ends.
21. Bent lines MUST use miter joins (sharp outer corners), at 90° or 135° only. A join sharper than
    90° MUST be built as a filled polygon instead.
22. Symmetry: when the object is symmetric, the icon MUST be exactly symmetric about x = 8 (and about
    y = 8 if the object is). Asymmetric only when the object itself is (play, volume, refresh,
    bluetooth), and then still centered on x = 8 unless rule 25 shifts it.

## 5. Size classes

23. Classify by silhouette, then use exactly this size and placement:

| Class | Size (w×h) | Placement | Existing |
|---|---|---|---|
| Upright object | 10×14 | x 3–13, y 1–15 | battery-full, phone, mouse, speaker, pause, play (x 4–14) |
| Wide object | 14×10 | x 1–15, y 3–13 | keyboard |
| Tall glyph or object | 12×14 | x 2–14, y 1–15 | lock, mic, refresh |
| Open, radial or busy shape | 14×14 | x 1–15, y 1–15 | brightness, headphones, wired, bell-off (the slash cut can trim an "off" icon's bounds: mic-off is 13×14) |
| Narrow glyph | its natural width × 14 | centered on x = 8 | bluetooth (9.4×14) |
| Fan | up to 16 | section 6 | Wi-Fi, volume |

24. Within its class an icon MUST be as large as the class allows while keeping every 2.
25. Optical centering: shift whole units only when the mass is clearly to one side. Play's triangle
    sits at x 4–14 (1 right of the upright box) because its mass is on the left. NEVER shift by
    fractions.

## 6. Primitives (build everything from these)

All formulas give the exact outline; export as filled polygons (section 9).

- Rectangle: whole-unit x, y, w, h.
- Cutout: a rectangle (or a 45°-edged polygon) subtracted from a solid; at least 2×2; exactly 2 of
  solid to its nearest neighbor.
- Line: centerline from P to Q, 2 thick, square caps. With unit direction d and unit normal n, the
  outline is P − d + n, Q + d + n, Q + d − n, P − d − n. For a 45° line, d and n are (±0.7071, ±0.7071).
- Dot: a 2×2 square. A dot that sits on a diagonal is a diamond (2×2 square turned 45°): center c,
  corners c ± (1.4142, 0) and c ± (0, 1.4142).
26. Octagon (replaces every circle): center c, flat distance r; corners at c + (±r·0.4142, ±r) and
    c + (±r, ±r·0.4142). r MUST be a whole number so the flats land on whole units.
27. Fan band (concentric arcs): the part of a regular octagon ring between the two 45° rays either
    side of its axis. Apex (ax, ay), opening up, inner flat distance r0, outer r1:
    - outer, left to right: (ax − 0.7071·r1, ay − 0.7071·r1), (ax − 0.4142·r1, ay − r1),
      (ax + 0.4142·r1, ay − r1), (ax + 0.7071·r1, ay − 0.7071·r1)
    - then inner, right to left, the same four with r0
    - r0 = 0 makes a wedge: the inner edge is the apex alone
    - opening right instead: map each up-frame offset (u, v) to (ax − v, ay + u)
    The ends lie on the 45° rays, so a band is exactly 2 thick across its flat and its 45° faces.
    Bands MUST have r1 − r0 = 2 and a 2 gap to the next band.

## 7. Families and states

28. Levels (signal, volume, brightness, battery-like meters): every level shows ALL parts; unlit
    parts are the same shape at opacity 0.35. Light from the source outward:
    - Wi-Fi: 0 all dim; 1 dot; 2 dot + band 1; 3 all lit; connecting = dot and band 2 lit, band 1 dim
    - volume: low speaker only; medium + wave 1; high + wave 2; over = high with wave 2 lengthened
      0.5 in x and y at each end; muted = speaker + X, no waves
    - brightness: low core only; medium core + 4 straight rays; high all 8 rays
    - thresholds (flurry's): volume muted at 0 or mute, low up to 33%, medium up to 66%, high up to
      100%, over above; brightness low up to 33%, medium up to 66%, high above; Wi-Fi 0 to 3 from the
      network's signal level
29. Off / disabled: the base icon, minus a gap, plus a slash drawn on top. Every off icon in the set
    MUST use the identical slash and gap:
    - slash (filled): (2.0711, 2.4142) (3.4853, 1) (15, 12.5147) (13.5858, 13.9289)
    - gap (subtracted from the base): (−0.7574, 2.4142) (3.4853, −1.8284) (17.8284, 12.5147)
      (13.5858, 16.7574)
    The slash runs top-left to bottom-right, 2 thick, square caps whose outer corners touch the box
    (y = 1 at the top, x = 15 at the bottom). It sits 5√2 − 6 = 1.0711 (in x − y) up-right of the
    center diagonal: the one position where Wi-Fi's leftovers are all exactly 2 thick. The gap is the
    slash grown by 2 on every side.
30. After the cut, every leftover fragment of the base MUST still be at least 2 thick. If one is not,
    enlarge or reshape the base (the bell got 14 wide for this); NEVER move the slash for one icon,
    NEVER accept a sliver.
31. Muted / cancelled: an X of two 2-thick 45° lines, (9.4142, 6)→(13.4142, 10) and
    (9.4142, 10)→(13.4142, 6), 2 in front of whatever it cancels.
32. Connected / active badge: diamond dots exactly 2 from the glyph (bluetooth-on).
33. Variants reuse the base parts unchanged. NEVER redraw or resize a part between variants.
34. Names: `thing`, `thing-level` (`wifi-0`..`wifi-3`, `volume-low|medium|high|over`,
    `brightness-low|medium|high`), `thing-off`, `thing-muted`, `thing-on`, `thing-connecting`.

## 8. Reference catalog (the existing icons, exact)

R(x, y, w, h) = filled rectangle, H(x, y, w, h) = cutout, polygons are vertex lists.

- battery-full: R(6,1,4,2) R(3,3,10,12). Solid: a level block inside a frame would leave 1-unit gaps.
- phone: R(3,1,10,14) H(6,3,4,2)
- mouse: R(3,1,10,14) H(7,1,2,5) H(3,6,10,2) (two buttons over a body)
- speaker: R(3,1,10,14) H(7,3,2,2) H(5,7,6,6), then R(7,9,2,2) filled inside the woofer hole
- pause: R(3,1,4,14) R(9,1,4,14)
- play: (4,1) (14,8) (4,15)
- lock: R(4,1,8,2) R(4,1,2,6) R(10,1,2,6) R(2,7,12,8) H(7,9,2,4) (a centered keyhole slot, 2 under the
  shackle opening and 2 above the bottom)
- keyboard: R(1,3,14,10) H(3,5,2,2) H(7,5,2,2) H(11,5,2,2) H(5,9,6,2)
- mic: R(6,1,4,6) R(2,5,2,4) R(12,5,2,4) R(2,9,12,2) R(7,11,2,2) R(4,13,8,2)
- bell: R(7,1,2,2), polygon (4,3) (12,3) (13,9) (15,11) (1,11) (3,9), R(6,13,4,2)
- headphones: (1,15) (1,1) (15,1) (15,15) (11,15) (11,9) (13,7) (13,3) (3,3) (3,7) (5,9) (5,15)
- wired: R(5,1,6,4) R(7,5,2,2) R(3,7,10,2) R(3,9,2,2) R(11,9,2,2) R(1,11,6,4) R(9,11,6,4)
- brightness:
  - core: (6.6823,5) (9.3177,5) (11,6.6823) (11,9.3177) (9.3177,11) (6.6823,11) (5,9.3177) (5,6.6823)
    (flats at 3 from center, 45° faces at 3.0531 so the diagonal rays are exactly 2 from it)
  - straight rays: R(7,1,2,2) R(7,13,2,2) R(1,7,2,2) R(13,7,2,2)
  - diagonal rays: diamonds centered at (3.7198,3.7198) (12.2802,3.7198) (3.7198,12.2802)
    (12.2802,12.2802), exactly 2 from the straight rays beside them
- bluetooth (solid, crossing at (8,8)):
  - upper arrowhead + stem: (7,1) (9,1) (12.7071,4.7071) (7,10.4142)
  - lower arrowhead + stem: (7,5.5858) (12.7071,11.2929) (9,15) (7,15)
  - upper-left tail: (3.2929,4.7071) (4.7071,3.2929) (8.7071,7.2929) (7.2929,8.7071)
  - lower-left tail: (3.2929,11.2929) (7.2929,7.2929) (8.7071,8.7071) (4.7071,12.7071)
  - bluetooth-on adds diamonds centered at (2.3431,8) and (13.6569,8)
- refresh (two halves, each a 2-thick bend ending in a 6-tall arrowhead; each half's tail is cut at
  45°, 2 below the other's tip):
  - (2,9.1716) (2,3) (11,3) (11,1) (14,4) (11,7) (11,5) (4,5) (4,7.1716)
  - (14,6.8284) (14,13) (5,13) (5,15) (2,12) (5,9) (5,11) (12,11) (12,8.8284)
- Wi-Fi (apex (8,14)):
  - dot: (5.8787,11.8787) (6.7574,11) (9.2426,11) (10.1213,11.8787) (8,14)
  - band 1: (3.0503,9.0503) (5.1005,7) (10.8995,7) (12.9497,9.0503) (11.5355,10.4645) (10.0711,9)
    (5.9289,9) (4.4645,10.4645)
  - band 2: (0.2218,6.2218) (3.4437,3) (12.5563,3) (15.7782,6.2218) (14.3640,7.6360) (11.7279,5)
    (4.2721,5) (1.6360,7.6360)
- volume (apex (5.5858, 8), opening right):
  - speaker: (1,5) (3,5) (6,2) (6,14) (3,11) (1,11)
  - wave 1: (9.4142,4.1716) (11,5.7574) (11,10.2426) (9.4142,11.8284) (8,10.4142) (9,9.4142)
    (9,6.5858) (8,5.5858)
  - wave 2: (12.2426,1.3431) (15,4.1005) (15,11.8995) (12.2426,14.6569) (10.8284,13.2426)
    (13,11.0711) (13,4.9289) (10.8284,2.7574)
  - wave 2, over: (11.7426,0.8431) (15,4.1005) (15,11.8995) (11.7426,15.1569) (10.3284,13.7426)
    (13,11.0711) (13,4.9289) (10.3284,2.2574)
  - muted X: the two lines of rule 31
- bell-off, mic-off, wifi-off: the base icon with rule 29's slash and gap.

Ink coverage (filled area in square units; `cargo test sizes -- --nocapture` prints it) runs from 60
to 132. Solid objects: phone 132, battery-full 128, lock 120, keyboard 116, pause 112, mouse 110,
speaker 104, headphones 104, wired 104. Glyphs and shaped objects: volume 66 to 85, mic 84, mic-off
78, bluetooth 73 (on: 82), play 70.5, refresh 67, bell-off 66, brightness 62 to 64, Wi-Fi 60 to 61.
A new solid object MUST land in 100 to 132, a new glyph or shaped object in 60 to 85.

## 9. SVG output

35. Root exactly: `<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">`.
36. Only `<path>` elements with `fill="currentColor"`. Dim parts are a second path with
    `fill-opacity="0.35"`. Nothing else: no `stroke`, `transform`, `<circle>`, `<rect>` with `rx`,
    arcs (`A`), curves (`C`, `Q`, `S`, `T`), `<mask>`, `<clipPath>`, `<g>` styling, `opacity` on the
    root, ids, classes, metadata or comments.
37. Every stroke converted to its outline (section 6). Overlapping shapes MUST be merged into one
    outline (boolean union) before export; the slash gap MUST be a real boolean subtraction.
38. Holes are inner subpaths with `fill-rule="evenodd"`; after the union no two subpaths overlap.
39. Absolute commands only (`M`, `H`, `V`, `L`, `Z`), coordinates with 4 decimals, integers without
    decimals.

Example (lock):

```svg
<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><path fill="currentColor" fill-rule="evenodd" d="M4 1H12V7H10V3H6V7H4Z M2 7H14V15H2Z M7 9V13H9V9Z"/></svg>
```

## 10. Process (follow in order)

40. Name the single most recognizable silhouette of the object (the way it looks from the front, in
    its most common form). Drop everything that is not needed to recognize it at 16 px.
41. Pick its class (section 5) and block it in with whole-unit rectangles at the class size.
42. Add 45° edges only where the silhouette needs a diagonal (tapers, arrowheads, chamfers), placed so
    every 2 holds.
43. Cut the defining details as holes and slits at least 2 wide, exactly 2 from their nearest edge.
44. Check every rule in section 2. If a detail cannot satisfy them, remove it or fill it solid.
45. Tie-breaks, in this order, when more than one layout passes:
    1. symmetric over asymmetric
    2. every edge on whole units over some on fractions
    3. the larger size within the class
    4. fewer parts and fewer cutouts
    5. the proportions of the closest icon in section 8 (a router should read like `wired`, a
       tablet like `phone` turned or widened, a camera like `speaker` with a lens hole)
46. States: build the base first, then derive levels, off, muted and on from it per section 7.

## 11. Frozen exceptions (reproduce, NEVER add new ones)

- play's triangle edges and the bell's upper sides are not 45° (their shapes need them).
- the brightness core's 45° faces are 0.0531 farther out than a regular octagon's.
- the Bluetooth dots poke 0.0711 past the box (exactly 2 from the crossing puts them there).
- Wi-Fi and volume fans, and the over-100% wave, use the full 16.

## 12. Verification (required for every icon, with numbers)

47. Render at 256 px (16 px per unit) and measure:
    - every part, fragment, hole and slit is at least 32 px wide at its narrowest
    - every part's nearest neighbor, and every hole's nearest neighbor across its wall, is 32 px away
      (±1.5 px)
    - the ink's bounding box matches the class exactly (±1 px), inside 16–240 px on both axes
      unless section 11 allows otherwise
48. Compute every 45° thickness and gap exactly: perpendicular distance between parallel edges is
    2.0000.
49. Place it beside battery-full, wifi-3, lock and bluetooth at 16 px and 32 px. It must not be the
    heaviest or the lightest of its kind, and must not read as a different style.
50. Report, for each icon: its class, bounding box, ink coverage, the smallest width and the smallest
    gap found, and which tie-breaks decided its layout. An icon without this report is not done.

flurry checks the same things on its own icons in `ui/icons.rs`: `exact_widths_and_gaps`,
`no_slivers_or_tight_gaps`, `nearest_gaps_are_exactly_2` and `sizes` (`cargo test icons`;
`cargo test sizes -- --nocapture` prints every icon's bounds and coverage).
